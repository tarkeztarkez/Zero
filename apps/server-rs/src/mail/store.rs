//! Local mirror of each mailbox: threads, messages and labels in Postgres.

use crate::model::{Label, LabelColor, ParsedMessage, Tag, labels};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashMap;

pub struct NewMessage {
    pub id: String,
    pub thread_id: String,
    pub message_id_header: Option<String>,
    pub received_on: DateTime<Utc>,
    pub label_ids: Vec<String>,
    pub provider_ref: Value,
    pub data: ParsedMessage,
    pub search_text: String,
}

pub async fn upsert_message(db: &PgPool, conn: &str, m: NewMessage) -> Result<()> {
    let mut tx = db.begin().await?;
    let previous_thread: Option<String> = sqlx::query_scalar(
        "SELECT thread_id FROM messages WHERE connection_id = $1 AND id = $2",
    )
    .bind(conn)
    .bind(&m.id)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO threads (connection_id, id, latest_received_on) VALUES ($1, $2, $3)
         ON CONFLICT (connection_id, id) DO NOTHING",
    )
    .bind(conn)
    .bind(&m.thread_id)
    .bind(m.received_on)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO messages (connection_id, id, thread_id, message_id_header, received_on, label_ids, provider_ref, data, search_text)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (connection_id, id) DO UPDATE SET
           thread_id = EXCLUDED.thread_id,
           message_id_header = EXCLUDED.message_id_header,
           received_on = EXCLUDED.received_on,
           label_ids = EXCLUDED.label_ids,
           provider_ref = EXCLUDED.provider_ref,
           data = EXCLUDED.data,
           search_text = EXCLUDED.search_text",
    )
    .bind(conn)
    .bind(&m.id)
    .bind(&m.thread_id)
    .bind(&m.message_id_header)
    .bind(m.received_on)
    .bind(&m.label_ids)
    .bind(&m.provider_ref)
    .bind(serde_json::to_value(&m.data)?)
    .bind(&m.search_text)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    recompute_thread(db, conn, &m.thread_id).await?;
    if let Some(prev) = previous_thread.filter(|p| *p != m.thread_id) {
        recompute_thread(db, conn, &prev).await?;
    }
    Ok(())
}

