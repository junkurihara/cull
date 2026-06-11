//! HTTP layer (axum): routes, shared state, and error mapping (design.md §10).
//!
//! All filesystem work (walk, move, undo, metadata, image reads) runs on the
//! blocking thread pool so the async runtime is never stalled. The only shared
//! mutable state is the bounded undo stack behind a `std::sync::Mutex`; the
//! server holds no image list (design.md §1).

use crate::config::Config;
use crate::moves::{self, UndoStack};
use crate::paths::{self, PathError};
use crate::{meta, walk};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

/// Shared application state.
pub struct AppState {
    pub cfg: Config,
    pub undo: Mutex<UndoStack>,
    stats: Mutex<Stats>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let undo = Mutex::new(UndoStack::new(cfg.undo_depth));
        AppState {
            cfg,
            undo,
            stats: Mutex::new(Stats::default()),
        }
    }

    /// Record a successful keep/trash and return today's totals.
    fn record_move(&self, dest: Destination) -> StatsBody {
        let mut stats = self.stats.lock().expect("stats mutex poisoned");
        stats.roll(today_index(self.cfg.tz_offset_hours));
        match dest {
            Destination::Keep => stats.kept += 1,
            Destination::Trash => stats.trashed += 1,
        }
        stats.body()
    }

    /// Record a successful undo, classified by which destination the file came
    /// back from, and return today's totals. Undoing a move made before the
    /// day rolled over decrements a counter that is already zero; saturating
    /// arithmetic keeps that harmless edge case from underflowing.
    fn record_undo(&self, undid_dst: &FsPath) -> StatsBody {
        let mut stats = self.stats.lock().expect("stats mutex poisoned");
        stats.roll(today_index(self.cfg.tz_offset_hours));
        if undid_dst.starts_with(&self.cfg.keep_dir) {
            stats.kept = stats.kept.saturating_sub(1);
        } else if undid_dst.starts_with(&self.cfg.trash_dir) {
            stats.trashed = stats.trashed.saturating_sub(1);
        }
        stats.body()
    }

    /// Today's totals (rolled to the current day first, so a read just after
    /// midnight reports zeros rather than yesterday's numbers).
    fn stats_snapshot(&self) -> StatsBody {
        let mut stats = self.stats.lock().expect("stats mutex poisoned");
        stats.roll(today_index(self.cfg.tz_offset_hours));
        stats.body()
    }
}

/// Daily triage statistics. In-memory only: lost on restart, which is
/// acceptable for a "how much did I get through today" affordance. Counters
/// reset to zero when the (offset-adjusted) day index changes.
#[derive(Debug, Default)]
struct Stats {
    day: i64,
    kept: u64,
    trashed: u64,
}

impl Stats {
    fn roll(&mut self, day: i64) {
        if self.day != day {
            *self = Stats {
                day,
                kept: 0,
                trashed: 0,
            };
        }
    }

    fn body(&self) -> StatsBody {
        StatsBody {
            kept: self.kept,
            trashed: self.trashed,
        }
    }
}

/// Today's totals as exposed over the API (in `/api/stats` and echoed in
/// every move/undo response so the client never needs an extra round trip).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct StatsBody {
    kept: u64,
    trashed: u64,
}

/// Day index (days since the Unix epoch) shifted by the configured UTC offset,
/// so "today" rolls over at local midnight rather than 00:00 UTC.
fn today_index(tz_offset_hours: i64) -> i64 {
    // A clock before the epoch is not a real deployment condition; mapping it
    // to 0 merely groups such times into one day instead of failing requests.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    day_of(secs, tz_offset_hours)
}

/// Pure day-index computation, separated from the clock for testability.
fn day_of(unix_secs: i64, tz_offset_hours: i64) -> i64 {
    (unix_secs + tz_offset_hours * 3600).div_euclid(86400)
}

