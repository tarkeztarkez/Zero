//! tRPC procedures, grouped like apps/server/src/trpc/routes.

use super::{Ctx, Output};
use crate::crypto::new_id;
use crate::error::{AppError, AppResult};
use crate::mail::{self, imap::ImapConfig, store};
use crate::model::{LabelColor, OutgoingMessage, labels};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn arg<T: DeserializeOwned>(v: Value) -> AppResult<T> {
    let v = if v.is_null() { json!({}) } else { v };
    Ok(serde_json::from_value(v)?)
}

#[derive(Deserialize)]
struct Ids {
    ids: Vec<String>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub async fn dispatch(ctx: &Ctx, path: &str, input: Value) -> AppResult<Output> {
    let out: Output = match path {
        // ------------------------------------------------------------ mail
        "mail.listThreads" => mail_list_threads(ctx, input).await?.into(),
        "mail.get" => {
            let Id { id } = arg(input)?;
            let conn = ctx.active_connection().await?;
            store::get_thread(&ctx.state.db, &conn, &id).await?.into()
        }
        "mail.markAsRead" => relabel(ctx, input, &[], &[labels::UNREAD]).await?,
        "mail.markAsUnread" => relabel(ctx, input, &[labels::UNREAD], &[]).await?,
        "mail.markAsImportant" | "mail.bulkMarkImportant" => relabel(ctx, input, &[labels::IMPORTANT], &[]).await?,
        "mail.bulkUnmarkImportant" => relabel(ctx, input, &[], &[labels::IMPORTANT]).await?,
        "mail.bulkStar" => relabel(ctx, input, &[labels::STARRED], &[]).await?,
        "mail.bulkUnstar" => relabel(ctx, input, &[], &[labels::STARRED]).await?,
        "mail.bulkDelete" => relabel(ctx, input, &[labels::TRASH], &[labels::INBOX]).await?,
        "mail.bulkArchive" => relabel(ctx, input, &[], &[labels::INBOX]).await?,
        "mail.bulkMute" => relabel(ctx, input, &["MUTE"], &[]).await?,
        "mail.toggleStar" => toggle(ctx, input, labels::STARRED).await?,
        "mail.toggleImportant" => toggle(ctx, input, labels::IMPORTANT).await?,
        "mail.modifyLabels" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                thread_id: Vec<String>,
                #[serde(default)]
                add_labels: Vec<String>,
                #[serde(default)]
                remove_labels: Vec<String>,
            }
            let i: In = arg(input)?;
            let conn = ctx.active_connection().await?;
            if i.add_labels.is_empty() && i.remove_labels.is_empty() {
                json!({ "success": false, "error": "No label changes specified" }).into()
            } else {
                mail::modify_threads(&ctx.state, &conn, &i.thread_id, &i.add_labels, &i.remove_labels).await?;
                json!({ "success": true }).into()
            }
        }
        "mail.delete" => {
            let Id { id } = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::modify_threads(&ctx.state, &conn, &[id.clone()], &strings(&[labels::TRASH]), &[]).await?;
            store::delete_thread(&ctx.state.db, &conn, &id).await?;
            json!(true).into()
        }
        "mail.deleteAllSpam" => {
            let conn = ctx.active_connection().await?;
            match mail::delete_all_spam(&ctx.state, &conn).await {
                Ok(n) => json!({ "success": true, "message": format!("Spam emails deleted {n} threads"), "count": n }),
                Err(e) => json!({ "success": false, "message": "Failed to delete spam emails", "error": e.to_string(), "count": 0 }),
            }
            .into()
        }
        "mail.send" => mail_send(ctx, input).await?.into(),
        "mail.unsend" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                message_id: String,
            }
            let i: In = arg(input)?;
            let conn = ctx.active_connection().await?;
            let done = sqlx::query("UPDATE outbox SET status = 'cancelled' WHERE id = $1 AND connection_id = $2 AND status = 'pending'")
                .bind(&i.message_id)
                .bind(&conn)
                .execute(&ctx.state.db)
                .await?
                .rows_affected();
            if done == 0 {
                json!({ "success": false, "error": "Email was already sent" }).into()
            } else {
                json!({ "success": true }).into()
            }
        }
        "mail.getEmailAliases" => {
            let conn = ctx.active_connection().await?;
            json!(mail::aliases(&ctx.state, &conn).await?).into()
        }
        "mail.snoozeThreads" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                ids: Vec<String>,
                wake_at: String,
            }
            let i: In = arg(input)?;
            let wake: DateTime<Utc> = DateTime::parse_from_rfc3339(&i.wake_at)
                .map_err(|_| AppError::BadRequest("invalid wakeAt".into()))?
                .with_timezone(&Utc);
            if i.ids.is_empty() {
                return Ok(json!({ "success": false, "error": "No thread IDs provided" }).into());
            }
            if wake <= Utc::now() {
                return Ok(json!({ "success": false, "error": "Snooze time must be in the future" }).into());
            }
            let conn = ctx.active_connection().await?;
            let threads = store::normalize_thread_ids(&ctx.state.db, &conn, &i.ids).await?;
            mail::modify_threads(&ctx.state, &conn, &threads, &strings(&[labels::SNOOZED]), &strings(&[labels::INBOX])).await?;
            for t in &threads {
                sqlx::query("INSERT INTO snoozes (connection_id, thread_id, wake_at) VALUES ($1, $2, $3) ON CONFLICT (connection_id, thread_id) DO UPDATE SET wake_at = EXCLUDED.wake_at")
                    .bind(&conn)
                    .bind(t)
                    .bind(wake)
                    .execute(&ctx.state.db)
                    .await?;
            }
            json!({ "success": true }).into()
        }
        "mail.unsnoozeThreads" => {
            let Ids { ids } = arg(input)?;
            if ids.is_empty() {
                return Ok(json!({ "success": false, "error": "No thread IDs" }).into());
            }
            let conn = ctx.active_connection().await?;
            let threads = store::normalize_thread_ids(&ctx.state.db, &conn, &ids).await?;
            mail::modify_threads(&ctx.state, &conn, &threads, &strings(&[labels::INBOX]), &strings(&[labels::SNOOZED])).await?;
            sqlx::query("DELETE FROM snoozes WHERE connection_id = $1 AND thread_id = ANY($2)")
                .bind(&conn)
                .bind(&threads)
                .execute(&ctx.state.db)
                .await?;
            json!({ "success": true }).into()
        }
        "mail.getMessageAttachments" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                message_id: String,
            }
            let i: In = arg(input)?;
            let conn = ctx.active_connection().await?;
            json!(mail::message_attachments(&ctx.state, &conn, &i.message_id).await?).into()
        }
        "mail.processEmailContent" => {
            ctx.user()?;
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                html: String,
                should_load_images: bool,
                theme: String,
            }
            let i: In = arg(input)?;
            let p = crate::html::process(&i.html, i.should_load_images, i.theme == "dark");
            json!({ "processedHtml": p.html, "hasBlockedImages": p.has_blocked_images }).into()
        }
        "mail.getRawEmail" => {
            let Id { id } = arg(input)?;
            let conn = ctx.active_connection().await?;
            json!(mail::raw_email(&ctx.state, &conn, &id).await?).into()
        }
        "mail.verifyEmail" => {
            let Id { id } = arg(input)?;
            let conn = ctx.active_connection().await?;
            let verified = match mail::raw_email(&ctx.state, &conn, &id).await {
                Ok(raw) => is_authenticated(&raw),
                Err(_) => false,
            };
            json!({ "isVerified": verified }).into()
        }
        "mail.suggestRecipients" => {
            #[derive(Deserialize)]
            struct In {
                #[serde(default)]
                query: String,
                #[serde(default = "ten")]
                limit: i64,
            }
            fn ten() -> i64 {
                10
            }
            let i: In = arg(input)?;
            let conn = ctx.active_connection().await?;
            json!(store::suggest_recipients(&ctx.state.db, &conn, &i.query, i.limit).await?).into()
        }
        "mail.forceSync" => {
            let conn = ctx.active_connection().await?;
            sqlx::query("UPDATE connections SET sync_state = '{}'::jsonb WHERE id = $1")
                .bind(&conn)
                .execute(&ctx.state.db)
                .await?;
            let state = ctx.state.clone();
            tokio::spawn(async move { crate::jobs::sync_connection(&state, &conn).await });
            json!({ "success": true }).into()
        }

        // ------------------------------------------------------------ labels
        "labels.list" => {
            let conn = ctx.active_connection().await?;
            let list = store::list_labels(&ctx.state.db, &conn).await?;
            json!(list).into()
        }
        "labels.create" => {
            #[derive(Deserialize)]
            struct In {
                name: String,
                #[serde(default)]
                color: Option<LabelColor>,
            }
            let i: In = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::create_label(&ctx.state, &conn, &i.name, i.color.as_ref()).await?;
            Value::Null.into()
        }
        "labels.update" => {
            #[derive(Deserialize)]
            struct In {
                id: String,
                name: String,
                #[serde(default)]
                color: Option<LabelColor>,
            }
            let i: In = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::update_label(&ctx.state, &conn, &i.id, &i.name, i.color.as_ref()).await?;
            Value::Null.into()
        }
        "labels.delete" => {
            let Id { id } = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::delete_label(&ctx.state, &conn, &id).await?;
            Value::Null.into()
        }

        // ------------------------------------------------------------ drafts
        "drafts.create" => {
            let d: mail::DraftInput = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::save_draft(&ctx.state, &conn, d).await?.into()
        }
        "drafts.get" => {
            let Id { id } = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::get_draft(&ctx.state, &conn, &id).await?.into()
        }
        "drafts.list" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                #[serde(default)]
                max_results: Option<i64>,
                #[serde(default)]
                page_token: Option<String>,
            }
            let i: In = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::list_drafts(&ctx.state, &conn, i.max_results.unwrap_or(20), i.page_token.as_deref()).await?.into()
        }
        "drafts.delete" => {
            let Id { id } = arg(input)?;
            let conn = ctx.active_connection().await?;
            mail::delete_draft(&ctx.state, &conn, &id).await?;
            json!(true).into()
        }

        // ------------------------------------------------------------ connections
        "connections.list" => connections_list(ctx).await?,
        "connections.getDefault" => {
            if ctx.session.is_none() {
                return Ok(Value::Null.into());
            }
            let conn = ctx.active_connection().await?;
            let row = connection_row(ctx, &conn).await?;
            Output::with_dates(row, vec!["createdAt".into()])
        }
        "connections.setDefault" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                connection_id: String,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            let updated = sqlx::query(
                "UPDATE users SET default_connection_id = $2 WHERE id = $1 AND EXISTS (SELECT 1 FROM connections WHERE id = $2 AND user_id = $1)",
            )
            .bind(&user.id)
            .bind(&i.connection_id)
            .execute(&ctx.state.db)
            .await?
            .rows_affected();
            if updated == 0 {
                return Err(AppError::NotFound("connection not found".into()));
            }
            Value::Null.into()
        }
        "connections.delete" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                connection_id: String,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            sqlx::query("DELETE FROM connections WHERE id = $1 AND user_id = $2")
                .bind(&i.connection_id)
                .bind(&user.id)
                .execute(&ctx.state.db)
                .await?;
            sqlx::query("UPDATE users SET default_connection_id = NULL WHERE id = $1 AND default_connection_id = $2")
                .bind(&user.id)
                .bind(&i.connection_id)
                .execute(&ctx.state.db)
                .await?;
            Value::Null.into()
        }
        "connections.addImap" => add_imap(ctx, input).await?.into(),

        // ------------------------------------------------------------ settings
        "settings.get" => {
            let Some(session) = &ctx.session else {
                return Ok(json!({ "settings": default_settings() }).into());
            };
            let stored: Option<Value> = sqlx::query_scalar("SELECT settings FROM user_settings WHERE user_id = $1")
                .bind(&session.user.id)
                .fetch_optional(&ctx.state.db)
                .await?;
            let mut settings = default_settings();
            if let (Some(Value::Object(stored)), Value::Object(base)) = (stored, &mut settings) {
                base.extend(stored);
            }
            json!({ "settings": settings }).into()
        }
        "settings.save" => {
            let user = ctx.user()?;
            let Value::Object(patch) = input else {
                return Err(AppError::BadRequest("settings must be an object".into()));
            };
            let stored: Option<Value> = sqlx::query_scalar("SELECT settings FROM user_settings WHERE user_id = $1")
                .bind(&user.id)
                .fetch_optional(&ctx.state.db)
                .await?;
            let mut settings = stored.unwrap_or_else(default_settings);
            if let Value::Object(s) = &mut settings {
                s.extend(patch);
            }
            sqlx::query(
                "INSERT INTO user_settings (user_id, settings) VALUES ($1, $2)
                 ON CONFLICT (user_id) DO UPDATE SET settings = EXCLUDED.settings, updated_at = now()",
            )
            .bind(&user.id)
            .bind(&settings)
            .execute(&ctx.state.db)
            .await?;
            json!({ "success": true }).into()
        }

        // ------------------------------------------------------------ user
        "user.delete" => {
            let user = ctx.user()?;
            sqlx::query("DELETE FROM users WHERE id = $1").bind(&user.id).execute(&ctx.state.db).await?;
            json!({ "success": true, "message": "User deleted" }).into()
        }
        "user.getIntercomToken" => {
            ctx.user()?;
            json!("").into()
        }

        // ------------------------------------------------------------ templates
        "templates.list" => {
            let user = ctx.user()?;
            let rows: Vec<Value> = sqlx::query_scalar(
                r#"SELECT jsonb_build_object('id', id, 'userId', user_id, 'name', name, 'subject', subject, 'body', body,
                    'to', "to", 'cc', cc, 'bcc', bcc, 'createdAt', created_at, 'updatedAt', updated_at)
                   FROM email_templates WHERE user_id = $1 ORDER BY updated_at DESC"#,
            )
            .bind(&user.id)
            .fetch_all(&ctx.state.db)
            .await?;
            let dates = date_paths("templates", rows.len(), &["createdAt", "updatedAt"]);
            Output::with_dates(json!({ "templates": rows }), dates)
        }
        "templates.create" => {
            #[derive(Deserialize)]
            struct In {
                name: String,
                #[serde(default)]
                subject: String,
                #[serde(default)]
                body: String,
                #[serde(default)]
                to: Option<Vec<String>>,
                #[serde(default)]
                cc: Option<Vec<String>>,
                #[serde(default)]
                bcc: Option<Vec<String>>,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            let row: Value = sqlx::query_scalar(
                r#"INSERT INTO email_templates (id, user_id, name, subject, body, "to", cc, bcc) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                   RETURNING jsonb_build_object('id', id, 'userId', user_id, 'name', name, 'subject', subject, 'body', body,
                    'to', "to", 'cc', cc, 'bcc', bcc, 'createdAt', created_at, 'updatedAt', updated_at)"#,
            )
            .bind(new_id())
            .bind(&user.id)
            .bind(&i.name)
            .bind(&i.subject)
            .bind(&i.body)
            .bind(json!(i.to))
            .bind(json!(i.cc))
            .bind(json!(i.bcc))
            .fetch_one(&ctx.state.db)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(d) if d.is_unique_violation() => {
                    AppError::BadRequest("A template with this name already exists".into())
                }
                _ => e.into(),
            })?;
            Output::with_dates(json!({ "template": row }), vec!["template.createdAt".into(), "template.updatedAt".into()])
        }
        "templates.delete" => {
            let Id { id } = arg(input)?;
            let user = ctx.user()?;
            sqlx::query("DELETE FROM email_templates WHERE id = $1 AND user_id = $2")
                .bind(&id)
                .bind(&user.id)
                .execute(&ctx.state.db)
                .await?;
            json!({ "success": true }).into()
        }

        // ------------------------------------------------------------ notes
        "notes.list" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                thread_id: String,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT {NOTE_JSON} FROM notes WHERE user_id = $1 AND thread_id = $2 ORDER BY is_pinned DESC, \"order\", created_at DESC"
            )))
            .bind(&user.id)
            .bind(&i.thread_id)
            .fetch_all(&ctx.state.db)
            .await?;
            let dates = date_paths("notes", rows.len(), &["createdAt", "updatedAt"]);
            Output::with_dates(json!({ "notes": rows }), dates)
        }
        "notes.create" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                thread_id: String,
                content: String,
                #[serde(default)]
                color: Option<String>,
                #[serde(default)]
                is_pinned: Option<bool>,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            let row: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "INSERT INTO notes (id, user_id, thread_id, content, color, is_pinned, \"order\")
                 VALUES ($1, $2, $3, $4, $5, $6, (SELECT COALESCE(max(\"order\"), -1) + 1 FROM notes WHERE user_id = $2 AND thread_id = $3))
                 RETURNING {NOTE_JSON}"
            )))
            .bind(new_id())
            .bind(&user.id)
            .bind(&i.thread_id)
            .bind(&i.content)
            .bind(i.color.unwrap_or_else(|| "default".into()))
            .bind(i.is_pinned.unwrap_or(false))
            .fetch_one(&ctx.state.db)
            .await?;
            Output::with_dates(json!({ "note": row }), vec!["note.createdAt".into(), "note.updatedAt".into()])
        }
        "notes.update" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Data {
                content: Option<String>,
                color: Option<String>,
                is_pinned: Option<bool>,
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                note_id: String,
                data: Data,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            let row: Option<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "UPDATE notes SET content = COALESCE($3, content), color = COALESCE($4, color),
                   is_pinned = COALESCE($5, is_pinned), updated_at = now()
                 WHERE id = $1 AND user_id = $2 RETURNING {NOTE_JSON}"
            )))
            .bind(&i.note_id)
            .bind(&user.id)
            .bind(i.data.content)
            .bind(i.data.color)
            .bind(i.data.is_pinned)
            .fetch_optional(&ctx.state.db)
            .await?;
            let row = row.ok_or_else(|| AppError::NotFound("note not found".into()))?;
            Output::with_dates(json!({ "note": row }), vec!["note.createdAt".into(), "note.updatedAt".into()])
        }
        "notes.delete" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct In {
                note_id: String,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            let n = sqlx::query("DELETE FROM notes WHERE id = $1 AND user_id = $2")
                .bind(&i.note_id)
                .bind(&user.id)
                .execute(&ctx.state.db)
                .await?
                .rows_affected();
            json!({ "success": n > 0 }).into()
        }
        "notes.reorder" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Item {
                id: String,
                order: i32,
                #[serde(default)]
                is_pinned: Option<bool>,
            }
            #[derive(Deserialize)]
            struct In {
                notes: Vec<Item>,
            }
            let i: In = arg(input)?;
            let user = ctx.user()?;
            for n in i.notes {
                sqlx::query("UPDATE notes SET \"order\" = $3, is_pinned = COALESCE($4, is_pinned), updated_at = now() WHERE id = $1 AND user_id = $2")
                    .bind(&n.id)
                    .bind(&user.id)
                    .bind(n.order)
                    .bind(n.is_pinned)
                    .execute(&ctx.state.db)
                    .await?;
            }
            json!({ "success": true }).into()
        }

        // ------------------------------------------------------------ small or disabled features
        "categories.defaults" => default_settings()["categories"].clone().into(),
        "cookiePreferences.getPreferences" => json!({ "necessary": true, "functional": true, "analytics": false, "marketing": false }).into(),
        "cookiePreferences.updatePreferences" | "cookiePreferences.setLocaleCookie" => json!({ "success": true }).into(),
        "bimi.getByEmail" | "bimi.getByDomain" => {
            #[derive(Deserialize)]
            struct In {
                #[serde(default)]
                email: Option<String>,
                #[serde(default)]
                domain: Option<String>,
            }
            let i: In = arg(input)?;
            let domain = i
                .domain
                .or_else(|| i.email.and_then(|e| e.rsplit_once('@').map(|(_, d)| d.to_string())))
                .unwrap_or_default();
            json!({ "domain": domain, "bimiRecord": null, "logo": null }).into()
        }
        "brain.getState" => json!({ "enabled": false }).into(),
        "brain.getLabels" => json!([]).into(),
        "brain.getPrompts" => json!({ "SummarizeMessage": "", "ReSummarizeThread": "", "SummarizeThread": "", "Chat": "", "Compose": "" }).into(),
        "brain.generateSummary" => Value::Null.into(),
        "brain.enableBrain" | "brain.disableBrain" => json!(false).into(),
        "brain.updatePrompt" | "brain.updateLabels" => json!({ "success": true }).into(),
        p if p.starts_with("logging.") => json!({ "success": true }).into(),
        p if p.starts_with("ai.") || p.starts_with("meet.") => {
            return Err(AppError::NotImplemented("AI features are not available on this server".into()));
        }
        other => return Err(AppError::NotFound(format!("No procedure found on path \"{other}\""))),
    };
    Ok(out)
}

