use axum::Extension;
use axum::response::Result;
use hyper::{Body, Response, StatusCode};
use serde_json::json;

use crate::telemetry::telemetry_structs;
use crate::telemetry::snippets_collection;
use crate::custom_error::ScratchError;
use crate::global_context::SharedGlobalContext;

pub async fn handle_v1_snippet_accepted(
    Extension(global_context): Extension<SharedGlobalContext>,
    body_bytes: hyper::body::Bytes
) -> Result<Response<Body>, ScratchError> {
    let post = serde_json::from_slice::<telemetry_structs::SnippetTracker>(&body_bytes).map_err(|e| {
        ScratchError::new(StatusCode::BAD_REQUEST, format!("JSON problem: {}", e))
    })?;
    let success = snippets_collection::snippet_accepted(global_context.clone(), post.snippet_telemetry_id).await;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(json!({"success": success}).to_string()))
        .unwrap())
}
