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
    walk_images(cfg, &mut |rel: &str, _abs: &Path| {
        consider(rel, after, cfg.order, &mut best)
    })?;
    Ok(best)
}

/// Count the unprocessed backlog (design.md §10). O(n) walk; intended for
/// occasional backlog display, not for frequent polling.
pub fn count_backlog(cfg: &Config) -> io::Result<usize> {
    let mut n = 0usize;
    walk_images(cfg, &mut |_rel: &str, _abs: &Path| n += 1)?;
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

/// Sort key for the keep gallery listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepSort {
    /// Byte-lexicographic relpath order.
    Name,
    /// File modification time. rename(2) preserves mtime, so for kept files
    /// this is effectively generation time, independent of when they were
    /// triaged or which folder they sit in.
    Mtime,
}

/// One keep gallery entry: the relpath plus its mtime in unix milliseconds
/// (0 when unavailable). Milliseconds keep the value inside JavaScript's safe
/// integer range, unlike nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepItem {
    pub rel: String,
    pub mtime_ms: u64,
}

/// Bounded top-N selector over (mtime, rel) keys: a min-heap keeps the N
/// largest survivors for descending output, a max-heap the N smallest for
/// ascending. O(limit) space regardless of tree size.
enum PageHeap {
    Desc(BinaryHeap<Reverse<(u64, String)>>),
    Asc(BinaryHeap<(u64, String)>),
}

impl PageHeap {
    fn new(desc: bool) -> Self {
        if desc {
            PageHeap::Desc(BinaryHeap::new())
        } else {
            PageHeap::Asc(BinaryHeap::new())
        }
    }

    fn len(&self) -> usize {
        match self {
            PageHeap::Desc(h) => h.len(),
            PageHeap::Asc(h) => h.len(),
        }
    }

    /// The key that would be evicted next (the current worst survivor).
    fn worst(&self) -> Option<(u64, &str)> {
        match self {
            PageHeap::Desc(h) => h.peek().map(|Reverse((m, r))| (*m, r.as_str())),
            PageHeap::Asc(h) => h.peek().map(|(m, r)| (*m, r.as_str())),
        }
    }

    fn push(&mut self, key: (u64, String)) {
        match self {
            PageHeap::Desc(h) => h.push(Reverse(key)),
            PageHeap::Asc(h) => h.push(key),
        }
    }

    fn pop(&mut self) {
        match self {
            PageHeap::Desc(h) => {
                h.pop();
            }
            PageHeap::Asc(h) => {
                h.pop();
            }
        }
    }

    fn into_sorted(self) -> Vec<(u64, String)> {
        match self {
            PageHeap::Desc(h) => {
                let mut v: Vec<_> = h.into_iter().map(|Reverse(k)| k).collect();
                v.sort_unstable_by(|a, b| b.cmp(a));
                v
            }
            PageHeap::Asc(h) => {
                let mut v: Vec<_> = h.into_vec();
                v.sort_unstable();
                v
            }
        }
    }
}

/// File mtime as unix milliseconds; `None` for transient stat failures (e.g.
/// the file was moved mid-walk), which simply skips the entry this pass.
fn mtime_ms(abs: &Path) -> Option<u64> {
    let t = std::fs::metadata(abs).ok()?.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(d.as_millis() as u64)
}

/// List one page of the keep gallery: up to `limit` entries under KEEP_DIR on
/// the requested side of the `after` cursor, ordered by `sort`/`desc`.
///
/// The cursor is the compound key `(mtime_ms, rel)` of the last item of the
/// previous page (relpath breaks mtime ties, keeping pagination stable); name
/// sort uses a constant 0 time component so the tuple order degenerates to
/// relpath order. Single O(n) pass with O(limit) extra space; mtime sort
/// additionally stats every walked file (the sort key), name sort stats only
/// the selected page (for display).
pub fn list_keep_page(
    cfg: &Config,
    sort: KeepSort,
    desc: bool,
    after: Option<(u64, &str)>,
    limit: usize,
) -> io::Result<Vec<KeepItem>> {
    let mut heap = PageHeap::new(desc);
    walk_dir(cfg, &cfg.keep_dir, "", &[], &mut |rel: &str, abs: &Path| {
        let time = match sort {
            KeepSort::Name => 0,
            KeepSort::Mtime => match mtime_ms(abs) {
                Some(m) => m,
                None => return,
            },
        };
        let key = (time, rel);
        let eligible = match after {
            None => true,
            Some(a) => {
                if desc {
                    key < a
                } else {
                    key > a
                }
            }
        };
        if !eligible {
            return;
        }
        if heap.len() < limit {
            heap.push((time, rel.to_string()));
        } else {
            // Full: only a key better than the current worst survivor earns a
            // slot. `worst()` is None only when limit == 0.
            let better = match heap.worst() {
                Some(w) => {
                    if desc {
                        key > w
                    } else {
                        key < w
                    }
                }
                None => false,
            };
            if better {
                heap.pop();
                heap.push((time, rel.to_string()));
            }
        }
    })?;
    Ok(heap
        .into_sorted()
        .into_iter()
        .map(|(time, rel)| {
            // Name sort skipped the per-file stat during the walk; fill the
            // display mtime for just the selected page here.
            let mtime = match sort {
                KeepSort::Mtime => time,
                KeepSort::Name => mtime_ms(&cfg.keep_dir.join(&rel)).unwrap_or(0),
            };
            KeepItem {
                rel,
                mtime_ms: mtime,
            }
        })
        .collect())
}