/// Build the application router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/next", get(next))
        .route("/api/count", get(count))
        .route("/api/stats", get(stats))
        .route("/api/image/{*relpath}", get(image))
        .route("/api/meta/{*relpath}", get(meta_endpoint))
        .route("/api/keep", post(keep))
        .route("/api/trash", post(trash))
        .route("/api/undo", post(undo))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// The single-page app, embedded into the binary (design.md §12).
async fn index() -> impl IntoResponse {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(include_str!("../static/index.html")))
        .expect("static index response is well-formed")
}

#[derive(Debug, Deserialize)]
struct NextParams {
    after: Option<String>,
}

#[derive(Debug, Serialize)]
struct NextResponse {
    relpath: String,
}

async fn next(
    State(st): State<Arc<AppState>>,
    Query(params): Query<NextParams>,
) -> Result<Response, ApiError> {
    let cfg = st.cfg.clone();
    let found = run_blocking(move || {
        // Time the walk: each `next` is a full O(n) pass, so this is the number
        // to watch when the backlog is large (design.md §1, §2).
        let started = std::time::Instant::now();
        let result = walk::find_next(&cfg, params.after.as_deref());
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "find_next walk"
        );
        result.map_err(ApiError::from)
    })
    .await?;
    match found {
        // 204 with an empty body signals a drained queue (design.md §10).
        Some(relpath) => Ok(Json(NextResponse { relpath }).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[derive(Debug, Serialize)]
struct CountResponse {
    count: usize,
}

async fn count(State(st): State<Arc<AppState>>) -> Result<Json<CountResponse>, ApiError> {
    let cfg = st.cfg.clone();
    let count = run_blocking(move || {
        let started = std::time::Instant::now();
        let result = walk::count_backlog(&cfg);
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "count_backlog walk"
        );
        result.map_err(ApiError::from)
    })
    .await?;
    Ok(Json(CountResponse { count }))
}

async fn stats(State(st): State<Arc<AppState>>) -> Json<StatsBody> {
    Json(st.stats_snapshot())
}

async fn image(
    State(st): State<Arc<AppState>>,
    Path(relpath): Path<String>,
) -> Result<Response, ApiError> {
    let cfg = st.cfg.clone();
    let (abs, bytes) = run_blocking(move || {
        let abs = paths::validate_relpath(&cfg, &relpath)?;
        let bytes = std::fs::read(&abs).map_err(|_| ApiError::from(PathError::NotFound))?;
        Ok((abs, bytes))
    })
    .await?;

    let mime = mime_guess::from_path(&abs).first_or_octet_stream();
    Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("failed to build image response: {e}")))
}

async fn meta_endpoint(
    State(st): State<Arc<AppState>>,
    Path(relpath): Path<String>,
) -> Result<Json<meta::Meta>, ApiError> {
    let cfg = st.cfg.clone();
    let meta = run_blocking(move || {
        let abs = paths::validate_relpath(&cfg, &relpath)?;
        Ok(meta::extract_meta(&abs))
    })
    .await?;
    Ok(Json(meta))
}

#[derive(Debug, Deserialize)]
struct RelpathBody {
    relpath: String,
}

#[derive(Debug, Serialize)]
struct MoveResponse {
    /// The relative path that was acted on (echoed back for the client).
    relpath: String,
    /// Whether an undo is now available.
    can_undo: bool,
    /// Today's totals after this move.
    stats: StatsBody,
}

#[derive(Debug, Clone, Copy)]
enum Destination {
    Keep,
    Trash,
}

async fn keep(
    state: State<Arc<AppState>>,
    body: Json<RelpathBody>,
) -> Result<Json<MoveResponse>, ApiError> {
    move_to(state, body, Destination::Keep).await
}

async fn trash(
    state: State<Arc<AppState>>,
    body: Json<RelpathBody>,
) -> Result<Json<MoveResponse>, ApiError> {
    move_to(state, body, Destination::Trash).await
}

