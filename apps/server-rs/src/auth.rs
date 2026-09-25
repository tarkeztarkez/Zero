//! Google sign-in and sessions, speaking the subset of the better-auth HTTP API
//! that the frontend's better-auth client uses.

use crate::crypto::{new_id, random_token};
use crate::error::{AppError, AppResult};
use crate::mail::gmail;
use crate::state::AppState;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const SESSION_COOKIE: &str = "zero_session";
const SESSION_DAYS: i64 = 30;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/sign-in/social", post(sign_in_social))
        .route("/api/auth/link-social", post(link_social))
        .route("/api/auth/callback/google", get(google_callback))
        .route("/api/auth/get-session", get(get_session))
        .route("/api/auth/sign-out", post(sign_out))
        .route("/api/public/providers", get(providers))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionUser {
    pub id: String,
    pub name: String,
    pub email: String,
    pub image: Option<String>,
    pub default_connection_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub user: SessionUser,
}

pub async fn load_session(state: &AppState, jar: &CookieJar) -> AppResult<Option<Session>> {
    let Some(token) = jar.get(SESSION_COOKIE).map(|c| c.value().to_string()) else {
        return Ok(None);
    };
    let row: Option<(String, DateTime<Utc>, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT id, expires_at, created_at, user_id FROM sessions WHERE token = $1 AND expires_at > now()",
    )
    .bind(&token)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, expires_at, created_at, user_id)) = row else {
        return Ok(None);
    };
    let user: SessionUser = sqlx::query_as(
        "SELECT id, name, email, image, default_connection_id, created_at, updated_at FROM users WHERE id = $1",
    )
    .bind(&user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Some(Session { id, token, expires_at, created_at, user }))
}

async fn get_session(State(state): State<AppState>, jar: CookieJar) -> AppResult<Json<Value>> {
    let Some(s) = load_session(&state, &jar).await? else {
        return Ok(Json(Value::Null));
    };
    Ok(Json(json!({
        "session": {
            "id": s.id,
            "token": s.token,
            "userId": s.user.id,
            "expiresAt": s.expires_at,
            "createdAt": s.created_at,
            "updatedAt": s.created_at,
        },
        "user": {
            "id": s.user.id,
            "name": s.user.name,
            "email": s.user.email,
            "emailVerified": true,
            "image": s.user.image,
            "createdAt": s.user.created_at,
            "updatedAt": s.user.updated_at,
            "defaultConnectionId": s.user.default_connection_id,
        }
    })))
}

