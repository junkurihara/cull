//! Runtime configuration, loaded from environment variables.
//!
//! Parsing (env -> values + defaults) is kept separate from resolution
//! (filesystem canonicalization, directory creation) so the parsing logic can
//! be unit-tested without touching real directories. See design.md §13.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Enumeration direction for `next` (design.md §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Oldest first (FIFO): walk ascending by relative path.
    Asc,
    /// Newest first: walk descending by relative path.
    Desc,
}

/// Default values (design.md §13). Kept as constants so tests and docs agree.
pub const DEFAULT_SOURCE_DIR: &str = "/data/images";
pub const DEFAULT_ORDER: Order = Order::Asc;
pub const DEFAULT_EXTENSIONS: &str = "png,jpg,jpeg,webp";
pub const DEFAULT_UNDO_DEPTH: usize = 50;
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";
pub const DEFAULT_TZ_OFFSET_HOURS: i64 = 0;

/// Configuration errors. A dedicated enum (rather than swallowing into a
/// string) so callers can distinguish causes if needed.
#[derive(Debug)]
pub enum ConfigError {
    InvalidOrder(String),
    EmptyExtensions,
    InvalidUndoDepth(String),
    InvalidBindAddr(String),
    InvalidTzOffset(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidOrder(v) => {
                write!(f, "ORDER must be 'asc' or 'desc', got {v:?}")
            }
            ConfigError::EmptyExtensions => {
                write!(f, "EXTENSIONS resolved to an empty set")
            }
            ConfigError::InvalidUndoDepth(v) => {
                write!(f, "UNDO_DEPTH must be a positive integer, got {v:?}")
            }
            ConfigError::InvalidBindAddr(v) => {
                write!(f, "BIND_ADDR is not a valid socket address: {v:?}")
            }
            ConfigError::InvalidTzOffset(v) => {
                write!(
                    f,
                    "TZ_OFFSET_HOURS must be an integer between -26 and 26, got {v:?}"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Parsed configuration prior to filesystem resolution. Paths here are exactly
/// as supplied (or derived) and are not yet canonicalized.
#[derive(Debug, Clone)]
pub struct RawConfig {
    pub source_dir: PathBuf,
    pub keep_dir: PathBuf,
    pub trash_dir: PathBuf,
    pub order: Order,
    /// Lowercase, dot-less extension set (e.g. "png").
    pub extensions: BTreeSet<String>,
    pub undo_depth: usize,
    pub bind_addr: SocketAddr,
    /// UTC offset (whole hours) used only to decide where "today" begins for
    /// the daily triage statistics. Not a full timezone: no DST handling.
    pub tz_offset_hours: i64,
}

/// Fully resolved configuration: directories exist and are canonicalized
/// (symlinks resolved), so prune and traversal checks can compare absolute
/// paths (design.md §2, §10).
#[derive(Debug, Clone)]
pub struct Config {
    pub source_dir: PathBuf,
    pub keep_dir: PathBuf,
    pub trash_dir: PathBuf,
    pub order: Order,
    pub extensions: BTreeSet<String>,
    pub undo_depth: usize,
    pub bind_addr: SocketAddr,
    pub tz_offset_hours: i64,
}

fn parse_order(s: &str) -> Result<Order, ConfigError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "asc" => Ok(Order::Asc),
        "desc" => Ok(Order::Desc),
        other => Err(ConfigError::InvalidOrder(other.to_string())),
    }
}

/// Parse a comma-separated extension list into a normalized set: lowercase,
/// trimmed, leading dots stripped, empty entries dropped.
fn parse_extensions(s: &str) -> Result<BTreeSet<String>, ConfigError> {
    let set: BTreeSet<String> = s
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    if set.is_empty() {
        Err(ConfigError::EmptyExtensions)
    } else {
        Ok(set)
    }
}

impl RawConfig {
    /// Load configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::load(|key| std::env::var(key).ok())
    }

    /// Load configuration using an arbitrary getter. The getter returns `None`
    /// for unset variables; this indirection makes the logic unit-testable
    /// without mutating global process state.
    pub fn load(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let source_dir = PathBuf::from(
            get("SOURCE_DIR")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_SOURCE_DIR.to_string()),
        );

        // keep/trash default to children of source (design.md §13).
        let keep_dir = get("KEEP_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| source_dir.join("keep"));
        let trash_dir = get("TRASH_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| source_dir.join("trash"));

        let order = match get("ORDER").filter(|s| !s.is_empty()) {
            Some(v) => parse_order(&v)?,
            None => DEFAULT_ORDER,
        };

        let extensions = parse_extensions(
            get("EXTENSIONS")
                .filter(|s| !s.is_empty())
                .as_deref()
                .unwrap_or(DEFAULT_EXTENSIONS),
        )?;

        let undo_depth = match get("UNDO_DEPTH").filter(|s| !s.is_empty()) {
            Some(v) => v
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| ConfigError::InvalidUndoDepth(v.clone()))?,
            None => DEFAULT_UNDO_DEPTH,
        };

        let bind_raw = get("BIND_ADDR")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
        let bind_addr = bind_raw
            .parse::<SocketAddr>()
            .map_err(|_| ConfigError::InvalidBindAddr(bind_raw.clone()))?;

        // Real-world UTC offsets span -12..+14; allow a little slack but
        // reject obvious typos (e.g. minutes passed as hours).
        let tz_offset_hours = match get("TZ_OFFSET_HOURS").filter(|s| !s.is_empty()) {
            Some(v) => v
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|h| h.abs() <= 26)
                .ok_or_else(|| ConfigError::InvalidTzOffset(v.clone()))?,
            None => DEFAULT_TZ_OFFSET_HOURS,
        };

        Ok(RawConfig {
            source_dir,
            keep_dir,
            trash_dir,
            order,
            extensions,
            undo_depth,
            bind_addr,
            tz_offset_hours,
        })
    }

