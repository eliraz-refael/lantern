//! Lantern daemon: owns the index, runs the agent that fills it, and serves it on localhost.
//!
//! `lantern-daemon [--agent claude|gemini] [--model NAME]` runs the daemon.
//! `lantern-daemon mcp --db PATH --agent NAME` is the MCP server the agent starts for itself.

mod agent;
mod indexer;
mod mcp;
mod status;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use lantern_core::Index;
use tracing_subscriber::EnvFilter;

use crate::indexer::Indexer;

const ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 7777);

const USAGE: &str = "usage: lantern-daemon [--agent claude|gemini] [--model NAME] [--db PATH]

  --agent  the ACP agent that describes files (default: claude)
  --model  the model Claude uses, such as haiku or sonnet (default: Claude's own)
  --db     the index to use (default: $LANTERN_DB, else the shared index)";

type Shared = Arc<Mutex<Index>>;

#[derive(Default)]
struct Args {
    mcp: bool,
    agent: Option<String>,
    model: Option<String>,
    db: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "mcp" => args.mcp = true,
            "--agent" => args.agent = Some(value()?),
            "--model" => args.model = Some(value()?),
            "--db" => args.db = Some(value()?.into()),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => bail!("unknown argument: {arg}\n\n{USAGE}"),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let db = match args.db {
        Some(db) => db,
        None => Index::default_path().context("could not determine a home directory")?,
    };
    let agent_name = args.agent.as_deref().unwrap_or("claude");
    let spec = agent::find(agent_name).with_context(|| {
        let known: Vec<_> = agent::AGENTS.iter().map(|a| a.name).collect();
        format!(
            "unknown agent {agent_name}; try one of: {}",
            known.join(", ")
        )
    })?;

    // Stdout belongs to the MCP protocol there, so logs go to stderr.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    if args.mcp {
        return mcp::serve(&db, spec.name.to_owned()).await;
    }

    let index = Index::open(&db).with_context(|| format!("opening {}", db.display()))?;
    tracing::info!(db = %db.display(), agent = spec.name, "opened index");

    // Bind first, so a second daemon stops here instead of indexing alongside the first.
    let addr = SocketAddr::from(ADDR);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}; is another lantern-daemon running?"))?;
    tracing::info!(%addr, "listening");
    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/folders", get(lit_folders))
        .with_state(Arc::new(Mutex::new(index)));
    let serving = axum::serve(listener, app).with_graceful_shutdown(async {
        tokio::signal::ctrl_c().await.ok();
    });

    let indexer = Indexer {
        spec,
        model: args.model,
        db,
    };
    let indexing = tokio::spawn(indexer.run());

    tokio::select! {
        served = serving => served?,
        indexed = indexing => indexed??,
    }
    Ok(())
}

async fn lit_folders(
    State(index): State<Shared>,
) -> Result<Json<Vec<PathBuf>>, (StatusCode, String)> {
    let folders = index
        .lock()
        .unwrap()
        .lit_folders()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(folders))
}
