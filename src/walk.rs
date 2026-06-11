//! Source-tree enumeration: the stateless single-pass `next` and `count`
//! (design.md §1, §2, §3).
//!
//! Every call walks the source tree fresh. We never sort and never retain a
//! full image list: `find_next` selects, in a single O(n) pass with O(1)
//! extra space, the minimal (asc) or maximal (desc) relative path on the
//! correct side of `after`. `n` is the unprocessed backlog, which shrinks as
//! images are moved out, so cost scales with work remaining, not with total
//! generations.
//!
//! Relative paths use '/' separators and are compared byte-lexicographically,
//! which equals generation order for the generator's zero-padded counters
//! (design.md §3).

use crate::config::{Config, Order};
use crate::paths::is_under;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{self, ErrorKind};
use std::path::Path;

/// Find the next relative path to show.
///
/// - asc: the smallest relpath strictly greater than `after` (or the global
///   minimum when `after` is `None`).
/// - desc: the largest relpath strictly less than `after` (or the global
///   maximum when `after` is `None`).
///
/// Returns `Ok(None)` when the backlog on that side is empty (queue drained).
pub fn find_next(cfg: &Config, after: Option<&str>) -> io::Result<Option<String>> {
    let mut best: Option<String> = None;
    walk_images(cfg, &mut |rel: &str| {
        consider(rel, after, cfg.order, &mut best)
    })?;
    Ok(best)
}

/// Count the unprocessed backlog (design.md §10). O(n) walk; intended for
/// occasional backlog display, not for frequent polling.
pub fn count_backlog(cfg: &Config) -> io::Result<usize> {
    let mut n = 0usize;
    walk_images(cfg, &mut |_rel: &str| n += 1)?;
    Ok(n)
}

/// Single-pass selection step: update `best` if `rel` is eligible (correct
/// side of `after`) and better (smaller for asc, larger for desc).
fn consider(rel: &str, after: Option<&str>, order: Order, best: &mut Option<String>) {
    let eligible = match (order, after) {
        (_, None) => true,
        (Order::Asc, Some(a)) => rel > a,
        (Order::Desc, Some(a)) => rel < a,
    };
    if !eligible {
        return;
    }
    let better = match best.as_deref() {
        None => true,
        Some(b) => match order {
            Order::Asc => rel < b,
            Order::Desc => rel > b,
        },
    };
    if better {
        *best = Some(rel.to_string());
    }
}

/// True if `name` has one of the configured (lowercase, dot-less) extensions.
/// A leading-dot-only name has already been excluded as hidden upstream.
fn has_allowed_ext(cfg: &Config, name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => cfg.extensions.contains(&ext.to_ascii_lowercase()),
        _ => false,
    }
}

/// List one page of the keep gallery: up to `limit` relative paths under
/// KEEP_DIR that are strictly less than `after` (when given), in descending
/// byte-lexicographic order — the gallery reviews the newest keeps first,
/// independent of the triage ORDER. Single O(n) pass with O(limit) extra
/// space: a min-heap keeps the `limit` largest survivors (design.md addendum).
pub fn list_keep_page(cfg: &Config, after: Option<&str>, limit: usize) -> io::Result<Vec<String>> {
    let mut heap: BinaryHeap<Reverse<String>> = BinaryHeap::with_capacity(limit + 1);
    walk_dir(cfg, &cfg.keep_dir, "", &[], &mut |rel: &str| {
        if let Some(a) = after {
            if rel >= a {
                return;
            }
        }
        if heap.len() < limit {
            heap.push(Reverse(rel.to_string()));
        } else if let Some(Reverse(smallest)) = heap.peek() {
            if rel > smallest.as_str() {
                heap.pop();
                heap.push(Reverse(rel.to_string()));
            }
        }
    })?;
    let mut items: Vec<String> = heap.into_iter().map(|Reverse(s)| s).collect();
    items.sort_unstable_by(|a, b| b.cmp(a));
    Ok(items)
}

