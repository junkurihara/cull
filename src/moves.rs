//! Move (keep/trash) and undo (design.md §6, §9).
//!
//! A move is `rename(2)` within a single filesystem; it preserves the relative
//! subpath (`SOURCE_DIR/rel` -> `DST_BASE/rel`) so provenance survives and
//! cross-subfolder name clashes are reduced. We never re-encode and never fall
//! back to a copy: a cross-device rename (`EXDEV`) is a deployment
//! misconfiguration and is surfaced as an error, not silently worked around.
//!
//! Collisions append `_N` to the final path element. The undo stack records
//! the *actual* destination path (after collision resolution) plus the
//! original relative path, so undo restores the real file rather than a
//! recomputed guess. The stack is in-memory and bounded: it is a "take back
//! the last action" affordance, not a journal — keep/trash destinations are
//! reclaimed/cleaned externally, so old destinations may no longer exist.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Linux errno for a cross-device link (rename across filesystems).
const EXDEV: i32 = 18;

/// A recorded move, enough to undo it: the real destination the file now lives
/// at, and the relative path it originally occupied under SOURCE_DIR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    pub dst: PathBuf,
    pub original_rel: String,
}

#[derive(Debug)]
pub enum MoveError {
    /// rename(2) failed with EXDEV: keep/trash is on a different filesystem
    /// than source. This violates the single-FS deployment requirement (§9).
    CrossDevice {
        from: PathBuf,
        to: PathBuf,
    },
    Io(io::Error),
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveError::CrossDevice { from, to } => write!(
                f,
                "cross-device rename from {} to {}: keep/trash must be on the same filesystem as source (EXDEV)",
                from.display(),
                to.display()
            ),
            MoveError::Io(e) => write!(f, "move failed: {e}"),
        }
    }
}

impl std::error::Error for MoveError {}

#[derive(Debug)]
pub enum UndoError {
    /// The undo stack is empty.
    Empty,
    /// The recorded destination no longer exists (reclaimed/cleaned away).
    DstGone(PathBuf),
    Io(io::Error),
}

impl fmt::Display for UndoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UndoError::Empty => write!(f, "nothing to undo"),
            UndoError::DstGone(p) => {
                write!(f, "cannot undo: {} no longer exists", p.display())
            }
            UndoError::Io(e) => write!(f, "undo failed: {e}"),
        }
    }
}

impl std::error::Error for UndoError {}

fn is_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(EXDEV)
}