const NOTE_JSON: &str = "jsonb_build_object('id', id, 'userId', user_id, 'threadId', thread_id, 'content', content, 'color', color, 'isPinned', is_pinned, 'order', \"order\", 'createdAt', created_at, 'updatedAt', updated_at)";

fn date_paths(prefix: &str, n: usize, fields: &[&str]) -> Vec<String> {
    (0..n).flat_map(|i| fields.iter().map(move |f| format!("{prefix}.{i}.{f}"))).collect()
}

async fn relabel(ctx: &Ctx, input: Value, add: &[&str], remove: &[&str]) -> AppResult<Output> {
    let Ids { ids } = arg(input)?;
    let conn = ctx.active_connection().await?;
    mail::modify_threads(&ctx.state, &conn, &ids, &strings(add), &strings(remove)).await?;
    Ok(json!({ "success": true }).into())
}

/// Adds the label to all threads unless one of them already has it, in which case it removes it.
async fn toggle(ctx: &Ctx, input: Value, label: &str) -> AppResult<Output> {
    let Ids { ids } = arg(input)?;
    let conn = ctx.active_connection().await?;
    let threads = store::normalize_thread_ids(&ctx.state.db, &conn, &ids).await?;
    if threads.is_empty() {
        return Ok(json!({ "success": false, "error": "No thread IDs provided" }).into());
    }
    let any: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM thread_labels WHERE connection_id = $1 AND thread_id = ANY($2) AND label_id = $3)",
    )
    .bind(&conn)
    .bind(&threads)
    .bind(label)
    .fetch_one(&ctx.state.db)
    .await?;
    let (add, remove) = if any { (vec![], strings(&[label])) } else { (strings(&[label]), vec![]) };
    mail::modify_threads(&ctx.state, &conn, &threads, &add, &remove).await?;
    Ok(json!({ "success": true }).into())
}

