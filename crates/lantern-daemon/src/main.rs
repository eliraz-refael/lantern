//! Lantern daemon: owns the index and serves it to the TUI and web client on localhost.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use lantern_core::Index;

const ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 7777);

type Shared = Arc<Mutex<Index>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let db = Index::default_path().context("could not determine a home directory")?;
    let index = Index::open(&db).with_context(|| format!("opening {}", db.display()))?;
    tracing::info!(db = %db.display(), "opened index");

    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/folders", get(lit_folders))
        .with_state(Arc::new(Mutex::new(index)));

    let addr = SocketAddr::from(ADDR);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
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
