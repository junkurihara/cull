//! HTTP-level integration tests for the router wiring (design.md §10).
//!
//! Drives the axum Router via `tower::ServiceExt::oneshot` against a temporary
//! source tree, asserting status codes and the next -> keep -> undo flow.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use cull::config::RawConfig;
use cull::server::{router, AppState};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn state_with_tree() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("output");
    std::fs::create_dir_all(&source).unwrap();

    // A couple of valid 1x1 PNGs (so meta/extension handling is realistic).
    write_png(&source.join("Image_00001_.png"));
    write_png(&source.join("Image_00002_.png"));

    let src = source.to_str().unwrap().to_string();
    let cfg = RawConfig::load(move |k: &str| match k {
        "SOURCE_DIR" => Some(src.clone()),
        _ => None,
    })
    .unwrap()
    .resolve()
    .unwrap();

    (Arc::new(AppState::new(cfg)), tmp)
}

fn write_png(path: &std::path::Path) {
    use std::io::BufWriter;
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(BufWriter::new(file), 1, 1);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&[0u8]).unwrap();
}

async fn get(state: &Arc<AppState>, uri: &str) -> (StatusCode, Vec<u8>) {
    let resp = router(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

async fn post_json(state: &Arc<AppState>, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test]
async fn count_reports_backlog() {
    let (state, _tmp) = state_with_tree();
    let (status, body) = get(&state, "/api/count").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["count"], 2);
}

#[tokio::test]
async fn next_returns_min_then_204_when_drained() {
    let (state, _tmp) = state_with_tree();

    let (status, body) = get(&state, "/api/next").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["relpath"], "Image_00001_.png");

    // Advancing past the last entry drains the queue.
    let (status, _) = get(&state, "/api/next?after=Image_00002_.png").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn image_serves_png_content_type() {
    let (state, _tmp) = state_with_tree();
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/image/Image_00001_.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
}

#[tokio::test]
async fn image_missing_is_404() {
    let (state, _tmp) = state_with_tree();
    let (status, _) = get(&state, "/api/image/nope.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn keep_then_undo_round_trip() {
    let (state, _tmp) = state_with_tree();

    // keep the first image
    let (status, body) =
        post_json(&state, "/api/keep", json!({"relpath":"Image_00001_.png"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["relpath"], "Image_00001_.png");
    assert_eq!(body["can_undo"], true);

    // backlog dropped by one
    let (_, body) = get(&state, "/api/count").await;
    assert_eq!(json(&body)["count"], 1);

    // undo restores it
    let (status, body) = post_json(&state, "/api/undo", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["relpath"], "Image_00001_.png");
    assert_eq!(body["can_undo"], false);

    let (_, body) = get(&state, "/api/count").await;
    assert_eq!(json(&body)["count"], 2);
}

#[tokio::test]
async fn stats_track_moves_and_undo() {
    let (state, _tmp) = state_with_tree();

    // A fresh server reports zeros.
    let (status, body) = get(&state, "/api/stats").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["kept"], 0);
    assert_eq!(v["trashed"], 0);

    // Each move response echoes the updated totals.
    let (_, body) = post_json(&state, "/api/keep", json!({"relpath":"Image_00001_.png"})).await;
    assert_eq!(body["stats"]["kept"], 1);
    assert_eq!(body["stats"]["trashed"], 0);

    let (_, body) = post_json(&state, "/api/trash", json!({"relpath":"Image_00002_.png"})).await;
    assert_eq!(body["stats"]["kept"], 1);
    assert_eq!(body["stats"]["trashed"], 1);

    // Undo (the last move was the trash) decrements the matching counter.
    let (_, body) = post_json(&state, "/api/undo", json!({})).await;
    assert_eq!(body["stats"]["kept"], 1);
    assert_eq!(body["stats"]["trashed"], 0);

    let (_, body) = get(&state, "/api/stats").await;
    let v = json(&body);
    assert_eq!(v["kept"], 1);
    assert_eq!(v["trashed"], 0);
}

#[tokio::test]
async fn keep_gallery_list_thumb_and_retriage() {
    let (state, _tmp) = state_with_tree();

    // Move both images into keep.
    post_json(&state, "/api/keep", json!({"relpath":"Image_00001_.png"})).await;
    post_json(&state, "/api/keep", json!({"relpath":"Image_00002_.png"})).await;

    // Listing is newest-first (descending relpath) and paginates via `after`.
    let (status, body) = get(&state, "/api/keep/list").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json(&body)["items"],
        json!(["Image_00002_.png", "Image_00001_.png"])
    );
    let (_, body) = get(&state, "/api/keep/list?limit=1").await;
    assert_eq!(json(&body)["items"], json!(["Image_00002_.png"]));
    let (_, body) = get(&state, "/api/keep/list?after=Image_00002_.png&limit=1").await;
    assert_eq!(json(&body)["items"], json!(["Image_00001_.png"]));

    // Full image and thumbnail are served from KEEP_DIR; the thumb is a JPEG.
    let (status, _) = get(&state, "/api/keep/image/Image_00001_.png").await;
    assert_eq!(status, StatusCode::OK);
    let (status, bytes) = get(&state, "/api/keep/thumb/Image_00001_.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "thumb must be JPEG");

    // Restore returns the file to the source tree: backlog +1, kept -1.
    let (status, body) = post_json(
        &state,
        "/api/keep/restore",
        json!({"relpath":"Image_00001_.png"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["relpath"], "Image_00001_.png");
    assert_eq!(body["stats"]["kept"], 1);
    let (_, body) = get(&state, "/api/count").await;
    assert_eq!(json(&body)["count"], 1);

    // Demoting a keep straight to trash flips both counters.
    let (status, body) = post_json(
        &state,
        "/api/keep/trash",
        json!({"relpath":"Image_00002_.png"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stats"]["kept"], 0);
    assert_eq!(body["stats"]["trashed"], 1);

    // The gallery is now empty.
    let (_, body) = get(&state, "/api/keep/list").await;
    assert_eq!(json(&body)["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn keep_gallery_rejects_traversal_and_missing() {
    let (state, _tmp) = state_with_tree();
    let (status, _) = post_json(
        &state,
        "/api/keep/restore",
        json!({"relpath":"../escape.png"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&state, "/api/keep/thumb/missing.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn undo_empty_is_409() {
    let (state, _tmp) = state_with_tree();
    let (status, _) = post_json(&state, "/api/undo", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn keep_with_traversal_is_400() {
    let (state, _tmp) = state_with_tree();
    let (status, _) = post_json(&state, "/api/keep", json!({"relpath":"../escape.png"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn meta_for_plain_png_is_empty() {
    let (state, _tmp) = state_with_tree();
    let (status, body) = get(&state, "/api/meta/Image_00001_.png").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert!(v["raw"].is_null());
    assert_eq!(v["prompts"].as_array().unwrap().len(), 0);
}
