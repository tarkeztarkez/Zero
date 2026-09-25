//! tRPC v11 HTTP protocol (httpBatchLink + superjson) on top of axum.

mod procs;

use crate::auth::{self, Session};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::CookieJar;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/trpc/{*path}", get(handle_get).post(handle_post))
}

pub struct Ctx {
    pub state: AppState,
    pub session: Option<Session>,
}

impl Ctx {
    pub fn user(&self) -> AppResult<&auth::SessionUser> {
        self.session.as_ref().map(|s| &s.user).ok_or(AppError::Unauthorized)
    }

    /// The mailbox the user is currently looking at.
    pub async fn active_connection(&self) -> AppResult<String> {
        let user = self.user()?;
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM connections WHERE user_id = $1
             ORDER BY (id = $2) DESC, created_at LIMIT 1",
        )
        .bind(&user.id)
        .bind(user.default_connection_id.as_deref().unwrap_or(""))
        .fetch_optional(&self.state.db)
        .await?;
        id.ok_or_else(|| AppError::NotFound("No email connections".into()))
    }
}

/// A procedure result: the JSON payload plus optional superjson metadata.
pub struct Output {
    pub json: Value,
    pub meta: Option<Value>,
}

impl From<Value> for Output {
    fn from(json: Value) -> Self {
        Output { json, meta: None }
    }
}

impl Output {
    /// Marks string fields at the given dotted paths as Dates for superjson.
    pub fn with_dates(json: Value, paths: Vec<String>) -> Self {
        if paths.is_empty() {
            return Output { json, meta: None };
        }
        let values: Map<String, Value> = paths.into_iter().map(|p| (p, json!(["Date"]))).collect();
        Output { json, meta: Some(json!({ "values": values })) }
    }
}

async fn handle_get(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let batch = query.get("batch").is_some_and(|b| b == "1" || b == "true");
    let input: Value = query
        .get("input")
        .and_then(|i| serde_json::from_str(i).ok())
        .unwrap_or(Value::Null);
    run(state, jar, &path, batch, input).await
}

async fn handle_post(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let batch = query.get("batch").is_some_and(|b| b == "1" || b == "true");
    let input: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
    run(state, jar, &path, batch, input).await
}

async fn run(state: AppState, jar: CookieJar, path: &str, batch: bool, input: Value) -> Response {
    let session = match auth::load_session(&state, &jar).await {
        Ok(s) => s,
        Err(e) => return error_response(&e, path),
    };
    let ctx = Ctx { state, session };
    let paths: Vec<&str> = path.split(',').collect();

    let calls = paths.iter().enumerate().map(|(i, p)| {
        let raw = if batch { input.get(i.to_string()).cloned().unwrap_or(Value::Null) } else { input.clone() };
        procs::dispatch(&ctx, p, unwrap_superjson(raw))
    });
    let outcomes = futures::future::join_all(calls).await;

    let mut results = Vec::with_capacity(paths.len());
    let mut statuses = Vec::with_capacity(paths.len());
    for (p, outcome) in paths.iter().zip(outcomes) {
        match outcome {
            Ok(out) => {
                let mut data = json!({ "json": out.json });
                if let Some(meta) = out.meta {
                    data["meta"] = meta;
                }
                results.push(json!({ "result": { "data": data } }));
                statuses.push(200);
            }
            Err(e) => {
                let (body, status) = e.to_trpc(p);
                results.push(body);
                statuses.push(status);
            }
        }
    }

    let status = if statuses.iter().all(|s| *s == statuses[0]) { statuses[0] } else { 207 };
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let body = if batch { Value::Array(results) } else { results.into_iter().next().unwrap_or(Value::Null) };
    (status, axum::Json(body)).into_response()
}

fn error_response(e: &AppError, path: &str) -> Response {
    let (body, status) = e.to_trpc(path);
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(json!([body]))).into_response()
}

/// Takes `{ json, meta }` from the client and returns the plain value.
/// Values superjson marks as `undefined` are simply absent, which serde treats as None.
fn unwrap_superjson(v: Value) -> Value {
    match v {
        Value::Object(mut m) if m.contains_key("json") => m.remove("json").unwrap_or(Value::Null),
        other => other,
    }
}
