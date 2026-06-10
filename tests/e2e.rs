//! End-to-end integration over the real synthetic fixture tree (design.md).
//!
//! Builds the spec-mandated fixtures into a tempdir and exercises the actual
//! library paths: enumeration order, path-based prune of keep/trash, metadata
//! extraction and its fallbacks, and a move + undo round trip preserving the
//! relative subpath.

use std::path::PathBuf;
use triage_tool::config::{Config, Order, RawConfig};
use triage_tool::fixtures;
use triage_tool::moves::{self, UndoStack};
use triage_tool::{meta, walk};

fn fixture_config() -> (Config, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("output");
    std::fs::create_dir_all(&source).unwrap();
    fixtures::build(&source).unwrap();

    let src = source.to_str().unwrap().to_string();
    let cfg = RawConfig::load(move |k: &str| match k {
        "SOURCE_DIR" => Some(src.clone()),
        _ => None,
    })
    .unwrap()
    .resolve()
    .unwrap();
    (cfg, tmp)
}

fn drain(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    while let Some(rel) = walk::find_next(cfg, after.as_deref()).unwrap() {
        out.push(rel.clone());
        after = Some(rel);
    }
    out
}

#[test]
fn enumeration_order_and_prune() {
    let (cfg, _tmp) = fixture_config();
    let all = drain(&cfg);

    // Exactly the seven enumerable images, in byte-lexicographic order.
    assert_eq!(
        all,
        vec![
            "2026-06-01/Image_00001_.png",
            "2026-06-01/Image_00002_.png",
            "Image_00001_.png",
            "Image_00002_.png",
            "Image_00003_.png",
            "proj-a/Image_00009_.png",
            "proj-a/keep/inside.png",
        ]
    );

    // (e) confusing "keep" subfolder is enumerated; real KEEP/TRASH are pruned.
    assert!(all.iter().any(|r| r == "proj-a/keep/inside.png"));
    assert!(!all.iter().any(|r| r.starts_with("keep/")));
    assert!(!all.iter().any(|r| r.starts_with("trash/")));

    assert_eq!(walk::count_backlog(&cfg).unwrap(), 7);
}

#[test]
fn desc_is_reverse_of_asc() {
    let (mut cfg, _tmp) = fixture_config();
    let asc = drain(&cfg);
    cfg.order = Order::Desc;
    let mut desc = drain(&cfg);
    desc.reverse();
    assert_eq!(asc, desc);
}

#[test]
fn meta_extraction_variants() {
    let (cfg, _tmp) = fixture_config();

    // (d) graph present -> positive/negative extracted.
    let m = meta::extract_meta(&cfg.source_dir.join("Image_00001_.png"));
    assert!(m.raw.is_some());
    assert_eq!(m.prompts.len(), 1);
    assert!(m.prompts[0]
        .positive
        .as_deref()
        .unwrap()
        .contains("serene mountain lake"));
    assert!(m.prompts[0].negative.as_deref().unwrap().contains("blurry"));

    // plain PNG -> no metadata at all.
    let m = meta::extract_meta(&cfg.source_dir.join("Image_00002_.png"));
    assert!(m.raw.is_none());
    assert!(m.prompts.is_empty());

    // (d) broken JSON -> raw text shown, no prompts (graceful fallback).
    let m = meta::extract_meta(&cfg.source_dir.join("Image_00003_.png"));
    assert_eq!(m.raw.as_deref(), Some("{ this is not valid json"));
    assert!(m.prompts.is_empty());
}

#[test]
fn move_preserves_subpath_and_undo_round_trips() {
    let (cfg, _tmp) = fixture_config();
    let rel = "2026-06-01/Image_00001_.png";
    let source_abs = cfg.source_dir.join(rel);
    assert!(source_abs.is_file());

    let mut stack = UndoStack::new(cfg.undo_depth);

    // keep: relative subpath preserved under KEEP_DIR.
    let entry = moves::perform_move(&cfg.keep_dir, &source_abs, rel).unwrap();
    assert_eq!(entry.dst, cfg.keep_dir.join(rel));
    assert!(entry.dst.is_file());
    assert!(!source_abs.exists());
    stack.push(entry);

    // It is now pruned from enumeration (lives under KEEP_DIR).
    assert!(!drain(&cfg).iter().any(|r| r == rel));

    // undo restores it to the original relative path; it re-enters the queue.
    let restored = moves::undo(&cfg.source_dir, &mut stack).unwrap();
    assert_eq!(restored, rel);
    assert!(source_abs.is_file());
    assert!(drain(&cfg).iter().any(|r| r == rel));
}

#[test]
fn gen_fixtures_builder_is_idempotent() {
    // Building twice into the same dir must not error (overwrites in place).
    let tmp = tempfile::tempdir().unwrap();
    let source: PathBuf = tmp.path().join("output");
    fixtures::build(&source).unwrap();
    fixtures::build(&source).unwrap();
    assert!(source.join("Image_00001_.png").is_file());
}