async fn providers(State(state): State<AppState>) -> Json<Value> {
    let enabled =
        state.config.google_client_id.is_some() && state.config.google_client_secret.is_some();
    Json(json!({
        "allProviders": [{
            "id": "google",
            "name": "Google",
            "enabled": enabled,
            "required": true,
            "envVarInfo": [],
            "envVarStatus": [],
        }],
        "isProd": true,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SocialRequest {
    provider: String,
    #[serde(default)]
    callback_url: Option<String>,
}

async fn sign_in_social(
    State(state): State<AppState>,
    Json(req): Json<SocialRequest>,
) -> AppResult<Json<Value>> {
    start_oauth(&state, req, None).await
}

async fn link_social(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<SocialRequest>,
) -> AppResult<Json<Value>> {
    let session = load_session(&state, &jar).await?.ok_or(AppError::Unauthorized)?;
    start_oauth(&state, req, Some(session.user.id)).await
}

async fn start_oauth(
    state: &AppState,
    req: SocialRequest,
    link_user_id: Option<String>,
) -> AppResult<Json<Value>> {
    if req.provider != "google" {
        return Err(AppError::BadRequest(format!("unsupported provider {}", req.provider)));
    }
    let client_id = state
        .config
        .google_client_id
        .clone()
        .ok_or_else(|| AppError::BadRequest("Google sign-in is not configured".into()))?;

    let oauth_state = random_token();
    let verifier = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let callback_url = sanitize_callback(&state.config.app_url, req.callback_url.as_deref());

    sqlx::query("DELETE FROM oauth_states WHERE created_at < now() - interval '1 hour'")
        .execute(&state.db)
        .await?;
    sqlx::query(
        "INSERT INTO oauth_states (state, code_verifier, callback_url, link_user_id) VALUES ($1, $2, $3, $4)",
    )
    .bind(&oauth_state)
    .bind(&verifier)
    .bind(&callback_url)
    .bind(&link_user_id)
    .execute(&state.db)
    .await?;

    let mut url = url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri(state))
        .append_pair("response_type", "code")
        .append_pair("scope", gmail::SCOPES)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent select_account")
        .append_pair("include_granted_scopes", "true")
        .append_pair("state", &oauth_state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    Ok(Json(json!({ "url": url.to_string(), "redirect": true })))
}

fn redirect_uri(state: &AppState) -> String {
    format!("{}/api/auth/callback/google", state.config.app_url)
}

/// Only allow redirects back into our own app.
fn sanitize_callback(app_url: &str, callback: Option<&str>) -> String {
    match callback {
        Some(c) if c.starts_with('/') && !c.starts_with("//") => format!("{app_url}{c}"),
        Some(c) if c == app_url || c.starts_with(&format!("{app_url}/")) => c.to_string(),
        _ => format!("{app_url}/mail/inbox"),
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn google_callback(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    match handle_callback(&state, jar, &headers, q).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(error = %e, "google callback failed");
            let msg = urlencoding::encode(&e.to_string()).into_owned();
            Redirect::to(&format!("{}/login?error={msg}", state.config.app_url)).into_response()
        }
    }
}

async fn handle_callback(
    state: &AppState,
    jar: CookieJar,
    headers: &HeaderMap,
    q: CallbackQuery,
) -> anyhow::Result<Response> {
    if let Some(err) = q.error {
        anyhow::bail!("Google returned an error: {err}");
    }
    let code = q.code.context("missing code")?;
    let oauth_state = q.state.context("missing state")?;
    let (verifier, callback_url, link_user_id): (String, String, Option<String>) = sqlx::query_as(
        "DELETE FROM oauth_states WHERE state = $1 RETURNING code_verifier, callback_url, link_user_id",
    )
    .bind(&oauth_state)
    .fetch_optional(&state.db)
    .await?
    .context("unknown or expired sign-in attempt")?;

    let tokens = gmail::exchange_code(state, &code, &verifier, &redirect_uri(state)).await?;
    let info = gmail::user_info(state, &tokens.access_token).await?;
    let email = info.email.to_lowercase();

    let user_id = match link_user_id {
        Some(uid) => uid,
        None => {
            if !state.config.is_email_allowed(&email) {
                anyhow::bail!("{email} is not allowed to sign in");
            }
            upsert_user(state, &email, &info.name, info.picture.as_deref()).await?
        }
    };

    let connection_id =
        gmail::upsert_connection(state, &user_id, &email, &info, &tokens).await?;
    sqlx::query(
        "UPDATE users SET default_connection_id = COALESCE(default_connection_id, $2) WHERE id = $1",
    )
    .bind(&user_id)
    .bind(&connection_id)
    .execute(&state.db)
    .await?;
    state.sync_notify.notify_one();

    let jar = if jar.get(SESSION_COOKIE).is_some() && is_linking_same_user(state, &jar, &user_id).await
    {
        jar
    } else {
        let token = create_session(state, &user_id, headers).await?;
        jar.add(session_cookie(state, token))
    };
    Ok((jar, Redirect::to(&callback_url)).into_response())
}

async fn is_linking_same_user(state: &AppState, jar: &CookieJar, user_id: &str) -> bool {
    matches!(load_session(state, jar).await, Ok(Some(s)) if s.user.id == user_id)
}

async fn upsert_user(
    state: &AppState,
    email: &str,
    name: &str,
    image: Option<&str>,
) -> anyhow::Result<String> {
    let id: String = sqlx::query_scalar(
        "INSERT INTO users (id, name, email, image) VALUES ($1, $2, $3, $4)
         ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name, image = EXCLUDED.image, updated_at = now()
         RETURNING id",
    )
    .bind(new_id())
    .bind(name)
    .bind(email)
    .bind(image)
    .fetch_one(&state.db)
    .await?;
    Ok(id)
}

async fn create_session(
    state: &AppState,
    user_id: &str,
    headers: &HeaderMap,
) -> anyhow::Result<String> {
    let token = random_token();
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string());
    sqlx::query(
        "INSERT INTO sessions (id, token, user_id, expires_at, ip_address, user_agent) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(new_id())
    .bind(&token)
    .bind(user_id)
    .bind(Utc::now() + Duration::days(SESSION_DAYS))
    .bind(ip)
    .bind(user_agent)
    .execute(&state.db)
    .await?;
    Ok(token)
}

fn session_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .secure(state.config.secure_cookies())
        .same_site(SameSite::Lax)
        .max_age(time_days(SESSION_DAYS))
        .build()
}

fn time_days(days: i64) -> time::Duration {
    time::Duration::days(days)
}

async fn sign_out(State(state): State<AppState>, jar: CookieJar) -> AppResult<impl IntoResponse> {
    if let Some(c) = jar.get(SESSION_COOKIE) {
        sqlx::query("DELETE FROM sessions WHERE token = $1")
            .bind(c.value())
            .execute(&state.db)
            .await?;
    }
    let jar = jar.remove(Cookie::build(SESSION_COOKIE).path("/").build());
    Ok((StatusCode::OK, jar, Json(json!({ "success": true }))))
}