async fn move_to(
    State(st): State<Arc<AppState>>,
    Json(body): Json<RelpathBody>,
    dest: Destination,
) -> Result<Json<MoveResponse>, ApiError> {
    let relpath = body.relpath;
    let echo = relpath.clone();
    let (can_undo, stats) = run_blocking(move || {
        let source_abs = paths::validate_relpath(&st.cfg, &relpath)?;
        let dst_base = match dest {
            Destination::Keep => &st.cfg.keep_dir,
            Destination::Trash => &st.cfg.trash_dir,
        };
        let entry = moves::perform_move(dst_base, &source_abs, &relpath).map_err(ApiError::from)?;
        let can_undo = {
            let mut stack = st.undo.lock().expect("undo mutex poisoned");
            stack.push(entry);
            !stack.is_empty()
        };
        Ok((can_undo, st.record_move(dest)))
    })
    .await?;
    Ok(Json(MoveResponse {
        relpath: echo,
        can_undo,
        stats,
    }))
}

#[derive(Debug, Serialize)]
struct UndoResponse {
    relpath: String,
    can_undo: bool,
    /// Today's totals after this undo.
    stats: StatsBody,
}

async fn undo(State(st): State<Arc<AppState>>) -> Result<Json<UndoResponse>, ApiError> {
    let (relpath, can_undo, stats) = run_blocking(move || {
        let (undone, can_undo) = {
            let mut stack = st.undo.lock().expect("undo mutex poisoned");
            let undone = moves::undo(&st.cfg.source_dir, &mut stack).map_err(ApiError::from)?;
            (undone, !stack.is_empty())
        };
        let stats = st.record_undo(&undone.undid_dst);
        Ok((undone.restored_rel, can_undo, stats))
    })
    .await?;
    Ok(Json(UndoResponse {
        relpath,
        can_undo,
        stats,
    }))
}

/// Run filesystem work on the blocking pool, mapping a join failure to 500.
async fn run_blocking<F, T>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?
}

/// An API error carrying the HTTP status and a human-readable message.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn internal(message: String) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<PathError> for ApiError {
    fn from(e: PathError) -> Self {
        // A missing file is 404; any other path rejection is a bad request.
        let status = match e {
            PathError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_REQUEST,
        };
        ApiError {
            status,
            message: e.to_string(),
        }
    }
}

impl From<moves::MoveError> for ApiError {
    fn from(e: moves::MoveError) -> Self {
        // CrossDevice and IO are server-side faults (misconfiguration / disk).
        ApiError::internal(e.to_string())
    }
}

impl From<moves::UndoError> for ApiError {
    fn from(e: moves::UndoError) -> Self {
        use moves::UndoError;
        let status = match e {
            // Nothing to undo, or the destination is already gone: 409 so the
            // client can show "can't undo" without treating it as a hard error.
            UndoError::Empty | UndoError::DstGone(_) => StatusCode::CONFLICT,
            UndoError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError {
            status,
            message: e.to_string(),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_roll_resets_counters_only_on_day_change() {
        let mut s = Stats {
            day: 1,
            kept: 3,
            trashed: 4,
        };
        s.roll(1);
        assert_eq!((s.kept, s.trashed), (3, 4));
        s.roll(2);
        assert_eq!((s.day, s.kept, s.trashed), (2, 0, 0));
    }

    #[test]
    fn day_of_rolls_at_offset_midnight() {
        assert_eq!(day_of(0, 0), 0);
        assert_eq!(day_of(86_399, 0), 0);
        assert_eq!(day_of(86_400, 0), 1);
        // With +9 (JST) the day flips 9 hours earlier in UTC terms.
        assert_eq!(day_of(86_400 - 9 * 3600, 9), 1);
        assert_eq!(day_of(86_400 - 9 * 3600 - 1, 9), 0);
        // Negative offsets shift the boundary the other way (euclidean
        // division keeps pre-boundary times on the previous day).
        assert_eq!(day_of(3_600, -2), -1);
    }
}
