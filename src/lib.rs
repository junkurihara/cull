//! Image triage tool for ComfyUI output (library crate).
//!
//! The binaries (`triage-tool`, `gen_fixtures`) are thin wrappers over these
//! modules. Exposing the logic as a library keeps `pub` items on a real API
//! boundary (no spurious dead-code warnings) and lets integration tests in
//! `tests/` exercise the same code paths. See `.tmp/design.md`.

pub mod config;
pub mod meta;
pub mod moves;
pub mod paths;
pub mod server;
pub mod walk;
