//! Gmail REST API driver.

use super::parse;
use super::store::{self, NewMessage};
use crate::crypto::new_id;
use crate::model::{Label, LabelColor, labels};
use crate::state::AppState;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, TimeZone, Utc};
use futures::{StreamExt, TryStreamExt};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

pub const SCOPES: &str = "openid email profile https://www.googleapis.com/auth/gmail.modify";
const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
/// How many recent threads to mirror on first sync.
const INITIAL_THREADS: usize = 1500;
const FETCH_CONCURRENCY: usize = 8;
/// Labels that only exist locally and must not be sent to Gmail.
const LOCAL_LABELS: &[&str] = &[labels::SNOOZED, "MUTE"];

#[derive(Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Deserialize)]
pub struct UserInfo {
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub picture: Option<String>,
}

fn client_credentials(state: &AppState) -> Result<(String, String)> {
    Ok((
        state.config.google_client_id.clone().context("GOOGLE_CLIENT_ID not set")?,
        state.config.google_client_secret.clone().context("GOOGLE_CLIENT_SECRET not set")?,
    ))
}

pub async fn exchange_code(
    state: &AppState,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens> {
    let (id, secret) = client_credentials(state)?;
    let resp = state
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code),
            ("client_id", &id),
            ("client_secret", &secret),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("token exchange failed: {}", resp.text().await.unwrap_or_default());
    }
    Ok(resp.json().await?)
}