async fn mail_list_threads(ctx: &Ctx, input: Value) -> AppResult<Value> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct In {
        #[serde(default)]
        folder: Option<String>,
        #[serde(default)]
        q: Option<String>,
        #[serde(default)]
        max_results: Option<i64>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        label_ids: Option<Vec<String>>,
    }
    let i: In = arg(input)?;
    let conn = ctx.active_connection().await?;
    let folder = i.folder.unwrap_or_else(|| "inbox".into());
    if folder == "draft" {
        return Ok(mail::list_drafts(&ctx.state, &conn, i.max_results.unwrap_or(20), i.cursor.as_deref()).await?);
    }
    let params = store::ListParams {
        folder,
        query: i.q.unwrap_or_default(),
        label_ids: i.label_ids.unwrap_or_default(),
        cursor: i.cursor.unwrap_or_default(),
        max_results: i.max_results.unwrap_or(20).clamp(1, 100),
    };
    Ok(store::list_threads(&ctx.state.db, &conn, &params).await?)
}

async fn mail_send(ctx: &Ctx, input: Value) -> AppResult<Value> {
    let msg: OutgoingMessage = arg(input)?;
    let conn = ctx.active_connection().await?;
    let user = ctx.user()?;
    let undo_send: bool = sqlx::query_scalar::<_, Option<bool>>(
        "SELECT (settings->>'undoSendEnabled')::boolean FROM user_settings WHERE user_id = $1",
    )
    .bind(&user.id)
    .fetch_optional(&ctx.state.db)
    .await?
    .flatten()
    .unwrap_or(false);

    let scheduled_at = match msg.schedule_at.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => {
            let t = DateTime::parse_from_rfc3339(s)
                .map_err(|_| AppError::BadRequest("Invalid schedule date format".into()))?
                .with_timezone(&Utc);
            if t <= Utc::now() {
                return Ok(json!({ "success": false, "error": "Schedule time must be in the future" }));
            }
            Some(t)
        }
        None if undo_send => Some(Utc::now() + chrono::Duration::seconds(15)),
        None => None,
    };

    if let Some(send_at) = scheduled_at {
        let id = new_id();
        sqlx::query("INSERT INTO outbox (id, connection_id, payload, send_at) VALUES ($1, $2, $3, $4)")
            .bind(&id)
            .bind(&conn)
            .bind(serde_json::to_value(&msg).map_err(anyhow::Error::from)?)
            .bind(send_at)
            .execute(&ctx.state.db)
            .await?;
        let long_term = msg.schedule_at.is_some();
        return Ok(json!({
            "success": true,
            "queued": !long_term,
            "scheduled": long_term,
            "messageId": id,
            "sendAt": send_at.timestamp_millis(),
        }));
    }

    mail::send(&ctx.state, &conn, &msg).await?;
    Ok(json!({ "success": true }))
}