/// Given a desired path, return the first non-existing path, appending `_N`
/// (1, 2, ...) to the final element's stem on collision. `img.png` becomes
/// `img_1.png`; an extensionless `img` becomes `img_1`.
fn resolve_collision(desired: &Path) -> PathBuf {
    if !desired.exists() {
        return desired.to_path_buf();
    }
    let parent = desired.parent().unwrap_or_else(|| Path::new(""));
    let stem = desired
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = desired
        .extension()
        .map(|e| e.to_string_lossy().into_owned());

    let mut n: u64 = 1;
    loop {
        let name = match &ext {
            Some(ext) => format!("{stem}_{n}.{ext}"),
            None => format!("{stem}_{n}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Move `source_abs` (a validated, canonical path under SOURCE_DIR) into
/// `dst_base`, preserving `rel`. Creates `dst_base/<rel parent>` as needed and
/// resolves collisions on the final element. Returns an [`UndoEntry`] for the
/// caller to push onto the undo stack.
pub fn perform_move(dst_base: &Path, source_abs: &Path, rel: &str) -> Result<UndoEntry, MoveError> {
    let desired = dst_base.join(rel);
    if let Some(parent) = desired.parent() {
        std::fs::create_dir_all(parent).map_err(MoveError::Io)?;
    }
    let dst = resolve_collision(&desired);

    match std::fs::rename(source_abs, &dst) {
        Ok(()) => Ok(UndoEntry {
            dst,
            original_rel: rel.to_string(),
        }),
        Err(e) if is_cross_device(&e) => Err(MoveError::CrossDevice {
            from: source_abs.to_path_buf(),
            to: dst,
        }),
        Err(e) => Err(MoveError::Io(e)),
    }
}

/// In-memory bounded stack of recent moves (design.md §6). When the depth is
/// exceeded the oldest entry is dropped; it remains undoable only as long as
/// its destination survives.
#[derive(Debug)]
pub struct UndoStack {
    entries: VecDeque<UndoEntry>,
    depth: usize,
}

impl UndoStack {
    pub fn new(depth: usize) -> Self {
        UndoStack {
            entries: VecDeque::new(),
            depth: depth.max(1),
        }
    }

    pub fn push(&mut self, entry: UndoEntry) {
        self.entries.push_back(entry);
        while self.entries.len() > self.depth {
            self.entries.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The outcome of a successful undo: where the file was restored to, plus the
/// (now vacated) destination it had been moved to, so the caller can classify
/// which kind of move — keep or trash — was undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undone {
    pub restored_rel: String,
    pub undid_dst: PathBuf,
}

/// Undo the most recent move: rename the recorded destination back under
/// SOURCE_DIR at its original relative path (collision-resolved, so a
/// regenerated file with the same name is not clobbered). Returns the relative
/// path the file was restored to and the destination it came back from.
///
/// The top entry is consumed whether the restore succeeds or the destination
/// is already gone: a vanished destination is permanently unrecoverable, so
/// keeping it would only wedge repeated undo attempts.
pub fn undo(source_dir: &Path, stack: &mut UndoStack) -> Result<Undone, UndoError> {
    let entry = stack.entries.pop_back().ok_or(UndoError::Empty)?;

    if !entry.dst.exists() {
        return Err(UndoError::DstGone(entry.dst));
    }

    let desired = source_dir.join(&entry.original_rel);
    if let Some(parent) = desired.parent() {
        std::fs::create_dir_all(parent).map_err(UndoError::Io)?;
    }
    let restore_to = resolve_collision(&desired);

    std::fs::rename(&entry.dst, &restore_to).map_err(UndoError::Io)?;

    // Report the actual restored relative path (may differ from the original
    // if a same-named file had reappeared).
    let restored_rel = restore_to
        .strip_prefix(source_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or(entry.original_rel);
    Ok(Undone {
        restored_rel,
        undid_dst: entry.dst,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Dirs {
        _tmp: tempfile::TempDir,
        source: PathBuf,
        keep: PathBuf,
    }

    fn dirs() -> Dirs {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("output");
        let keep = source.join("keep");
        fs::create_dir_all(&keep).unwrap();
        Dirs {
            _tmp: tmp,
            source,
            keep,
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn move_preserves_relative_subpath() {
        let d = dirs();
        let rel = "2026-06-01/Image_00001_.png";
        let src = d.source.join(rel);
        write(&src, b"img");

        let entry = perform_move(&d.keep, &src, rel).unwrap();
        assert_eq!(entry.dst, d.keep.join(rel));
        assert!(entry.dst.is_file());
        assert!(!src.exists(), "source file should be gone after move");
        assert_eq!(entry.original_rel, rel);
    }

    #[test]
    fn move_collision_gets_numeric_suffix() {
        let d = dirs();
        let rel = "a.png";
        write(&d.keep.join("a.png"), b"existing");
        let src = d.source.join(rel);
        write(&src, b"new");

        let entry = perform_move(&d.keep, &src, rel).unwrap();
        assert_eq!(entry.dst, d.keep.join("a_1.png"));
        assert_eq!(fs::read(&entry.dst).unwrap(), b"new");
        assert_eq!(fs::read(d.keep.join("a.png")).unwrap(), b"existing");
    }

    #[test]
    fn undo_restores_to_original_rel() {
        let d = dirs();
        let rel = "sub/x.png";
        let src = d.source.join(rel);
        write(&src, b"img");

        let mut stack = UndoStack::new(50);
        stack.push(perform_move(&d.keep, &src, rel).unwrap());
        assert_eq!(stack.len(), 1);

        let undone = undo(&d.source, &mut stack).unwrap();
        assert_eq!(undone.restored_rel, rel);
        assert_eq!(undone.undid_dst, d.keep.join(rel));
        assert!(src.is_file(), "file should be back at original location");
        assert!(stack.is_empty());
    }

    #[test]
    fn undo_empty_stack_errors() {
        let d = dirs();
        let mut stack = UndoStack::new(50);
        assert!(matches!(undo(&d.source, &mut stack), Err(UndoError::Empty)));
    }

    #[test]
    fn undo_consumes_entry_when_dst_gone() {
        let d = dirs();
        let rel = "g.png";
        let src = d.source.join(rel);
        write(&src, b"img");
        let mut stack = UndoStack::new(50);
        let entry = perform_move(&d.keep, &src, rel).unwrap();
        let dst = entry.dst.clone();
        stack.push(entry);

        // Simulate external reclamation/cleanup of the destination.
        fs::remove_file(&dst).unwrap();

        match undo(&d.source, &mut stack) {
            Err(UndoError::DstGone(p)) => assert_eq!(p, dst),
            other => panic!("expected DstGone, got {other:?}"),
        }
        assert!(stack.is_empty(), "dead entry must be consumed");
    }

    #[test]
    fn undo_collision_resolves_when_name_reappeared() {
        let d = dirs();
        let rel = "dup.png";
        let src = d.source.join(rel);
        write(&src, b"original");
        let mut stack = UndoStack::new(50);
        stack.push(perform_move(&d.keep, &src, rel).unwrap());

        // A new generation produced the same filename in the meantime.
        write(&src, b"regenerated");

        let undone = undo(&d.source, &mut stack).unwrap();
        assert_eq!(undone.restored_rel, "dup_1.png");
        assert_eq!(fs::read(d.source.join("dup_1.png")).unwrap(), b"original");
        assert_eq!(fs::read(&src).unwrap(), b"regenerated");
    }

    #[test]
    fn stack_is_bounded() {
        let mut stack = UndoStack::new(2);
        for i in 0..3 {
            stack.push(UndoEntry {
                dst: PathBuf::from(format!("/x/{i}")),
                original_rel: format!("{i}"),
            });
        }
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn cross_device_error_is_classified() {
        assert!(is_cross_device(&io::Error::from_raw_os_error(EXDEV)));
        assert!(!is_cross_device(&io::Error::from_raw_os_error(2))); // ENOENT
        assert!(!is_cross_device(&io::Error::other("not an os error")));
    }
}