pub async fn user_info(state: &AppState, access_token: &str) -> Result<UserInfo> {
    let resp = state
        .http
        .get("https://openidconnect.googleapis.com/v1/userinfo")
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

pub async fn upsert_connection(
    state: &AppState,
    user_id: &str,
    email: &str,
    info: &UserInfo,
    tokens: &Tokens,
) -> Result<String> {
    if !tokens.scope.contains("gmail.modify") && !tokens.scope.contains("mail.google.com") {
        bail!("Gmail access was not granted. Please allow all requested permissions.");
    }
    let refresh = tokens.refresh_token.as_deref().map(|t| state.crypto.encrypt(t)).transpose()?;
    let expires_at = Utc::now() + Duration::seconds(tokens.expires_in.max(60) - 30);
    let id: String = sqlx::query_scalar(
        "INSERT INTO connections (id, user_id, email, name, picture, provider_id, access_token, refresh_token, expires_at, scope)
         VALUES ($1, $2, $3, $4, $5, 'google', $6, $7, $8, $9)
         ON CONFLICT (user_id, email) DO UPDATE SET
            name = EXCLUDED.name,
            picture = EXCLUDED.picture,
            provider_id = 'google',
            access_token = EXCLUDED.access_token,
            refresh_token = COALESCE(EXCLUDED.refresh_token, connections.refresh_token),
            expires_at = EXCLUDED.expires_at,
            scope = EXCLUDED.scope,
            updated_at = now()
         RETURNING id",
    )
    .bind(new_id())
    .bind(user_id)
    .bind(email)
    .bind(&info.name)
    .bind(&info.picture)
    .bind(&tokens.access_token)
    .bind(refresh)
    .bind(expires_at)
    .bind(&tokens.scope)
    .fetch_one(&state.db)
    .await?;
    Ok(id)
}

pub struct Gmail {
    state: AppState,
    pub conn_id: String,
    token: String,
}

impl Gmail {
    pub async fn connect(state: &AppState, conn_id: &str) -> Result<Self> {
        let (access, refresh, expires_at): (Option<String>, Option<String>, Option<DateTime<Utc>>) =
            sqlx::query_as(
                "SELECT access_token, refresh_token, expires_at FROM connections WHERE id = $1",
            )
            .bind(conn_id)
            .fetch_one(&state.db)
            .await?;
        let token = match (access, expires_at) {
            (Some(a), Some(exp)) if exp > Utc::now() => a,
            _ => {
                let refresh = refresh.context("mailbox is disconnected, please reconnect it")?;
                refresh_access_token(state, conn_id, &state.crypto.decrypt(&refresh)?).await?
            }
        };
        Ok(Self { state: state.clone(), conn_id: conn_id.to_string(), token })
    }

    async fn call(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<Value>) -> Result<Value> {
        let mut attempt = 0;
        loop {
            let mut req = self
                .state
                .http
                .request(method.clone(), format!("{API}{path}"))
                .bearer_auth(&self.token)
                .query(query);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                attempt += 1;
                if attempt <= 4 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * 2u64.pow(attempt))).await;
                    continue;
                }
            }
            if status == StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(GmailError { status: status.as_u16(), body: text }.into());
            }
            let text = resp.text().await?;
            return Ok(if text.is_empty() { Value::Null } else { serde_json::from_str(&text)? });
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.call(Method::GET, path, query, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(Method::POST, path, &[], Some(body)).await
    }

    // ------------------------------------------------------------ labels

    pub async fn fetch_labels(&self) -> Result<Vec<Label>> {
        let resp = self.get("/labels", &[]).await?;
        let mut out = Vec::new();
        for l in resp["labels"].as_array().cloned().unwrap_or_default() {
            let id = l["id"].as_str().unwrap_or_default().to_string();
            let kind = l["type"].as_str().unwrap_or("user").to_string();
            let name = l["name"].as_str().unwrap_or(&id).to_string();
            let color = l.get("color").and_then(|c| {
                Some(LabelColor {
                    background_color: c["backgroundColor"].as_str()?.to_string(),
                    text_color: c["textColor"].as_str()?.to_string(),
                })
            });
            out.push(Label { id, name, color, kind });
        }
        Ok(out)
    }

    pub async fn create_label(&self, name: &str, color: Option<&LabelColor>) -> Result<Value> {
        let mut body = json!({ "name": name, "labelListVisibility": "labelShow", "messageListVisibility": "show" });
        if let Some(c) = color.filter(|c| !c.background_color.is_empty()) {
            body["color"] = json!({ "backgroundColor": c.background_color, "textColor": c.text_color });
        }
        self.post("/labels", body).await
    }

    pub async fn update_label(&self, id: &str, name: &str, color: Option<&LabelColor>) -> Result<()> {
        let mut body = json!({ "name": name });
        if let Some(c) = color.filter(|c| !c.background_color.is_empty()) {
            body["color"] = json!({ "backgroundColor": c.background_color, "textColor": c.text_color });
        }
        self.call(Method::PATCH, &format!("/labels/{id}"), &[], Some(body)).await?;
        Ok(())
    }

    pub async fn delete_label(&self, id: &str) -> Result<()> {
        self.call(Method::DELETE, &format!("/labels/{id}"), &[], None).await?;
        Ok(())
    }

    // ------------------------------------------------------------ messages

    pub async fn fetch_raw(&self, message_id: &str) -> Result<RawMessage> {
        let v = self.get(&format!("/messages/{message_id}"), &[("format", "raw".into())]).await?;
        let raw = v["raw"].as_str().context("message without raw body")?;
        let bytes = URL_SAFE
            .decode(raw)
            .or_else(|_| URL_SAFE_NO_PAD.decode(raw.trim_end_matches('=')))?;
        Ok(RawMessage {
            id: v["id"].as_str().unwrap_or(message_id).to_string(),
            thread_id: v["threadId"].as_str().unwrap_or_default().to_string(),
            label_ids: string_array(&v["labelIds"]),
            internal_date: v["internalDate"]
                .as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
            raw: bytes,
        })
    }

    async fn message_labels(&self, message_id: &str) -> Result<Vec<String>> {
        let v = self
            .get(&format!("/messages/{message_id}"), &[("format", "minimal".into())])
            .await?;
        Ok(string_array(&v["labelIds"]))
    }

    /// Downloads and stores one message in the local mirror.
    pub async fn store_message(&self, message_id: &str) -> Result<()> {
        let raw = self.fetch_raw(message_id).await?;
        let parsed = parse::parse(&raw.raw, &raw.id, raw.internal_date)
            .with_context(|| format!("could not parse message {}", raw.id))?;
        store::upsert_message(
            &self.state.db,
            &self.conn_id,
            NewMessage {
                id: raw.id.clone(),
                thread_id: raw.thread_id.clone(),
                message_id_header: parsed.message_id_header,
                received_on: raw.internal_date.unwrap_or(parsed.received_on),
                label_ids: raw.label_ids,
                provider_ref: json!({ "gmailId": raw.id }),
                data: parsed.message,
                search_text: parsed.search_text,
            },
        )
        .await
    }

    pub async fn modify_thread(&self, thread_id: &str, add: &[String], remove: &[String]) -> Result<()> {
        let add: Vec<&String> = add.iter().filter(|l| !LOCAL_LABELS.contains(&l.as_str())).collect();
        let remove: Vec<&String> =
            remove.iter().filter(|l| !LOCAL_LABELS.contains(&l.as_str())).collect();
        if add.iter().any(|l| *l == labels::TRASH) {
            self.post(&format!("/threads/{thread_id}/trash"), json!({})).await?;
        }
        if remove.iter().any(|l| *l == labels::TRASH) {
            self.post(&format!("/threads/{thread_id}/untrash"), json!({})).await?;
        }
        let add: Vec<&String> = add.into_iter().filter(|l| *l != labels::TRASH).collect();
        let remove: Vec<&String> = remove.into_iter().filter(|l| *l != labels::TRASH).collect();
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }
        self.post(
            &format!("/threads/{thread_id}/modify"),
            json!({ "addLabelIds": add, "removeLabelIds": remove }),
        )
        .await?;
        Ok(())
    }

    pub async fn send_raw(&self, raw: &[u8], thread_id: Option<&str>) -> Result<String> {
        let mut body = json!({ "raw": URL_SAFE.encode(raw) });
        if let Some(t) = thread_id.filter(|t| !t.is_empty()) {
            body["threadId"] = json!(t);
        }
        let v = self.post("/messages/send", body).await?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    pub async fn send_as(&self) -> Result<Vec<Value>> {
        let v = self.get("/settings/sendAs", &[]).await?;
        Ok(v["sendAs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                json!({
                    "email": a["sendAsEmail"],
                    "name": a["displayName"],
                    "primary": a["isPrimary"].as_bool().unwrap_or(false),
                })
            })
            .collect())
    }

    // ------------------------------------------------------------ drafts

    pub async fn save_draft(&self, draft_id: Option<&str>, raw: &[u8], thread_id: Option<&str>) -> Result<String> {
        let mut message = json!({ "raw": URL_SAFE.encode(raw) });
        if let Some(t) = thread_id.filter(|t| !t.is_empty()) {
            message["threadId"] = json!(t);
        }
        let v = match draft_id.filter(|d| !d.is_empty()) {
            Some(id) => {
                self.call(Method::PUT, &format!("/drafts/{id}"), &[], Some(json!({ "message": message })))
                    .await?
            }
            None => self.post("/drafts", json!({ "message": message })).await?,
        };
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    pub async fn get_draft(&self, draft_id: &str) -> Result<(String, Vec<u8>, Option<String>)> {
        let v = self.get(&format!("/drafts/{draft_id}"), &[("format", "raw".into())]).await?;
        let raw = v["message"]["raw"].as_str().context("draft without body")?;
        let bytes = URL_SAFE.decode(raw).or_else(|_| URL_SAFE_NO_PAD.decode(raw.trim_end_matches('=')))?;
        Ok((
            v["id"].as_str().unwrap_or(draft_id).to_string(),
            bytes,
            v["message"]["threadId"].as_str().map(str::to_string),
        ))
    }

    pub async fn list_drafts(&self, max: i64, page_token: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
        let mut query = vec![("maxResults", max.to_string())];
        if let Some(p) = page_token.filter(|p| !p.is_empty()) {
            query.push(("pageToken", p.to_string()));
        }
        let v = self.get("/drafts", &query).await?;
        let drafts = v["drafts"].as_array().cloned().unwrap_or_default();
        Ok((drafts, v["nextPageToken"].as_str().map(str::to_string)))
    }

    pub async fn delete_draft(&self, draft_id: &str) -> Result<()> {
        self.call(Method::DELETE, &format!("/drafts/{draft_id}"), &[], None).await?;
        Ok(())
    }


    // ------------------------------------------------------------ sync

    pub async fn sync(&self) -> Result<()> {
        let labels_list = self.fetch_labels().await?;
        store::replace_labels(&self.state.db, &self.conn_id, &labels_list).await?;

        let sync_state: Value = sqlx::query_scalar("SELECT sync_state FROM connections WHERE id = $1")
            .bind(&self.conn_id)
            .fetch_one(&self.state.db)
            .await?;
        match sync_state["historyId"].as_str() {
            Some(history_id) => match self.incremental_sync(history_id).await {
                Err(e) if is_status(&e, 404) => {
                    tracing::warn!(conn = %self.conn_id, "gmail history expired, doing full sync");
                    self.full_sync().await
                }
                other => other,
            },
            None => self.full_sync().await,
        }
    }

    async fn save_history_id(&self, history_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE connections SET sync_state = jsonb_set(sync_state, '{historyId}', to_jsonb($2::text)) WHERE id = $1",
        )
        .bind(&self.conn_id)
        .bind(history_id)
        .execute(&self.state.db)
        .await?;
        Ok(())
    }

    async fn full_sync(&self) -> Result<()> {
        let profile = self.get("/profile", &[]).await?;
        let history_id = profile["historyId"].as_str().context("profile without historyId")?.to_string();

        let mut thread_ids = Vec::new();
        let mut page: Option<String> = None;
        while thread_ids.len() < INITIAL_THREADS {
            let mut query = vec![("maxResults", "500".to_string()), ("includeSpamTrash", "true".into())];
            if let Some(p) = &page {
                query.push(("pageToken", p.clone()));
            }
            let v = self.get("/threads", &query).await?;
            for t in v["threads"].as_array().cloned().unwrap_or_default() {
                if let Some(id) = t["id"].as_str() {
                    thread_ids.push(id.to_string());
                }
            }
            page = v["nextPageToken"].as_str().map(str::to_string);
            if page.is_none() {
                break;
            }
        }
        thread_ids.truncate(INITIAL_THREADS);
        tracing::info!(conn = %self.conn_id, threads = thread_ids.len(), "gmail full sync");

        futures::stream::iter(thread_ids)
            .map(|tid| async move { self.sync_thread(&tid).await })
            .buffer_unordered(FETCH_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
        self.save_history_id(&history_id).await
    }

    /// Makes the local copy of a thread match Gmail (new messages, labels, deletions).
    pub async fn sync_thread(&self, thread_id: &str) -> Result<()> {
        let v = match self
            .get(&format!("/threads/{thread_id}"), &[("format", "minimal".into())])
            .await
        {
            Err(e) if is_status(&e, 404) => {
                return store::delete_thread(&self.state.db, &self.conn_id, thread_id).await;
            }
            other => other?,
        };
        let remote = v["messages"].as_array().cloned().unwrap_or_default();
        let remote_ids: Vec<String> =
            remote.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect();
        for local in store::thread_message_refs(&self.state.db, &self.conn_id, thread_id).await? {
            if !remote_ids.contains(&local.id) {
                store::delete_message(&self.state.db, &self.conn_id, &local.id).await?;
            }
        }
        for m in remote {
            let Some(id) = m["id"].as_str() else { continue };
            if store::message_exists(&self.state.db, &self.conn_id, id).await? {
                let label_ids = merge_local_labels(
                    &self.state.db,
                    &self.conn_id,
                    id,
                    string_array(&m["labelIds"]),
                )
                .await?;
                store::set_message_labels(&self.state.db, &self.conn_id, id, &label_ids).await?;
            } else if let Err(e) = self.store_message(id).await {
                tracing::warn!(conn = %self.conn_id, message = id, error = %e, "failed to store message");
            }
        }
        Ok(())
    }

    async fn incremental_sync(&self, start_history_id: &str) -> Result<()> {
        let mut page: Option<String> = None;
        let mut latest_history = start_history_id.to_string();
        let mut touched_threads: Vec<String> = Vec::new();
        let mut deleted: Vec<String> = Vec::new();
        let mut relabeled: Vec<String> = Vec::new();
        loop {
            let mut query = vec![("startHistoryId", start_history_id.to_string()), ("maxResults", "500".into())];
            if let Some(p) = &page {
                query.push(("pageToken", p.clone()));
            }
            let v = self.get("/history", &query).await?;
            if let Some(h) = v["historyId"].as_str() {
                latest_history = h.to_string();
            }
            for h in v["history"].as_array().cloned().unwrap_or_default() {
                for added in h["messagesAdded"].as_array().into_iter().flatten() {
                    if let Some(t) = added["message"]["threadId"].as_str() {
                        if !touched_threads.iter().any(|x| x == t) {
                            touched_threads.push(t.to_string());
                        }
                    }
                }
                for d in h["messagesDeleted"].as_array().into_iter().flatten() {
                    if let Some(id) = d["message"]["id"].as_str() {
                        deleted.push(id.to_string());
                    }
                }
                for key in ["labelsAdded", "labelsRemoved"] {
                    for l in h[key].as_array().into_iter().flatten() {
                        if let Some(id) = l["message"]["id"].as_str() {
                            if !relabeled.iter().any(|x| x == id) {
                                relabeled.push(id.to_string());
                            }
                        }
                    }
                }
            }
            page = v["nextPageToken"].as_str().map(str::to_string);
            if page.is_none() {
                break;
            }
        }

        for id in &deleted {
            store::delete_message(&self.state.db, &self.conn_id, id).await?;
        }
        futures::stream::iter(touched_threads)
            .map(|tid| async move { self.sync_thread(&tid).await })
            .buffer_unordered(FETCH_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
        for id in relabeled.iter().filter(|id| !deleted.contains(id)) {
            if !store::message_exists(&self.state.db, &self.conn_id, id).await? {
                continue;
            }
            match self.message_labels(id).await {
                Ok(remote) => {
                    let label_ids = merge_local_labels(&self.state.db, &self.conn_id, id, remote).await?;
                    store::set_message_labels(&self.state.db, &self.conn_id, id, &label_ids).await?;
                }
                Err(e) if is_status(&e, 404) => {
                    store::delete_message(&self.state.db, &self.conn_id, id).await?;
                }
                Err(e) => return Err(e),
            }
        }
        self.save_history_id(&latest_history).await
    }
}

/// Keeps local-only labels (snooze, mute) when refreshing labels from Gmail.
async fn merge_local_labels(
    db: &sqlx::PgPool,
    conn: &str,
    message_id: &str,
    mut remote: Vec<String>,
) -> Result<Vec<String>> {
    if let Some(r) = store::message_ref(db, conn, message_id).await? {
        for l in r.label_ids {
            if LOCAL_LABELS.contains(&l.as_str()) && !remote.contains(&l) {
                remote.push(l);
            }
        }
    }
    Ok(remote)
}

pub struct RawMessage {
    pub id: String,
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub internal_date: Option<DateTime<Utc>>,
    pub raw: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
#[error("Gmail API error {status}: {body}")]
pub struct GmailError {
    pub status: u16,
    pub body: String,
}

fn is_status(e: &anyhow::Error, status: u16) -> bool {
    e.downcast_ref::<GmailError>().is_some_and(|g| g.status == status)
}

fn string_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

async fn refresh_access_token(state: &AppState, conn_id: &str, refresh_token: &str) -> Result<String> {
    let (id, secret) = client_credentials(state)?;
    let resp = state
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        if body.contains("invalid_grant") {
            sqlx::query("UPDATE connections SET access_token = NULL, refresh_token = NULL WHERE id = $1")
                .bind(conn_id)
                .execute(&state.db)
                .await?;
        }
        return Err(anyhow!("refreshing Google token failed: {body}"));
    }
    let tokens: Tokens = resp.json().await?;
    sqlx::query("UPDATE connections SET access_token = $2, expires_at = $3 WHERE id = $1")
        .bind(conn_id)
        .bind(&tokens.access_token)
        .bind(Utc::now() + Duration::seconds(tokens.expires_in.max(60) - 30))
        .execute(&state.db)
        .await?;
    Ok(tokens.access_token)
}
