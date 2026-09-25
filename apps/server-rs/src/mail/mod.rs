pub mod compose;
pub mod gmail;
pub mod imap;
pub mod parse;
pub mod store;

use crate::crypto::new_id;
use crate::model::{LabelColor, OutgoingMessage, Sender, labels};
use crate::state::AppState;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub enum Driver {
    Gmail(gmail::Gmail),
    Imap(imap::Imap),
}

/// One lock per mailbox so syncs and remote changes do not interleave.
pub async fn mailbox_lock(state: &AppState, conn_id: &str) -> Arc<Mutex<()>> {
    let mut locks = state.mailbox_locks.lock().await;
    locks.entry(conn_id.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
}

impl Driver {
    pub async fn load(state: &AppState, conn_id: &str) -> Result<Self> {
        let provider: String = sqlx::query_scalar("SELECT provider_id FROM connections WHERE id = $1")
            .bind(conn_id)
            .fetch_optional(&state.db)
            .await?
            .context("mailbox not found")?;
        Ok(match provider.as_str() {
            "google" => Driver::Gmail(gmail::Gmail::connect(state, conn_id).await?),
            "imap" => Driver::Imap(imap::Imap::load(state, conn_id).await?),
            other => bail!("unsupported provider {other}"),
        })
    }

    pub async fn sync(&self) -> Result<()> {
        match self {
            Driver::Gmail(g) => g.sync().await,
            Driver::Imap(i) => i.sync().await,
        }
    }

    pub async fn modify_thread(&self, thread_id: &str, add: &[String], remove: &[String]) -> Result<()> {
        match self {
            Driver::Gmail(g) => g.modify_thread(thread_id, add, remove).await,
            Driver::Imap(i) => i.modify_thread(thread_id, add, remove).await,
        }
    }

    pub async fn fetch_raw(&self, message_id: &str) -> Result<Vec<u8>> {
        match self {
            Driver::Gmail(g) => Ok(g.fetch_raw(message_id).await?.raw),
            Driver::Imap(i) => i.fetch_raw(message_id).await,
        }
    }
}

/// Updates the local mirror immediately and pushes the change upstream in the background.
pub async fn modify_threads(
    state: &AppState,
    conn_id: &str,
    thread_ids: &[String],
    add: &[String],
    remove: &[String],
) -> Result<()> {
    let thread_ids = store::normalize_thread_ids(&state.db, conn_id, thread_ids).await?;
    // IMAP moves need the message locations from before the local change.
    let mut plans = Vec::new();
    for t in &thread_ids {
        plans.push(t.clone());
        store::modify_thread_labels(&state.db, conn_id, t, add, remove).await?;
    }
    let state = state.clone();
    let conn_id = conn_id.to_string();
    let add = add.to_vec();
    let remove = remove.to_vec();
    tokio::spawn(async move {
        let lock = mailbox_lock(&state, &conn_id).await;
        let _guard = lock.lock().await;
        let result = async {
            let driver = Driver::load(&state, &conn_id).await?;
            for t in &plans {
                driver.modify_thread(t, &add, &remove).await?;
            }
            anyhow::Ok(driver)
        }
        .await;
        match result {
            Ok(Driver::Gmail(g)) => {
                for t in &plans {
                    if let Err(e) = g.sync_thread(t).await {
                        tracing::warn!(conn = %conn_id, error = %e, "resync after modify failed");
                    }
                }
            }
            Ok(Driver::Imap(_)) => {}
            Err(e) => {
                tracing::error!(conn = %conn_id, error = ?e, "applying label change upstream failed");
                state.sync_notify.notify_one();
            }
        }
    });
    Ok(())
}

pub struct ConnectionInfo {
    pub email: String,
    pub name: Option<String>,
}

pub async fn connection_info(state: &AppState, conn_id: &str) -> Result<ConnectionInfo> {
    let (email, name): (String, Option<String>) =
        sqlx::query_as("SELECT email, name FROM connections WHERE id = $1")
            .bind(conn_id)
            .fetch_one(&state.db)
            .await?;
    Ok(ConnectionInfo { email, name })
}

pub async fn send(state: &AppState, conn_id: &str, msg: &OutgoingMessage) -> Result<()> {
    let info = connection_info(state, conn_id).await?;
    let from = Sender { name: info.name.clone(), email: info.email.clone() };
    let composed = compose::build(msg, &from)?;
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => {
            g.send_raw(&composed.raw_with_bcc, msg.thread_id.as_deref()).await?;
            if let Some(d) = msg.draft_id.as_deref().filter(|d| !d.is_empty()) {
                let _ = g.delete_draft(d).await;
            }
            let g2 = gmail::Gmail::connect(state, conn_id).await?;
            let thread = msg.thread_id.clone();
            tokio::spawn(async move {
                let result = match thread {
                    Some(t) if !t.is_empty() => g2.sync_thread(&t).await,
                    _ => g2.sync().await,
                };
                if let Err(e) = result {
                    tracing::warn!(error = %e, "post-send sync failed");
                }
            });
        }
        Driver::Imap(i) => {
            i.send(&composed.message).await?;
            if let Some(d) = msg.draft_id.as_deref().filter(|d| !d.is_empty()) {
                delete_local_draft(state, conn_id, d).await?;
            }
            state.sync_notify.notify_one();
        }
    }
    Ok(())
}

