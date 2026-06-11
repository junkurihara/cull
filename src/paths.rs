//! Path validation against directory traversal (design.md §10).
//!
//! A received name is always a relative path. We reject empty, absolute, and
//! `..`-bearing inputs up front, then join under SOURCE_DIR, canonicalize
//! (resolving symlinks), and re-confirm the result lands under SOURCE_DIR and
//! NOT under KEEP_DIR/TRASH_DIR. We deliberately do not basename the input:
//! subdirectory structure must be preserved (design.md §2, §6).

use crate::config::Config;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Reasons a relative path is rejected. Mapped to HTTP status by the API layer:
/// `NotFound` -> 404, everything else -> 400 (a malformed/illegal request).
#[derive(Debug, PartialEq, Eq)]
pub enum PathError {
    /// The supplied name was empty.
    Empty,
    /// The name was absolute (had a root or prefix component).
    Absolute,
    /// The name contained a `..` component.
    ParentTraversal,
    /// The path does not resolve to an existing entry.
    NotFound,
    /// The canonicalized path escaped SOURCE_DIR (e.g. via a symlink).
    OutsideSource,
    /// The canonicalized path resolved into KEEP_DIR or TRASH_DIR.
    InsideDestination,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            PathError::Empty => "path is empty",
            PathError::Absolute => "path must be relative, not absolute",
            PathError::ParentTraversal => "path must not contain '..' components",
            PathError::NotFound => "path does not resolve to an existing file",
            PathError::OutsideSource => "path escapes the source directory",
            PathError::InsideDestination => "path resolves inside keep/trash",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for PathError {}

/// True if `path` is `base` itself or lies within its subtree. Both arguments
/// are expected to be canonical absolute paths so component-wise `starts_with`
/// is meaningful (no `..`/`.`/symlink surprises).
pub fn is_under(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

/// Validate a client-supplied relative path and resolve it to a canonical
/// absolute path guaranteed to live under SOURCE_DIR and outside KEEP/TRASH.
///
/// This is the single choke point for reads (`image`, `meta`) and for the
/// source side of a move (`keep`, `trash`). The destination path of a move is
/// computed separately in the move layer (it does not exist yet, so it cannot
/// be canonicalized here).
pub fn validate_relpath(cfg: &Config, rel: &str) -> Result<PathBuf, PathError> {
    validate_relpath_under(&cfg.source_dir, rel, &[&cfg.keep_dir, &cfg.trash_dir])
}

/// Validate a client-supplied relative path against an arbitrary canonical
/// `base` directory (the keep gallery validates against KEEP_DIR). The
/// resolved path must exist, stay inside `base` after symlink resolution, and
/// avoid every `forbidden` subtree.
pub fn validate_relpath_under(
    base: &Path,
    rel: &str,
    forbidden: &[&Path],
) -> Result<PathBuf, PathError> {
    if rel.is_empty() {
        return Err(PathError::Empty);
    }

    // Reject illegal components before touching the filesystem.
    let rel_path = Path::new(rel);
    for comp in rel_path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => return Err(PathError::Absolute),
            Component::ParentDir => return Err(PathError::ParentTraversal),
            // CurDir ('.') is harmless and collapsed by canonicalize.
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    let joined = base.join(rel_path);
    let canonical = std::fs::canonicalize(&joined).map_err(|_| PathError::NotFound)?;

    // Re-confirm containment on the resolved path: a symlink inside the base
    // could otherwise point anywhere (design.md §10).
    if !is_under(&canonical, base) {
        return Err(PathError::OutsideSource);
    }
    if forbidden.iter().any(|f| is_under(&canonical, f)) {
        return Err(PathError::InsideDestination);
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RawConfig;
    use std::fs;

    /// Build a resolved Config rooted at a fresh tempdir, plus the tempdir
    /// guard (kept alive by the caller).
    fn test_config() -> (Config, tempfile::TempDir) {
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
        (cfg, tmp)
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn accepts_file_in_source_root() {
        let (cfg, _tmp) = test_config();
        touch(&cfg.source_dir.join("Image_00001_.png"));
        let resolved = validate_relpath(&cfg, "Image_00001_.png").unwrap();
        assert_eq!(resolved, cfg.source_dir.join("Image_00001_.png"));
    }

    #[test]
    fn accepts_file_in_subdir() {
        let (cfg, _tmp) = test_config();
        touch(&cfg.source_dir.join("2026-06-01/Image_00007_.png"));
        let resolved = validate_relpath(&cfg, "2026-06-01/Image_00007_.png").unwrap();
        assert!(resolved.ends_with("2026-06-01/Image_00007_.png"));
    }

    #[test]
    fn rejects_empty() {
        let (cfg, _tmp) = test_config();
        assert_eq!(validate_relpath(&cfg, ""), Err(PathError::Empty));
    }

    #[test]
    fn rejects_absolute() {
        let (cfg, _tmp) = test_config();
        assert_eq!(
            validate_relpath(&cfg, "/etc/passwd"),
            Err(PathError::Absolute)
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        let (cfg, _tmp) = test_config();
        assert_eq!(
            validate_relpath(&cfg, "../secret.png"),
            Err(PathError::ParentTraversal)
        );
        assert_eq!(
            validate_relpath(&cfg, "a/../../secret.png"),
            Err(PathError::ParentTraversal)
        );
    }

    #[test]
    fn rejects_nonexistent() {
        let (cfg, _tmp) = test_config();
        assert_eq!(
            validate_relpath(&cfg, "missing.png"),
            Err(PathError::NotFound)
        );
    }

    #[test]
    fn rejects_path_inside_keep() {
        let (cfg, _tmp) = test_config();
        // keep dir is under source by default; a file inside it must be rejected
        // as a destination, not served as a source image.
        touch(&cfg.keep_dir.join("kept.png"));
        let rel = cfg
            .keep_dir
            .strip_prefix(&cfg.source_dir)
            .unwrap()
            .join("kept.png");
        assert_eq!(
            validate_relpath(&cfg, rel.to_str().unwrap()),
            Err(PathError::InsideDestination)
        );
    }

    #[test]
    fn validate_under_keep_base_accepts_kept_file() {
        let (cfg, _tmp) = test_config();
        touch(&cfg.keep_dir.join("sub/kept.png"));
        let resolved = validate_relpath_under(&cfg.keep_dir, "sub/kept.png", &[]).unwrap();
        assert_eq!(resolved, cfg.keep_dir.join("sub/kept.png"));
        // The same traversal rules apply against the alternate base.
        assert_eq!(
            validate_relpath_under(&cfg.keep_dir, "../escape.png", &[]),
            Err(PathError::ParentTraversal)
        );
        assert_eq!(
            validate_relpath_under(&cfg.keep_dir, "missing.png", &[]),
            Err(PathError::NotFound)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_source() {
        use std::os::unix::fs::symlink;
        let (cfg, tmp) = test_config();
        // Target lives outside the source tree.
        let outside = tmp.path().join("outside.png");
        touch(&outside);
        let link = cfg.source_dir.join("escape.png");
        symlink(&outside, &link).unwrap();
        assert_eq!(
            validate_relpath(&cfg, "escape.png"),
            Err(PathError::OutsideSource)
        );
    }
}