/// Refreshes a thread's labels and date from its messages; deletes it when empty.
pub async fn recompute_thread(db: &PgPool, conn: &str, thread_id: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    let latest: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT max(received_on) FROM messages WHERE connection_id = $1 AND thread_id = $2",
    )
    .bind(conn)
    .bind(thread_id)
    .fetch_one(&mut *tx)
    .await?;
    match latest {
        None => {
            sqlx::query("DELETE FROM threads WHERE connection_id = $1 AND id = $2")
                .bind(conn)
                .bind(thread_id)
                .execute(&mut *tx)
                .await?;
        }
        Some(latest) => {
            sqlx::query(
                "UPDATE threads SET latest_received_on = $3 WHERE connection_id = $1 AND id = $2",
            )
            .bind(conn)
            .bind(thread_id)
            .bind(latest)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM thread_labels WHERE connection_id = $1 AND thread_id = $2")
                .bind(conn)
                .bind(thread_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO thread_labels (connection_id, thread_id, label_id)
                 SELECT DISTINCT $1, $2, unnest(label_ids) FROM messages
                 WHERE connection_id = $1 AND thread_id = $2
                 ON CONFLICT DO NOTHING",
            )
            .bind(conn)
            .bind(thread_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

pub async fn set_message_labels(
    db: &PgPool,
    conn: &str,
    message_id: &str,
    label_ids: &[String],
) -> Result<()> {
    let thread: Option<String> = sqlx::query_scalar(
        "UPDATE messages SET label_ids = $3 WHERE connection_id = $1 AND id = $2 RETURNING thread_id",
    )
    .bind(conn)
    .bind(message_id)
    .bind(label_ids)
    .fetch_optional(db)
    .await?;
    if let Some(t) = thread {
        recompute_thread(db, conn, &t).await?;
    }
    Ok(())
}

/// Applies a Gmail-style label change to every message of a thread.
pub async fn modify_thread_labels(
    db: &PgPool,
    conn: &str,
    thread_id: &str,
    add: &[String],
    remove: &[String],
) -> Result<()> {
    sqlx::query(
        "UPDATE messages SET label_ids = ARRAY(
            SELECT DISTINCT l FROM unnest(array_cat(label_ids, $3::text[])) AS l
            WHERE NOT (l = ANY($4::text[]))
         )
         WHERE connection_id = $1 AND thread_id = $2",
    )
    .bind(conn)
    .bind(thread_id)
    .bind(add)
    .bind(remove)
    .execute(db)
    .await?;
    recompute_thread(db, conn, thread_id).await
}

pub async fn delete_message(db: &PgPool, conn: &str, message_id: &str) -> Result<()> {
    let thread: Option<String> = sqlx::query_scalar(
        "DELETE FROM messages WHERE connection_id = $1 AND id = $2 RETURNING thread_id",
    )
    .bind(conn)
    .bind(message_id)
    .fetch_optional(db)
    .await?;
    if let Some(t) = thread {
        recompute_thread(db, conn, &t).await?;
    }
    Ok(())
}

pub async fn delete_thread(db: &PgPool, conn: &str, thread_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM threads WHERE connection_id = $1 AND id = $2")
        .bind(conn)
        .bind(thread_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn message_exists(db: &PgPool, conn: &str, id: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE connection_id = $1 AND id = $2)",
    )
    .bind(conn)
    .bind(id)
    .fetch_one(db)
    .await?)
}

/// Finds the thread of any message whose Message-ID is in `ids`.
pub async fn thread_for_message_ids(db: &PgPool, conn: &str, ids: &[String]) -> Result<Option<String>> {
    if ids.is_empty() {
        return Ok(None);
    }
    Ok(sqlx::query_scalar(
        "SELECT thread_id FROM messages WHERE connection_id = $1 AND message_id_header = ANY($2) LIMIT 1",
    )
    .bind(conn)
    .bind(ids)
    .fetch_optional(db)
    .await?)
}

#[derive(sqlx::FromRow)]
pub struct MessageRef {
    pub id: String,
    pub label_ids: Vec<String>,
    pub provider_ref: Value,
}

pub async fn thread_message_refs(db: &PgPool, conn: &str, thread_id: &str) -> Result<Vec<MessageRef>> {
    Ok(sqlx::query_as(
        "SELECT id, label_ids, provider_ref FROM messages
         WHERE connection_id = $1 AND thread_id = $2 ORDER BY received_on",
    )
    .bind(conn)
    .bind(thread_id)
    .fetch_all(db)
    .await?)
}

pub async fn message_ref(db: &PgPool, conn: &str, id: &str) -> Result<Option<MessageRef>> {
    Ok(sqlx::query_as(
        "SELECT id, label_ids, provider_ref FROM messages WHERE connection_id = $1 AND id = $2",
    )
    .bind(conn)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Resolves ids that may be message ids into thread ids.
pub async fn normalize_thread_ids(db: &PgPool, conn: &str, ids: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for id in ids {
        let thread: Option<String> = sqlx::query_scalar(
            "SELECT id FROM threads WHERE connection_id = $1 AND id = $2
             UNION ALL
             SELECT thread_id FROM messages WHERE connection_id = $1 AND id = $2
             LIMIT 1",
        )
        .bind(conn)
        .bind(id)
        .fetch_optional(db)
        .await?;
        if let Some(t) = thread {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- labels

pub async fn replace_labels(db: &PgPool, conn: &str, list: &[Label]) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM labels WHERE connection_id = $1")
        .bind(conn)
        .execute(&mut *tx)
        .await?;
    for l in list {
        sqlx::query(
            "INSERT INTO labels (connection_id, id, name, type, color) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT DO NOTHING",
        )
        .bind(conn)
        .bind(&l.id)
        .bind(&l.name)
        .bind(&l.kind)
        .bind(l.color.as_ref().map(|c| serde_json::to_value(c).unwrap()))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn list_labels(db: &PgPool, conn: &str) -> Result<Vec<Label>> {
    let rows: Vec<(String, String, String, Option<Value>)> = sqlx::query_as(
        "SELECT id, name, type, color FROM labels WHERE connection_id = $1 ORDER BY type DESC, lower(name)",
    )
    .bind(conn)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, kind, color)| Label {
            id,
            name,
            kind,
            color: color.and_then(|c| serde_json::from_value::<LabelColor>(c).ok()),
        })
        .collect())
}

async fn label_names(db: &PgPool, conn: &str) -> Result<HashMap<String, (String, String)>> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, name, type FROM labels WHERE connection_id = $1")
            .bind(conn)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|(id, name, kind)| (id, (name, kind))).collect())
}

// ---------------------------------------------------------------- queries

pub struct ListParams {
    pub folder: String,
    pub query: String,
    pub label_ids: Vec<String>,
    pub cursor: String,
    pub max_results: i64,
}

/// Lists threads of one or more mailboxes, newest first.
/// With `qualify`, ids are returned as "connection~thread" for the unified inbox.
pub async fn list_threads(db: &PgPool, conns: &[String], qualify: bool, p: &ListParams) -> Result<Value> {
    let mut b = SqlBuilder {
        sql: String::from("SELECT t.connection_id, t.id, t.latest_received_on FROM threads t WHERE t.connection_id = ANY($1)"),
        binds: vec![],
        next: 2,
    };
    let has = |label: &str| {
        format!(" AND EXISTS (SELECT 1 FROM thread_labels l WHERE l.connection_id = t.connection_id AND l.thread_id = t.id AND l.label_id = '{label}')")
    };
    let has_not = |label: &str| {
        format!(" AND NOT EXISTS (SELECT 1 FROM thread_labels l WHERE l.connection_id = t.connection_id AND l.thread_id = t.id AND l.label_id = '{label}')")
    };

    let search = SearchQuery::parse(&p.query);
    let folder = search.folder.clone().unwrap_or_else(|| p.folder.to_lowercase());
    match folder.as_str() {
        "sent" => b.sql.push_str(&has(labels::SENT)),
        "spam" => b.sql.push_str(&has(labels::SPAM)),
        "bin" | "trash" => b.sql.push_str(&has(labels::TRASH)),
        "snoozed" => b.sql.push_str(&has(labels::SNOOZED)),
        "draft" | "drafts" => b.sql.push_str(&has(labels::DRAFT)),
        "starred" => b.sql.push_str(&has(labels::STARRED)),
        "all" | "anywhere" => {}
        "archive" => {
            b.sql.push_str(&has_not(labels::INBOX));
            b.sql.push_str(&has_not(labels::TRASH));
            b.sql.push_str(&has_not(labels::SPAM));
            b.sql.push_str(" AND EXISTS (SELECT 1 FROM messages m WHERE m.connection_id = t.connection_id AND m.thread_id = t.id AND NOT ('DRAFT' = ANY(m.label_ids)))");
        }
        _ => b.sql.push_str(&has(labels::INBOX)),
    }
    if !matches!(folder.as_str(), "spam" | "bin" | "trash" | "all" | "anywhere") {
        b.sql.push_str(&has_not(labels::TRASH));
        b.sql.push_str(&has_not(labels::SPAM));
    }

    for label in p.label_ids.iter().chain(search.labels.iter()) {
        b.clause(
            " AND EXISTS (SELECT 1 FROM thread_labels l WHERE l.connection_id = t.connection_id AND l.thread_id = t.id AND l.label_id = $?)",
            BindValue::Text(label.clone()),
        );
    }
    for (field, value) in &search.fields {
        let clause = match field.as_str() {
            "from" => " AND EXISTS (SELECT 1 FROM messages m WHERE m.connection_id = t.connection_id AND m.thread_id = t.id AND (m.data->'sender'->>'email' ILIKE $? OR m.data->'sender'->>'name' ILIKE $?))",
            "to" => " AND EXISTS (SELECT 1 FROM messages m WHERE m.connection_id = t.connection_id AND m.thread_id = t.id AND (m.data->>'to' ILIKE $? OR m.data->>'cc' ILIKE $?))",
            "subject" => " AND EXISTS (SELECT 1 FROM messages m WHERE m.connection_id = t.connection_id AND m.thread_id = t.id AND (m.data->>'subject' ILIKE $? OR $? = ''))",
            _ => continue,
        };
        b.clause(clause, BindValue::Text(format!("%{value}%")));
    }
    if search.has_attachment {
        b.sql.push_str(" AND EXISTS (SELECT 1 FROM messages m WHERE m.connection_id = t.connection_id AND m.thread_id = t.id AND jsonb_array_length(m.data->'attachments') > 0)");
    }
    if let Some(after) = search.after {
        b.clause(" AND t.latest_received_on >= $?", BindValue::Time(after));
    }
    if let Some(before) = search.before {
        b.clause(" AND t.latest_received_on < $?", BindValue::Time(before));
    }
    if !search.text.is_empty() {
        b.clause(
            " AND EXISTS (SELECT 1 FROM messages m WHERE m.connection_id = t.connection_id AND m.thread_id = t.id AND m.search_text ILIKE $?)",
            BindValue::Text(format!("%{}%", search.text)),
        );
    }
    if let Some((ts, conn, id)) = parse_cursor(&p.cursor) {
        b.clause(" AND (t.latest_received_on, t.connection_id, t.id) < ($?", BindValue::Time(ts));
        b.clause(", $?", BindValue::Text(conn));
        b.clause(", $?)", BindValue::Text(id));
    }
    b.sql.push_str(&format!(
        " ORDER BY t.latest_received_on DESC, t.connection_id DESC, t.id DESC LIMIT {}",
        p.max_results
    ));
    let SqlBuilder { sql, binds, .. } = b;

    let mut q = sqlx::query_as::<_, (String, String, DateTime<Utc>)>(sqlx::AssertSqlSafe(sql)).bind(conns);
    for b in binds {
        q = match b {
            BindValue::Text(s) => q.bind(s),
            BindValue::Time(t) => q.bind(t),
        };
    }
    let rows = q.fetch_all(db).await?;

    let next_page = if rows.len() as i64 == p.max_results {
        rows.last().map(|(conn, id, ts)| format!("{}|{}|{}", ts.to_rfc3339(), conn, id))
    } else {
        None
    };
    let threads: Vec<Value> = rows
        .iter()
        .map(|(conn, id, _)| {
            let id = if qualify { format!("{conn}~{id}") } else { id.clone() };
            json!({ "id": id, "historyId": null })
        })
        .collect();
    Ok(json!({
        "threads": threads,
        "nextPageToken": next_page,
    }))
}

enum BindValue {
    Text(String),
    Time(DateTime<Utc>),
}

struct SqlBuilder {
    sql: String,
    binds: Vec<BindValue>,
    next: usize,
}

impl SqlBuilder {
    /// Appends a clause; every `$?` in it refers to the same new parameter.
    fn clause(&mut self, clause: &str, value: BindValue) {
        self.sql.push_str(&clause.replace("$?", &format!("${}", self.next)));
        self.binds.push(value);
        self.next += 1;
    }
}

fn parse_cursor(cursor: &str) -> Option<(DateTime<Utc>, String, String)> {
    let mut parts = cursor.splitn(3, '|');
    let ts = DateTime::parse_from_rfc3339(parts.next()?).ok()?.with_timezone(&Utc);
    Some((ts, parts.next()?.to_string(), parts.next()?.to_string()))
}

/// A small subset of Gmail search syntax, applied to the local mirror.
#[derive(Default, Debug)]
pub struct SearchQuery {
    pub folder: Option<String>,
    pub labels: Vec<String>,
    pub fields: Vec<(String, String)>,
    pub has_attachment: bool,
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub text: String,
}

impl SearchQuery {
    pub fn parse(q: &str) -> Self {
        let mut out = SearchQuery::default();
        let mut words = Vec::new();
        for token in tokenize(q) {
            let Some((key, value)) = token.split_once(':') else {
                words.push(token);
                continue;
            };
            let value = value.trim_matches('"').to_string();
            match key.to_lowercase().as_str() {
                "in" => out.folder = Some(value.to_lowercase()),
                "is" => match value.to_lowercase().as_str() {
                    "unread" => out.labels.push(labels::UNREAD.into()),
                    "starred" => out.labels.push(labels::STARRED.into()),
                    "important" => out.labels.push(labels::IMPORTANT.into()),
                    _ => {}
                },
                "label" => out.labels.push(value),
                "from" | "to" | "subject" => out.fields.push((key.to_lowercase(), value)),
                "has" if value == "attachment" => out.has_attachment = true,
                "after" | "newer" => out.after = parse_date(&value),
                "before" | "older" => out.before = parse_date(&value),
                _ => words.push(token),
            }
        }
        out.text = words.join(" ").trim_matches('"').to_string();
        out
    }
}

fn tokenize(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in q.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_date(v: &str) -> Option<DateTime<Utc>> {
    let v = v.replace('/', "-");
    chrono::NaiveDate::parse_from_str(&v, "%Y-%m-%d")
        .ok()
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

pub async fn get_thread(db: &PgPool, conn: &str, thread_id: &str) -> Result<Value> {
    let rows: Vec<(Value, Vec<String>, String)> = sqlx::query_as(
        "SELECT data, label_ids, thread_id FROM messages WHERE connection_id = $1 AND thread_id = $2 ORDER BY received_on",
    )
    .bind(conn)
    .bind(thread_id)
    .fetch_all(db)
    .await?;
    let names = label_names(db, conn).await?;

    let mut messages = Vec::new();
    let mut thread_labels: Vec<String> = Vec::new();
    for (data, label_ids, tid) in rows {
        let mut m: ParsedMessage = serde_json::from_value(data)?;
        m.connection_id = Some(conn.to_string());
        m.thread_id = Some(tid);
        m.unread = label_ids.iter().any(|l| l == labels::UNREAD);
        m.is_draft = label_ids.iter().any(|l| l == labels::DRAFT);
        m.tags = label_ids
            .iter()
            .map(|id| {
                let (name, kind) = names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| (id.clone(), "system".into()));
                Tag { id: id.clone(), name, kind }
            })
            .collect();
        for l in label_ids {
            if !thread_labels.contains(&l) {
                thread_labels.push(l);
            }
        }
        messages.push(m);
    }

    let non_drafts: Vec<&ParsedMessage> = messages.iter().filter(|m| !m.is_draft).collect();
    let latest = non_drafts.last().map(|m| serde_json::to_value(m)).transpose()?;
    let has_unread = thread_labels.iter().any(|l| l == labels::UNREAD);
    let total_replies = non_drafts.len();
    let is_latest_draft = messages.iter().any(|m| m.is_draft);
    let labels_out: Vec<Value> = thread_labels
        .iter()
        .map(|id| {
            let name = names.get(id).map(|(n, _)| n.clone()).unwrap_or_else(|| id.clone());
            json!({ "id": id, "name": name })
        })
        .collect();

    let mut out = json!({
        "messages": messages,
        "hasUnread": has_unread,
        "totalReplies": total_replies,
        "labels": labels_out,
        "isLatestDraft": is_latest_draft,
    });
    if let Some(latest) = latest {
        out["latest"] = latest;
    }
    Ok(out)
}

pub async fn suggest_recipients(db: &PgPool, conn: &str, query: &str, limit: i64) -> Result<Vec<Value>> {
    let pattern = format!("%{query}%");
    let rows: Vec<(Option<String>, String, i64)> = sqlx::query_as(
        "WITH people AS (
            SELECT r->>'name' AS name, lower(r->>'email') AS email
            FROM messages m, jsonb_array_elements(COALESCE(m.data->'to', '[]'::jsonb) || COALESCE(m.data->'cc', '[]'::jsonb)) r
            WHERE m.connection_id = $1 AND 'SENT' = ANY(m.label_ids)
            UNION ALL
            SELECT m.data->'sender'->>'name', lower(m.data->'sender'->>'email')
            FROM messages m WHERE m.connection_id = $1
         )
         SELECT max(name), email, count(*) AS n FROM people
         WHERE email IS NOT NULL AND (email ILIKE $2 OR name ILIKE $2)
         GROUP BY email ORDER BY n DESC LIMIT $3",
    )
    .bind(conn)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, email, _)| {
            let display = match &name {
                Some(n) if !n.is_empty() => format!("{n} <{email}>"),
                _ => email.clone(),
            };
            json!({ "email": email, "name": name, "displayText": display })
        })
        .collect())
}