/// Minimal SplitMix64 PRNG. A few lines of arithmetic let the random
/// slideshow sample reproducibly from a seed without pulling in a `rand`
/// dependency. Not cryptographic, which is fine for picking pictures.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`. The modulo bias is negligible for the small
    /// bounds (reservoir indices) this sees against a 64-bit draw.
    fn next_below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

/// Reservoir-sample up to `n` kept images uniformly at random in a single
/// pass, seeded by `seed` for reproducibility. O(n_total) time and O(n) space:
/// the keep set is never materialized as a list, in keeping with the
/// stateless-walk principle (design.md §1). The selected entries are then
/// stat'd for their display mtime (as the name-sorted listing does).
///
/// The returned order is the reservoir's internal order (already a uniform
/// random subset); the slideshow does not depend on it being shuffled further.
pub fn sample_keep(cfg: &Config, n: usize, seed: u64) -> io::Result<Vec<KeepItem>> {
    let mut rng = SplitMix64::new(seed);
    let mut reservoir: Vec<String> = Vec::with_capacity(n);
    // Index of the current item (0-based), i.e. how many have been seen so far.
    let mut seen: u64 = 0;
    walk_dir(
        cfg,
        &cfg.keep_dir,
        "",
        &[],
        &mut |rel: &str, _abs: &Path| {
            if n == 0 {
                return;
            }
            if reservoir.len() < n {
                reservoir.push(rel.to_string());
            } else {
                // Classic algorithm R: replace a random slot with probability n/seen.
                let j = rng.next_below(seen + 1);
                if (j as usize) < n {
                    reservoir[j as usize] = rel.to_string();
                }
            }
            seen += 1;
        },
    )?;
    Ok(reservoir
        .into_iter()
        .map(|rel| {
            let mtime = mtime_ms(&cfg.keep_dir.join(&rel)).unwrap_or(0);
            KeepItem {
                rel,
                mtime_ms: mtime,
            }
        })
        .collect())
}