    /// Resolve to a [`Config`]: create KEEP/TRASH if absent, then canonicalize
    /// all three directories. SOURCE_DIR must already exist (canonicalize
    /// fails otherwise), which is the intended fail-fast on misconfiguration.
    pub fn resolve(self) -> std::io::Result<Config> {
        std::fs::create_dir_all(&self.keep_dir)?;
        std::fs::create_dir_all(&self.trash_dir)?;

        let source_dir = canonicalize_existing(&self.source_dir)?;
        let keep_dir = canonicalize_existing(&self.keep_dir)?;
        let trash_dir = canonicalize_existing(&self.trash_dir)?;

        Ok(Config {
            source_dir,
            keep_dir,
            trash_dir,
            order: self.order,
            extensions: self.extensions,
            undo_depth: self.undo_depth,
            bind_addr: self.bind_addr,
            tz_offset_hours: self.tz_offset_hours,
        })
    }
}

fn canonicalize_existing(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("cannot canonicalize {}: {e}", p.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a getter closure from a map for deterministic, env-free tests.
    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn defaults_apply_when_unset() {
        let cfg = RawConfig::load(getter(&[])).expect("defaults must parse");
        assert_eq!(cfg.source_dir, PathBuf::from(DEFAULT_SOURCE_DIR));
        assert_eq!(cfg.keep_dir, PathBuf::from(DEFAULT_SOURCE_DIR).join("keep"));
        assert_eq!(
            cfg.trash_dir,
            PathBuf::from(DEFAULT_SOURCE_DIR).join("trash")
        );
        assert_eq!(cfg.order, Order::Asc);
        assert_eq!(cfg.undo_depth, DEFAULT_UNDO_DEPTH);
        assert_eq!(cfg.bind_addr.port(), 8080);
        let exts: BTreeSet<String> = ["jpeg", "jpg", "png", "webp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(cfg.extensions, exts);
    }

    #[test]
    fn keep_trash_default_to_source_children() {
        let cfg = RawConfig::load(getter(&[("SOURCE_DIR", "/data/out")])).unwrap();
        assert_eq!(cfg.keep_dir, PathBuf::from("/data/out/keep"));
        assert_eq!(cfg.trash_dir, PathBuf::from("/data/out/trash"));
    }

    #[test]
    fn explicit_keep_trash_override() {
        let cfg = RawConfig::load(getter(&[
            ("SOURCE_DIR", "/data/out"),
            ("KEEP_DIR", "/elsewhere/k"),
            ("TRASH_DIR", "/elsewhere/t"),
        ]))
        .unwrap();
        assert_eq!(cfg.keep_dir, PathBuf::from("/elsewhere/k"));
        assert_eq!(cfg.trash_dir, PathBuf::from("/elsewhere/t"));
    }

    #[test]
    fn order_parsing_is_case_insensitive() {
        assert_eq!(
            RawConfig::load(getter(&[("ORDER", "DESC")])).unwrap().order,
            Order::Desc
        );
        assert_eq!(
            RawConfig::load(getter(&[("ORDER", " asc ")]))
                .unwrap()
                .order,
            Order::Asc
        );
    }

    #[test]
    fn invalid_order_rejected() {
        let err = RawConfig::load(getter(&[("ORDER", "sideways")])).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidOrder(_)));
    }

    #[test]
    fn extensions_normalized() {
        let cfg = RawConfig::load(getter(&[("EXTENSIONS", " .PNG, JPG ,,jpg")])).unwrap();
        let exts: BTreeSet<String> = ["jpg", "png"].iter().map(|s| s.to_string()).collect();
        assert_eq!(cfg.extensions, exts);
    }

    #[test]
    fn empty_extensions_rejected() {
        let err = RawConfig::load(getter(&[("EXTENSIONS", " , ,")])).unwrap_err();
        assert!(matches!(err, ConfigError::EmptyExtensions));
    }

    #[test]
    fn undo_depth_must_be_positive_integer() {
        assert!(matches!(
            RawConfig::load(getter(&[("UNDO_DEPTH", "0")])).unwrap_err(),
            ConfigError::InvalidUndoDepth(_)
        ));
        assert!(matches!(
            RawConfig::load(getter(&[("UNDO_DEPTH", "abc")])).unwrap_err(),
            ConfigError::InvalidUndoDepth(_)
        ));
        assert_eq!(
            RawConfig::load(getter(&[("UNDO_DEPTH", "10")]))
                .unwrap()
                .undo_depth,
            10
        );
    }

    #[test]
    fn tz_offset_parsed_and_bounded() {
        assert_eq!(
            RawConfig::load(getter(&[])).unwrap().tz_offset_hours,
            DEFAULT_TZ_OFFSET_HOURS
        );
        assert_eq!(
            RawConfig::load(getter(&[("TZ_OFFSET_HOURS", "9")]))
                .unwrap()
                .tz_offset_hours,
            9
        );
        assert_eq!(
            RawConfig::load(getter(&[("TZ_OFFSET_HOURS", "-5")]))
                .unwrap()
                .tz_offset_hours,
            -5
        );
        assert!(matches!(
            RawConfig::load(getter(&[("TZ_OFFSET_HOURS", "540")])).unwrap_err(),
            ConfigError::InvalidTzOffset(_)
        ));
        assert!(matches!(
            RawConfig::load(getter(&[("TZ_OFFSET_HOURS", "jst")])).unwrap_err(),
            ConfigError::InvalidTzOffset(_)
        ));
    }

    #[test]
    fn invalid_bind_addr_rejected() {
        assert!(matches!(
            RawConfig::load(getter(&[("BIND_ADDR", "not-an-addr")])).unwrap_err(),
            ConfigError::InvalidBindAddr(_)
        ));
    }

    #[test]
    fn resolve_creates_and_canonicalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("output");
        std::fs::create_dir_all(&source).unwrap();
        let raw = RawConfig::load(getter(&[("SOURCE_DIR", source.to_str().unwrap())])).unwrap();
        let cfg = raw.resolve().expect("resolve should succeed");
        // keep/trash were created under source and canonicalized.
        assert!(cfg.keep_dir.is_dir());
        assert!(cfg.trash_dir.is_dir());
        assert!(cfg.keep_dir.starts_with(&cfg.source_dir));
        assert!(cfg.trash_dir.starts_with(&cfg.source_dir));
    }

    #[test]
    fn resolve_fails_when_source_missing() {
        let raw =
            RawConfig::load(getter(&[("SOURCE_DIR", "/nonexistent/triage/source/xyz")])).unwrap();
        assert!(raw.resolve().is_err());
    }
}
