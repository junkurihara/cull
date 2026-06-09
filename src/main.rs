//! Image triage tool for ComfyUI output.
//!
//! Serves a single-page app that displays generated images one at a time and
//! moves the chosen ones into keep/trash directories via `rename(2)`. See
//! `.tmp/design.md` for the full specification. The server keeps no in-memory
//! list of images (only a bounded undo stack): every `next` is a fresh,
//! sorted-free single pass over the source tree, so work scales with the
//! unprocessed backlog rather than total generations.

mod config;

use config::{Config, RawConfig};

fn main() {
    let cfg = match load_config() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("triage-tool: configuration error: {msg}");
            std::process::exit(1);
        }
    };

    // HTTP wiring (router, listener) is added in later tasks. For now report
    // the resolved configuration so the startup path is exercised end to end.
    println!(
        "triage-tool: source={} keep={} trash={} order={:?} exts={:?} undo_depth={} bind={}",
        cfg.source_dir.display(),
        cfg.keep_dir.display(),
        cfg.trash_dir.display(),
        cfg.order,
        cfg.extensions,
        cfg.undo_depth,
        cfg.bind_addr,
    );
}

fn load_config() -> Result<Config, String> {
    let raw = RawConfig::from_env().map_err(|e| e.to_string())?;
    raw.resolve().map_err(|e| e.to_string())
}
