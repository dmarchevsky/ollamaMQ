//! `/ui/api/reviews` and `/ui/api/review-prompts`: send a conversation to
//! review, follow it, read what came back, and keep the prompt library.
//! See [`crate::review`].

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult};
use crate::dispatcher::AppState;
use crate::review::{Request, SubmitError};

impl From<SubmitError> for ApiError {
    fn from(e: SubmitError) -> Self {
        match e {
            SubmitError::NotFound(m) => ApiError::new(StatusCode::NOT_FOUND, m),
            SubmitError::Invalid(m) => ApiError::new(StatusCode::BAD_REQUEST, m),
        }
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    /// One conversation's reviews only (its key).
    conv: Option<String>,
}

pub async fn list(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Json<Value> {
    Json(json!({
        "reviews": state.review.list(q.conv.as_deref()),
        "enabled": state.review.settings().enabled,
    }))
}

pub async fn get(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<crate::review::Job> {
    state
        .review
        .get(&id)
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such review"))
}

#[derive(Deserialize)]
pub struct SubmitBody {
    key: String,
    prompt_id: Option<String>,
    prompt: Option<String>,
    model: Option<String>,
    /// Start it at once instead of waiting for a quiet backend.
    #[serde(default)]
    run_now: bool,
}

pub async fn submit(
    State(state): State<Arc<AppState>>,
    Json(b): Json<SubmitBody>,
) -> ApiResult<crate::review::Job> {
    let job = state.review.submit(
        &state,
        Request {
            key: &b.key,
            prompt_id: b.prompt_id.as_deref(),
            prompt: b.prompt.as_deref(),
            model: b.model.as_deref(),
            run_now: b.run_now,
        },
    )?;
    Ok(Json(job))
}

/// `POST /reviews/{id}/{op}`: `retry`, `cancel` or `run` (start a queued one now).
pub async fn action(
    State(state): State<Arc<AppState>>,
    Path((id, op)): Path<(String, String)>,
) -> ApiResult<crate::review::Job> {
    let job = match op.as_str() {
        "retry" => state.review.retry(&state, &id)?,
        "cancel" => state.review.cancel(&id)?,
        "run" => state.review.run_now(&id)?,
        _ => return Err(ApiError::new(StatusCode::NOT_FOUND, "no such review action")),
    };
    Ok(Json(job))
}

pub async fn delete(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    if !state.review.delete(&id) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "no such review"));
    }
    Ok(Json(json!({ "deleted": id })))
}

pub async fn prompts(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.review.prompts())
}

#[derive(Deserialize)]
pub struct PromptBody {
    /// Absent: a new prompt.
    id: Option<String>,
    name: String,
    text: String,
    #[serde(default)]
    default: bool,
}

pub async fn put_prompt(State(state): State<Arc<AppState>>, Json(b): Json<PromptBody>) -> ApiResult<Value> {
    if b.text.chars().count() > 32_000 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "a prompt is at most 32000 characters"));
    }
    let id = state
        .review
        .upsert_prompt(b.id.as_deref(), &b.name, &b.text, b.default)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    let mut v = state.review.prompts();
    v["saved"] = Value::String(id);
    Ok(Json(v))
}

pub async fn delete_prompt(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    state
        .review
        .remove_prompt(&id)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(state.review.prompts()))
}