async fn connection_row(ctx: &Ctx, conn: &str) -> AppResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'email', email, 'name', name, 'picture', picture,
            'createdAt', created_at, 'providerId', provider_id)
         FROM connections WHERE id = $1",
    )
    .bind(conn)
    .fetch_one(&ctx.state.db)
    .await?)
}

async fn connections_list(ctx: &Ctx) -> AppResult<Output> {
    let user = ctx.user()?;
    let rows: Vec<(Value, bool)> = sqlx::query_as(
        "SELECT jsonb_build_object('id', id, 'email', email, 'name', name, 'picture', picture,
            'createdAt', created_at, 'providerId', provider_id),
            CASE WHEN provider_id = 'imap' THEN secret IS NULL ELSE refresh_token IS NULL END
         FROM connections WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(&user.id)
    .fetch_all(&ctx.state.db)
    .await?;
    let disconnected: Vec<Value> = rows.iter().filter(|(_, d)| *d).map(|(r, _)| r["id"].clone()).collect();
    let connections: Vec<Value> = rows.into_iter().map(|(r, _)| r).collect();
    let dates = date_paths("connections", connections.len(), &["createdAt"]);
    Ok(Output::with_dates(json!({ "connections": connections, "disconnectedIds": disconnected }), dates))
}

async fn add_imap(ctx: &Ctx, input: Value) -> AppResult<Value> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct In {
        email: String,
        #[serde(default)]
        name: Option<String>,
        password: String,
        #[serde(flatten)]
        config: ImapConfig,
    }
    let i: In = arg(input)?;
    let user = ctx.user()?;
    mail::imap::test_settings(&i.config, &i.password)
        .await
        .map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    let secret = ctx.state.crypto.encrypt(&i.password)?;
    let id: String = sqlx::query_scalar(
        "INSERT INTO connections (id, user_id, email, name, provider_id, imap_config, secret, scope)
         VALUES ($1, $2, $3, $4, 'imap', $5, $6, 'imap')
         ON CONFLICT (user_id, email) DO UPDATE SET name = EXCLUDED.name, provider_id = 'imap',
            imap_config = EXCLUDED.imap_config, secret = EXCLUDED.secret, sync_state = '{}'::jsonb, updated_at = now()
         RETURNING id",
    )
    .bind(new_id())
    .bind(&user.id)
    .bind(i.email.trim().to_lowercase())
    .bind(i.name.filter(|n| !n.is_empty()))
    .bind(serde_json::to_value(&i.config).map_err(anyhow::Error::from)?)
    .bind(secret)
    .fetch_one(&ctx.state.db)
    .await?;
    ctx.state.sync_notify.notify_one();
    Ok(json!({ "success": true, "connectionId": id }))
}

