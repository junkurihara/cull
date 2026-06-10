//! Image-directory triage tool.
//!
//! Serves a single-page app that displays generated images one at a time and
//! moves the chosen ones into keep/trash directories via `rename(2)`. See
//! `.tmp/design.md` for the full specification. The server keeps no in-memory
//! list of images (only a bounded undo stack): every `next` is a fresh,
//! sorted-free single pass over the source tree, so work scales with the
//! unprocessed backlog rather than total generations.

use cull::config::{Config, RawConfig};
use cull::server::{router, AppState};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = match load_config() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("cull: configuration error: {msg}");
            std::process::exit(1);
        }
    };

    let bind_addr = cfg.bind_addr;
    tracing::info!(
        source = %cfg.source_dir.display(),
        keep = %cfg.keep_dir.display(),
        trash = %cfg.trash_dir.display(),
        order = ?cfg.order,
        undo_depth = cfg.undo_depth,
        %bind_addr,
        "starting cull"
    );

    let state = Arc::new(AppState::new(cfg));

    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cull: failed to bind {bind_addr}: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = axum::serve(listener, router(state)).await {
        eprintln!("cull: server error: {e}");
        std::process::exit(1);
    }
}

fn load_config() -> Result<Config, String> {
    let raw = RawConfig::from_env().map_err(|e| e.to_string())?;
    raw.resolve().map_err(|e| e.to_string())
}
