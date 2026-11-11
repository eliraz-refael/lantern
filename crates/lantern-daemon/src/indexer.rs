//! The indexing loop: keep lit folders scanned and run the agent while files are pending.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use lantern_core::{AgentState, Index};

use crate::agent::{self, Run};
use crate::status::Status;

/// How often to check for newly lit folders.
const POLL: Duration = Duration::from_secs(2);
/// How often lit folders are rescanned for new and changed files.
const RESCAN: Duration = Duration::from_secs(10 * 60);
/// How long to wait before starting the agent again after it failed.
const RETRY: Duration = Duration::from_secs(60);

pub struct Indexer {
    pub spec: &'static agent::Spec,
    pub model: Option<String>,
    pub db: PathBuf,
}

impl Indexer {
    pub async fn run(self) -> anyhow::Result<()> {
        let open =
            || Index::open(&self.db).with_context(|| format!("opening {}", self.db.display()));
        let index = Arc::new(Mutex::new(open()?));
        let status = Status::start(self.spec.name, open()?);
        let workspace = self
            .db
            .parent()
            .context("the index has no parent folder")?
            .join("agent-workspace");
        std::fs::create_dir_all(&workspace)?;

        let mut scanned: Option<(Vec<PathBuf>, Instant)> = None;
        loop {
            let roots = index.lock().unwrap().lit_folders()?;
            let stale = scanned
                .as_ref()
                .is_none_or(|(seen, at)| *seen != roots || at.elapsed() >= RESCAN);
            if stale {
                status.set(AgentState::Working, Some("scanning lit folders"));
                let scan = index.clone();
                let report =
                    tokio::task::spawn_blocking(move || scan.lock().unwrap().scan()).await??;
                tracing::info!(?report, folders = roots.len(), "scanned");
                scanned = Some((roots, Instant::now()));
            }

            let pending = index.lock().unwrap().counts(None)?.pending;
            if pending > 0 {
                let run = Run {
                    spec: self.spec,
                    model: self.model.as_deref(),
                    db: &self.db,
                    workspace: &workspace,
                };
                if let Err(e) = agent::run(run, index.clone(), &status).await {
                    tracing::warn!("{} failed: {e:#}", self.spec.name);
                    status.set(AgentState::Error, Some(&agent::explain(self.spec, &e)));
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
            }
            status.set(AgentState::Idle, None);
            tokio::time::sleep(POLL).await;
        }
    }
}