/// True when the receiving server recorded passing DKIM or SPF plus DMARC.
fn is_authenticated(raw: &str) -> bool {
    let header_end = raw.find("\r\n\r\n").or_else(|| raw.find("\n\n")).unwrap_or(raw.len());
    let headers = raw[..header_end].to_lowercase();
    let auth: Vec<&str> = headers
        .split("\nauthentication-results:")
        .skip(1)
        .collect();
    auth.iter().any(|a| a.contains("dmarc=pass") && (a.contains("dkim=pass") || a.contains("spf=pass")))
}

fn default_settings() -> Value {
    json!({
        "language": "en",
        "timezone": "Europe/Warsaw",
        "dynamicContent": false,
        "externalImages": true,
        "customPrompt": "",
        "trustedSenders": [],
        "isOnboarded": true,
        "colorTheme": "system",
        "zeroSignature": false,
        "autoRead": true,
        "defaultEmailAlias": "",
        "categories": [
            { "id": "Important", "name": "Important", "searchValue": "IMPORTANT", "order": 0, "icon": "Lightning", "isDefault": false },
            { "id": "All Mail", "name": "All Mail", "searchValue": "", "order": 1, "icon": "Mail", "isDefault": true },
            { "id": "Unread", "name": "Unread", "searchValue": "UNREAD", "order": 5, "icon": "ScanEye", "isDefault": false }
        ],
        "undoSendEnabled": false,
        "imageCompression": "medium",
        "animations": false
    })
}
