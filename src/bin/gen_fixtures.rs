//! Synthetic fixture generator (design.md sandbox constraints).
//!
//! Writes a synthetic image-output tree into `<target>/output` for manual inspection and
//! as material for the meta-extraction tests. The reusable layout lives in
//! `cull::fixtures`; this binary just chooses where to write.
//!
//! Usage: `cargo run --bin gen_fixtures [TARGET_DIR]`
//! TARGET_DIR defaults to `.tmp/fixtures`. The `output` subdirectory under it
//! is what you would point `SOURCE_DIR` at.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let target = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".tmp/fixtures"));
    let source = target.join("output");

    if let Err(e) = cull::fixtures::build(&source) {
        eprintln!(
            "gen_fixtures: failed to build fixtures in {}: {e}",
            source.display()
        );
        return ExitCode::FAILURE;
    }

    println!(
        "gen_fixtures: wrote fixture tree under {}",
        source.display()
    );
    println!("  point SOURCE_DIR at: {}", source.display());
    ExitCode::SUCCESS
}
