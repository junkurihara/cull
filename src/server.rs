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
use std::sync::{Arc, Mutex};

/// Shared application state.
pub struct AppState {
    pub cfg: Config,
    pub undo: Mutex<UndoStack>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let undo = Mutex::new(UndoStack::new(cfg.undo_depth));
        AppState { cfg, undo }
    }
}

/// Build the application router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/next", get(next))
        .route("/api/count", get(count))
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
    let can_undo = run_blocking(move || {
        let source_abs = paths::validate_relpath(&st.cfg, &relpath)?;
        let dst_base = match dest {
            Destination::Keep => &st.cfg.keep_dir,
            Destination::Trash => &st.cfg.trash_dir,
        };
        let entry = moves::perform_move(dst_base, &source_abs, &relpath).map_err(ApiError::from)?;
        let mut stack = st.undo.lock().expect("undo mutex poisoned");
        stack.push(entry);
        Ok(!stack.is_empty())
    })
    .await?;
    Ok(Json(MoveResponse {
        relpath: echo,
        can_undo,
    }))
}

#[derive(Debug, Serialize)]
struct UndoResponse {
    relpath: String,
    can_undo: bool,
}

async fn undo(State(st): State<Arc<AppState>>) -> Result<Json<UndoResponse>, ApiError> {
    let (relpath, can_undo) = run_blocking(move || {
        let mut stack = st.undo.lock().expect("undo mutex poisoned");
        let relpath = moves::undo(&st.cfg.source_dir, &mut stack).map_err(ApiError::from)?;
        Ok((relpath, !stack.is_empty()))
    })
    .await?;
    Ok(Json(UndoResponse { relpath, can_undo }))
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
