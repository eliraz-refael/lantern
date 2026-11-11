//! What the daemon is doing, written to the index for the TUI. A heartbeat keeps it fresh, so a
//! stale status means the daemon has stopped.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lantern_core::{AgentState, Index};

const HEARTBEAT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct Status {
    agent: &'static str,
    index: Arc<Mutex<Index>>,
    current: Arc<Mutex<(AgentState, Option<String>)>>,
}

impl Status {
    /// Starts the heartbeat. `index` is a connection of its own, so the heartbeat never waits
    /// on a scan.
    pub fn start(agent: &'static str, index: Index) -> Self {
        let status = Self {
            agent,
            index: Arc::new(Mutex::new(index)),
            current: Arc::new(Mutex::new((AgentState::Idle, None))),
        };
        let beat = status.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT);
            loop {
                interval.tick().await;
                beat.write();
            }
        });
        status
    }

    pub fn set(&self, state: AgentState, detail: Option<&str>) {
        let changed = {
            let mut current = self.current.lock().unwrap();
            let next = (state, detail.map(str::to_owned));
            let changed = *current != next;
            *current = next;
            changed
        };
        if changed {
            self.write();
        }
    }

    fn write(&self) {
        let (state, detail) = self.current.lock().unwrap().clone();
        let result =
            self.index
                .lock()
                .unwrap()
                .set_agent_status(self.agent, state, detail.as_deref());
        if let Err(e) = result {
            tracing::warn!("could not write the agent status: {e}");
        }
    }
}