/// Walk every eligible image in the source tree, invoking `f` with each
/// relative and absolute path. Pruning, hidden-file exclusion and the
/// extension filter are applied here so callers see only candidate images.
fn walk_images(cfg: &Config, f: &mut dyn FnMut(&str, &Path)) -> io::Result<()> {
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
    f: &mut dyn FnMut(&str, &Path),
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
            f(&child_rel, &abs);
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

    /// Write a file under KEEP_DIR; hidden/non-image names exercise filters.
    fn write_keep(cfg: &Config, rel: &str) {
        let p = cfg.keep_dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    /// Pin a kept file's mtime to a deterministic unix-seconds value.
    fn set_keep_mtime(cfg: &Config, rel: &str, secs: u64) {
        let p = cfg.keep_dir.join(rel);
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn rels(items: &[KeepItem]) -> Vec<&str> {
        items.iter().map(|i| i.rel.as_str()).collect()
    }

    #[test]
    fn keep_page_lists_by_name_with_after_chaining() {
        let (cfg, _tmp) = setup();
        write_keep(&cfg, "a.png");
        write_keep(&cfg, "sub/b.png");
        write_keep(&cfg, ".hidden.png");
        write_keep(&cfg, "notes.txt");
        // setup() already wrote keep/kept.png -> relpath "kept.png".

        // Full listing, descending relpath.
        let all = list_keep_page(&cfg, KeepSort::Name, true, None, 100).unwrap();
        assert_eq!(rels(&all), vec!["sub/b.png", "kept.png", "a.png"]);

        // Bounded pages chain via the (0, rel) cursor and drain cleanly.
        let p1 = list_keep_page(&cfg, KeepSort::Name, true, None, 2).unwrap();
        assert_eq!(rels(&p1), vec!["sub/b.png", "kept.png"]);
        let p2 = list_keep_page(&cfg, KeepSort::Name, true, Some((0, "kept.png")), 2).unwrap();
        assert_eq!(rels(&p2), vec!["a.png"]);
        let p3 = list_keep_page(&cfg, KeepSort::Name, true, Some((0, "a.png")), 2).unwrap();
        assert!(p3.is_empty());

        // Ascending flips the order; zero limit returns nothing.
        let asc = list_keep_page(&cfg, KeepSort::Name, false, None, 100).unwrap();
        assert_eq!(rels(&asc), vec!["a.png", "kept.png", "sub/b.png"]);
        assert!(list_keep_page(&cfg, KeepSort::Name, true, None, 0)
            .unwrap()
            .is_empty());

        // Name sort still reports the display mtime of the selected page.
        set_keep_mtime(&cfg, "a.png", 1_000);
        let one = list_keep_page(&cfg, KeepSort::Name, false, None, 1).unwrap();
        assert_eq!(one[0].mtime_ms, 1_000_000);
    }

    #[test]
    fn keep_page_sorts_by_mtime_with_cursor_and_tiebreak() {
        let (cfg, _tmp) = setup();
        write_keep(&cfg, "a.png");
        write_keep(&cfg, "sub/b.png");
        set_keep_mtime(&cfg, "a.png", 3_000);
        set_keep_mtime(&cfg, "kept.png", 2_000);
        set_keep_mtime(&cfg, "sub/b.png", 1_000);

        // Newest first, independent of relpath order.
        let all = list_keep_page(&cfg, KeepSort::Mtime, true, None, 100).unwrap();
        assert_eq!(rels(&all), vec!["a.png", "kept.png", "sub/b.png"]);
        assert_eq!(all[0].mtime_ms, 3_000_000);

        // The compound cursor (mtime_ms, rel) continues mid-list.
        let p1 = list_keep_page(&cfg, KeepSort::Mtime, true, None, 2).unwrap();
        assert_eq!(rels(&p1), vec!["a.png", "kept.png"]);
        let cursor = (p1[1].mtime_ms, p1[1].rel.as_str());
        let p2 = list_keep_page(&cfg, KeepSort::Mtime, true, Some(cursor), 2).unwrap();
        assert_eq!(rels(&p2), vec!["sub/b.png"]);

        // Oldest first.
        let asc = list_keep_page(&cfg, KeepSort::Mtime, false, None, 100).unwrap();
        assert_eq!(rels(&asc), vec!["sub/b.png", "kept.png", "a.png"]);

        // Equal mtimes fall back to relpath order and still paginate stably.
        set_keep_mtime(&cfg, "a.png", 2_000);
        set_keep_mtime(&cfg, "sub/b.png", 2_000);
        let tied = list_keep_page(&cfg, KeepSort::Mtime, true, None, 100).unwrap();
        assert_eq!(rels(&tied), vec!["sub/b.png", "kept.png", "a.png"]);
        let t1 = list_keep_page(&cfg, KeepSort::Mtime, true, None, 1).unwrap();
        let cursor = (t1[0].mtime_ms, t1[0].rel.as_str());
        let t2 = list_keep_page(&cfg, KeepSort::Mtime, true, Some(cursor), 2).unwrap();
        assert_eq!(rels(&t2), vec!["kept.png", "a.png"]);
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

    /// Every rel under KEEP_DIR (sorted), the universe a sample draws from.
    fn keep_universe(cfg: &Config) -> Vec<String> {
        let mut all = list_keep_page(cfg, KeepSort::Name, false, None, 1000).unwrap();
        all.sort_by(|a, b| a.rel.cmp(&b.rel));
        all.into_iter().map(|i| i.rel).collect()
    }

    #[test]
    fn sample_keep_is_deterministic_and_a_valid_subset() {
        let (cfg, _tmp) = setup();
        // setup() wrote keep/kept.png; add a handful more for a real sample.
        for name in ["a.png", "b.png", "sub/c.png", "sub/d.png", "e.png"] {
            write_keep(&cfg, name);
        }
        let universe = keep_universe(&cfg); // 6 entries total

        let first = sample_keep(&cfg, 3, 42).unwrap();
        // Exactly n entries, all distinct, all drawn from the keep set.
        assert_eq!(first.len(), 3);
        let mut rels: Vec<&str> = first.iter().map(|i| i.rel.as_str()).collect();
        rels.sort_unstable();
        rels.dedup();
        assert_eq!(rels.len(), 3, "sample must not repeat an image");
        assert!(rels.iter().all(|r| universe.iter().any(|u| u == r)));

        // Same seed -> identical selection (reproducible). The mtime is filled
        // from a stat of each chosen file.
        let again = sample_keep(&cfg, 3, 42).unwrap();
        let names = |v: &[KeepItem]| v.iter().map(|i| i.rel.clone()).collect::<Vec<_>>();
        assert_eq!(names(&first), names(&again));
    }

    #[test]
    fn sample_keep_handles_boundaries() {
        let (cfg, _tmp) = setup();
        for name in ["a.png", "b.png"] {
            write_keep(&cfg, name);
        }
        let universe = keep_universe(&cfg); // kept.png + a.png + b.png = 3

        // n == 0 yields nothing.
        assert!(sample_keep(&cfg, 0, 1).unwrap().is_empty());

        // n >= total returns the whole set (order-independent).
        let mut all: Vec<String> = sample_keep(&cfg, 10, 7)
            .unwrap()
            .into_iter()
            .map(|i| i.rel)
            .collect();
        all.sort();
        assert_eq!(all, universe);
    }

    #[test]
    fn sample_keep_empty_keep_is_empty() {
        let (cfg, _tmp) = setup();
        // setup() seeds keep/kept.png; remove it so KEEP_DIR holds no images.
        fs::remove_file(cfg.keep_dir.join("kept.png")).unwrap();
        assert!(sample_keep(&cfg, 5, 99).unwrap().is_empty());
    }
}