pub async fn aliases(state: &AppState, conn_id: &str) -> Result<Vec<Value>> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => g.send_as().await,
        Driver::Imap(i) => Ok(vec![json!({ "email": i.email, "name": i.name, "primary": true })]),
    }
}

pub async fn message_attachments(state: &AppState, conn_id: &str, message_id: &str) -> Result<Vec<Value>> {
    let driver = Driver::load(state, conn_id).await?;
    let raw = driver.fetch_raw(message_id).await?;
    Ok(parse::attachments(&raw)
        .into_iter()
        .map(|a| {
            let mut meta = serde_json::to_value(&a.meta).unwrap();
            meta["body"] = json!(STANDARD.encode(&a.data));
            meta
        })
        .collect())
}

pub async fn raw_email(state: &AppState, conn_id: &str, message_id: &str) -> Result<String> {
    let raw = Driver::load(state, conn_id).await?.fetch_raw(message_id).await?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

// ---------------------------------------------------------------- labels

pub async fn create_label(state: &AppState, conn_id: &str, name: &str, color: Option<&LabelColor>) -> Result<()> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => {
            g.create_label(name, color).await?;
            store::replace_labels(&state.db, conn_id, &g.fetch_labels().await?).await?;
        }
        Driver::Imap(i) => {
            i.create_folder(name).await?;
            sqlx::query("INSERT INTO labels (connection_id, id, name, type) VALUES ($1, $2, $2, 'user') ON CONFLICT DO NOTHING")
                .bind(conn_id)
                .bind(name)
                .execute(&state.db)
                .await?;
        }
    }
    Ok(())
}

pub async fn update_label(state: &AppState, conn_id: &str, id: &str, name: &str, color: Option<&LabelColor>) -> Result<()> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => {
            g.update_label(id, name, color).await?;
            store::replace_labels(&state.db, conn_id, &g.fetch_labels().await?).await?;
        }
        Driver::Imap(_) => {
            // Folder renames are not supported; keep a local display name and color.
            sqlx::query("UPDATE labels SET name = $3, color = $4 WHERE connection_id = $1 AND id = $2")
                .bind(conn_id)
                .bind(id)
                .bind(name)
                .bind(color.map(|c| serde_json::to_value(c).unwrap()))
                .execute(&state.db)
                .await?;
        }
    }
    Ok(())
}