/// Walk every eligible image in the source tree, invoking `f` with each
/// relative path. Pruning, hidden-file exclusion and the extension filter are
/// applied here so callers see only candidate images.
fn walk_images(cfg: &Config, f: &mut dyn FnMut(&str)) -> io::Result<()> {
    walk_dir(
        cfg,
        &cfg.source_dir,
        "",
        &[&cfg.keep_dir, &cfg.trash_dir],
        f,
    )
}

/// Recursive walker shared by the source enumeration and the keep gallery:
/// `dir` is the current directory, `rel` its path relative to the walk root,
/// and `prune` the canonical subtrees to skip (empty for the keep walk).
fn walk_dir(
    cfg: &Config,
    dir: &Path,
    rel: &str,
    prune: &[&Path],
    f: &mut dyn FnMut(&str),
) -> io::Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // A directory removed mid-walk (external move/delete, or our own move
        // of the last entry) is not an error: the tree is re-read on every
        // call, so it simply won't appear next time (design.md §2, §7).
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    for entry in read {
        // A transient per-entry stat error during a live tree is skipped
        // rather than aborting the whole enumeration.
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let os_name = entry.file_name();
        let name = match os_name.to_str() {
            Some(n) => n,
            None => continue, // non-UTF8 names are not produced by the generator
        };
        if name.starts_with('.') {
            continue; // hidden files and directories are excluded (design.md §2)
        }
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        // Do not follow symlinks: this keeps every built path canonical (the
        // source root is canonical), so the prune comparison below is a
        // canonical-path match, and it avoids symlink loops / escapes.
        if file_type.is_symlink() {
            continue;
        }

        let abs = dir.join(name);
        let child_rel = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };

        if file_type.is_dir() {
            // Prune by canonical path containment, not by basename: a
            // user-made subfolder also named "keep" elsewhere under output
            // must still be walked (design.md §2).
            if prune.iter().any(|p| is_under(&abs, p)) {
                continue;
            }
            walk_dir(cfg, &abs, &child_rel, prune, f)?;
        } else if file_type.is_file() && has_allowed_ext(cfg, name) {
            f(&child_rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RawConfig;
    use std::fs;

    /// Construct a resolved Config plus a populated source tree:
    ///
    /// ```text
    /// output/
    ///   2026-06-01/Image_00001_.png
    ///   Image_00001_.png
    ///   Image_00002_.png
    ///   UPPER.PNG                  (uppercase extension -> still matches)
    ///   proj-a/Image_00009_.png
    ///   proj-a/keep/inside.png     (confusing "keep" -> NOT pruned, path != KEEP_DIR)
    ///   .hidden.png                (hidden -> excluded)
    ///   notes.txt                  (non-image -> excluded)
    ///   keep/kept.png              (KEEP_DIR -> pruned)
    ///   trash/t.png                (TRASH_DIR -> pruned)
    /// ```
    fn setup() -> (Config, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("output");
        fs::create_dir_all(&source).unwrap();
        let src = source.to_str().unwrap().to_string();
        let cfg = RawConfig::load(move |k: &str| match k {
            "SOURCE_DIR" => Some(src.clone()),
            _ => None,
        })
        .unwrap()
        .resolve()
        .unwrap();

        let write = |rel: &str| {
            let p = source.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"x").unwrap();
        };
        write("2026-06-01/Image_00001_.png");
        write("Image_00001_.png");
        write("Image_00002_.png");
        write("UPPER.PNG");
        write("proj-a/Image_00009_.png");
        write("proj-a/keep/inside.png");
        write(".hidden.png");
        write("notes.txt");
        // KEEP_DIR / TRASH_DIR contents (must be pruned).
        write("keep/kept.png");
        write("trash/t.png");

        (cfg, tmp)
    }

    /// Drive `find_next` from the start to exhaustion, collecting the sequence.
    fn drain(cfg: &Config) -> Vec<String> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        while let Some(next) = find_next(cfg, after.as_deref()).unwrap() {
            out.push(next.clone());
            after = Some(next);
        }
        out
    }

    fn expected_order() -> Vec<String> {
        // Byte-lexicographic over relpaths: '2' < 'C' < 'U' < 'p'.
        [
            "2026-06-01/Image_00001_.png",
            "Image_00001_.png",
            "Image_00002_.png",
            "UPPER.PNG",
            "proj-a/Image_00009_.png",
            "proj-a/keep/inside.png",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn asc_enumerates_in_lexicographic_order() {
        let (cfg, _tmp) = setup();
        assert_eq!(drain(&cfg), expected_order());
    }

    #[test]
    fn desc_enumerates_in_reverse() {
        let (mut cfg, _tmp) = setup();
        cfg.order = Order::Desc;
        let mut expected = expected_order();
        expected.reverse();
        assert_eq!(drain(&cfg), expected);
    }

    #[test]
    fn first_call_returns_global_min_and_max() {
        let (mut cfg, _tmp) = setup();
        assert_eq!(
            find_next(&cfg, None).unwrap().as_deref(),
            Some("2026-06-01/Image_00001_.png")
        );
        cfg.order = Order::Desc;
        assert_eq!(
            find_next(&cfg, None).unwrap().as_deref(),
            Some("proj-a/keep/inside.png")
        );
    }

    #[test]
    fn keep_page_lists_descending_with_after_chaining() {
        let (cfg, _tmp) = setup();
        // Populate KEEP_DIR beyond the single fixture file, including a
        // subfolder, a hidden file and a non-image (both excluded).
        let write = |rel: &str| {
            let p = cfg.keep_dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"x").unwrap();
        };
        write("a.png");
        write("sub/b.png");
        write(".hidden.png");
        write("notes.txt");
        // setup() already wrote keep/kept.png -> relpath "kept.png".

        // Full listing, newest (largest relpath) first.
        let all = list_keep_page(&cfg, None, 100).unwrap();
        assert_eq!(all, vec!["sub/b.png", "kept.png", "a.png"]);

        // Bounded pages chain via `after` and drain cleanly.
        let p1 = list_keep_page(&cfg, None, 2).unwrap();
        assert_eq!(p1, vec!["sub/b.png", "kept.png"]);
        let p2 = list_keep_page(&cfg, Some("kept.png"), 2).unwrap();
        assert_eq!(p2, vec!["a.png"]);
        let p3 = list_keep_page(&cfg, Some("a.png"), 2).unwrap();
        assert!(p3.is_empty());

        // A zero limit returns nothing rather than panicking.
        assert!(list_keep_page(&cfg, None, 0).unwrap().is_empty());
    }

    #[test]
    fn keep_and_trash_are_pruned() {
        let (cfg, _tmp) = setup();
        let all = drain(&cfg);
        assert!(!all.iter().any(|r| r.starts_with("keep/")));
        assert!(!all.iter().any(|r| r.starts_with("trash/")));
    }

    #[test]
    fn confusing_keep_subfolder_is_not_pruned() {
        // Prune is by path, not basename: proj-a/keep is a distinct path from
        // KEEP_DIR (output/keep), so its contents must be enumerated.
        let (cfg, _tmp) = setup();
        let all = drain(&cfg);
        assert!(all.iter().any(|r| r == "proj-a/keep/inside.png"));
    }

    #[test]
    fn hidden_and_nonimage_excluded() {
        let (cfg, _tmp) = setup();
        let all = drain(&cfg);
        assert!(!all.iter().any(|r| r == ".hidden.png"));
        assert!(!all.iter().any(|r| r == "notes.txt"));
    }

    #[test]
    fn uppercase_extension_matches() {
        let (cfg, _tmp) = setup();
        assert!(drain(&cfg).iter().any(|r| r == "UPPER.PNG"));
    }

    #[test]
    fn count_matches_enumeration() {
        let (cfg, _tmp) = setup();
        assert_eq!(count_backlog(&cfg).unwrap(), expected_order().len());
    }

    #[test]
    fn empty_source_yields_none_and_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("output");
        fs::create_dir_all(&source).unwrap();
        let src = source.to_str().unwrap().to_string();
        let cfg = RawConfig::load(move |k: &str| match k {
            "SOURCE_DIR" => Some(src.clone()),
            _ => None,
        })
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(find_next(&cfg, None).unwrap(), None);
        assert_eq!(count_backlog(&cfg).unwrap(), 0);
    }
}
