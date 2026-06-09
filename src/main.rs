//! Image triage tool for ComfyUI output.
//!
//! Serves a single-page app that displays generated images one at a time and
//! moves the chosen ones into keep/trash directories via `rename(2)`. See
//! `.tmp/design.md` for the full specification. The server keeps no in-memory
//! list of images (only a bounded undo stack): every `next` is a fresh,
//! sorted-free single pass over the source tree, so work scales with the
//! unprocessed backlog rather than total generations.

fn main() {
    // Real wiring (config load, router, listener) is added in later tasks.
    // This skeleton exists so the crate builds from the first commit.
    println!("triage-tool: not yet wired up");
}