pub async fn delete_label(state: &AppState, conn_id: &str, id: &str) -> Result<()> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => g.delete_label(id).await?,
        Driver::Imap(i) => i.delete_folder(id).await?,
    }
    sqlx::query("DELETE FROM labels WHERE connection_id = $1 AND id = $2")
        .bind(conn_id)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn delete_all_spam(state: &AppState, conn_id: &str) -> Result<usize> {
    let threads: Vec<String> = sqlx::query_scalar(
        "SELECT thread_id FROM thread_labels WHERE connection_id = $1 AND label_id = 'SPAM'",
    )
    .bind(conn_id)
    .fetch_all(&state.db)
    .await?;
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => {
            for t in &threads {
                g.modify_thread(t, &[labels::TRASH.to_string()], &[labels::SPAM.to_string()]).await?;
            }
        }
        Driver::Imap(i) => {
            i.empty_spam().await?;
        }
    }
    for t in &threads {
        store::delete_thread(&state.db, conn_id, t).await?;
    }
    Ok(threads.len())
}

// ---------------------------------------------------------------- drafts

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DraftInput {
    pub to: String,
    #[serde(default)]
    pub cc: Option<String>,
    #[serde(default)]
    pub bcc: Option<String>,
    pub subject: String,
    pub message: String,
    #[serde(default)]
    pub attachments: Option<Vec<crate::model::SerializedFile>>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub from_email: Option<String>,
}

fn split_recipients(list: &str) -> Vec<Sender> {
    list.split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|r| match r.rsplit_once('<') {
            Some((name, email)) => Sender {
                name: Some(name.trim().trim_matches('"').to_string()).filter(|n| !n.is_empty()),
                email: email.trim_end_matches('>').trim().to_string(),
            },
            None => Sender { name: None, email: r.to_string() },
        })
        .collect()
}

pub async fn save_draft(state: &AppState, conn_id: &str, d: DraftInput) -> Result<Value> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => {
            let info = connection_info(state, conn_id).await?;
            let msg = OutgoingMessage {
                to: split_recipients(&d.to),
                cc: d.cc.as_deref().map(split_recipients),
                bcc: d.bcc.as_deref().map(split_recipients),
                subject: d.subject.clone(),
                message: d.message.clone(),
                attachments: d.attachments.clone().unwrap_or_default(),
                from_email: d.from_email.clone(),
                ..Default::default()
            };
            let composed = compose::build(&msg, &Sender { name: info.name, email: info.email })?;
            let id = g.save_draft(d.id.as_deref(), &composed.raw_with_bcc, d.thread_id.as_deref()).await?;
            Ok(json!({ "id": id, "success": true }))
        }
        Driver::Imap(_) => {
            let id = d.id.clone().filter(|i| !i.is_empty()).unwrap_or_else(new_id);
            let data = json!({
                "to": d.to, "cc": d.cc, "bcc": d.bcc, "subject": d.subject,
                "message": d.message, "fromEmail": d.from_email,
                "attachments": d.attachments.unwrap_or_default(),
            });
            sqlx::query(
                "INSERT INTO local_drafts (id, connection_id, thread_id, data) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data, thread_id = EXCLUDED.thread_id, updated_at = now()",
            )
            .bind(&id)
            .bind(conn_id)
            .bind(&d.thread_id)
            .bind(data)
            .execute(&state.db)
            .await?;
            Ok(json!({ "id": id, "success": true }))
        }
    }
}

fn draft_view(id: &str, to: Vec<String>, cc: Vec<String>, bcc: Vec<String>, subject: &str, content: &str, date: &str, attachments: Value) -> Value {
    json!({
        "id": id,
        "to": to,
        "cc": cc,
        "bcc": bcc,
        "subject": subject,
        "content": content,
        "rawMessage": { "internalDate": date },
        "attachments": attachments,
    })
}

fn split_list(v: &Value) -> Vec<String> {
    v.as_str()
        .map(|s| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

pub async fn get_draft(state: &AppState, conn_id: &str, id: &str) -> Result<Value> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => {
            let (draft_id, raw, _thread) = g.get_draft(id).await?;
            let parsed = parse::parse(&raw, &draft_id, None).context("unreadable draft")?;
            let m = parsed.message;
            let attachments: Vec<Value> = parse::attachments(&raw)
                .into_iter()
                .map(|a| {
                    let mut v = serde_json::to_value(&a.meta).unwrap();
                    v["body"] = json!(STANDARD.encode(&a.data));
                    v
                })
                .collect();
            Ok(draft_view(
                &draft_id,
                m.to.iter().map(parse::format_sender).collect(),
                m.cc.unwrap_or_default().iter().map(parse::format_sender).collect(),
                m.bcc.unwrap_or_default().iter().map(parse::format_sender).collect(),
                if m.subject == "(no subject)" { "" } else { &m.subject },
                m.decoded_body.as_deref().unwrap_or_default(),
                &parsed.received_on.timestamp_millis().to_string(),
                json!(attachments),
            ))
        }
        Driver::Imap(_) => {
            let (data, updated): (Value, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
                "SELECT data, updated_at FROM local_drafts WHERE connection_id = $1 AND id = $2",
            )
            .bind(conn_id)
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .context("draft not found")?;
            let attachments: Vec<Value> = data["attachments"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(i, a)| json!({
                    "attachmentId": i.to_string(), "filename": a["name"], "mimeType": a["type"],
                    "size": a["size"], "body": a["base64"], "headers": [],
                }))
                .collect();
            Ok(draft_view(
                id,
                split_list(&data["to"]),
                split_list(&data["cc"]),
                split_list(&data["bcc"]),
                data["subject"].as_str().unwrap_or_default(),
                data["message"].as_str().unwrap_or_default(),
                &updated.timestamp_millis().to_string(),
                json!(attachments),
            ))
        }
    }
}

pub async fn list_drafts(state: &AppState, conn_id: &str, max: i64, page: Option<&str>) -> Result<Value> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => {
            let (drafts, next) = g.list_drafts(max, page).await?;
            let mut out = Vec::new();
            for d in drafts {
                let Some(id) = d["id"].as_str() else { continue };
                let Ok((draft_id, raw, _)) = g.get_draft(id).await else { continue };
                let Some(parsed) = parse::parse(&raw, &draft_id, None) else { continue };
                let mut m = serde_json::to_value(&parsed.message)?;
                m["threadId"] = d["message"]["id"].clone();
                out.push((parsed.received_on, json!({ "id": draft_id, "historyId": d["message"]["id"], "$raw": m })));
            }
            out.sort_by(|a, b| b.0.cmp(&a.0));
            Ok(json!({ "threads": out.into_iter().map(|(_, v)| v).collect::<Vec<_>>(), "nextPageToken": next }))
        }
        Driver::Imap(i) => {
            let rows: Vec<(String, Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
                "SELECT id, data, updated_at FROM local_drafts WHERE connection_id = $1 ORDER BY updated_at DESC LIMIT $2",
            )
            .bind(conn_id)
            .bind(max)
            .fetch_all(&state.db)
            .await?;
            let threads: Vec<Value> = rows
                .into_iter()
                .map(|(id, data, updated)| {
                    let to: Vec<Value> = split_recipients(data["to"].as_str().unwrap_or_default())
                        .into_iter()
                        .map(|s| serde_json::to_value(s).unwrap())
                        .collect();
                    json!({
                        "id": id,
                        "historyId": null,
                        "$raw": {
                            "id": id, "title": "", "subject": data["subject"], "tags": [],
                            "sender": { "name": i.name, "email": i.email }, "to": to, "cc": null, "bcc": null,
                            "tls": false, "receivedOn": updated.to_rfc3339(), "unread": false,
                            "body": "", "processedHtml": "", "blobUrl": "", "isDraft": true,
                        }
                    })
                })
                .collect();
            Ok(json!({ "threads": threads, "nextPageToken": null }))
        }
    }
}

pub async fn delete_local_draft(state: &AppState, conn_id: &str, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM local_drafts WHERE connection_id = $1 AND id = $2")
        .bind(conn_id)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn delete_draft(state: &AppState, conn_id: &str, id: &str) -> Result<()> {
    match Driver::load(state, conn_id).await? {
        Driver::Gmail(g) => g.delete_draft(id).await,
        Driver::Imap(_) => delete_local_draft(state, conn_id, id).await,
    }
}

pub type LockMap = Mutex<HashMap<String, Arc<Mutex<()>>>>;
