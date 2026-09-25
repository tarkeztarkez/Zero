//! Background work: mailbox sync, delayed sends and snooze wake-ups.

use crate::mail::{self, Driver};
use crate::model::{OutgoingMessage, labels};
use crate::state::AppState;
use anyhow::Result;
use std::time::Duration;

pub fn spawn_all(state: AppState) {
    tokio::spawn(sync_loop(state.clone()));
    tokio::spawn(outbox_loop(state.clone()));
    tokio::spawn(snooze_loop(state));
}

async fn sync_loop(state: AppState) {
    loop {
        let conns: Vec<String> = match sqlx::query_scalar(
            "SELECT id FROM connections WHERE (provider_id = 'google' AND refresh_token IS NOT NULL)
                OR (provider_id = 'imap' AND secret IS NOT NULL)",
        )
        .fetch_all(&state.db)
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "listing connections failed");
                vec![]
            }
        };
        for conn in conns {
            let state = state.clone();
            tokio::spawn(async move { sync_connection(&state, &conn).await });
        }
        let interval = Duration::from_secs(state.config.sync_interval_secs);
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = state.sync_notify.notified() => {}
        }
    }
}

/// Syncs one mailbox unless a sync or change for it is already running.
pub async fn sync_connection(state: &AppState, conn: &str) {
    let lock = mail::mailbox_lock(state, conn).await;
    let Ok(_guard) = lock.try_lock() else { return };
    let result = async { Driver::load(state, conn).await?.sync().await }.await;
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    if let Some(e) = &error {
        tracing::warn!(conn, error = %e, "sync failed");
    }
    let _ = sqlx::query("UPDATE connections SET last_sync_at = now(), last_sync_error = $2 WHERE id = $1")
        .bind(conn)
        .bind(error)
        .execute(&state.db)
        .await;
}

async fn outbox_loop(state: AppState) {
    loop {
        if let Err(e) = send_due(&state).await {
            tracing::error!(error = ?e, "outbox processing failed");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn send_due(state: &AppState) -> Result<()> {
    let due: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "UPDATE outbox SET status = 'sending' WHERE id IN (
            SELECT id FROM outbox WHERE status = 'pending' AND send_at <= now() LIMIT 10 FOR UPDATE SKIP LOCKED
         ) RETURNING id, connection_id, payload",
    )
    .fetch_all(&state.db)
    .await?;
    for (id, conn, payload) in due {
        let result = async {
            let msg: OutgoingMessage = serde_json::from_value(payload)?;
            mail::send(state, &conn, &msg).await
        }
        .await;
        let (status, error) = match &result {
            Ok(()) => ("sent", None),
            Err(e) => {
                tracing::error!(outbox = %id, error = ?e, "scheduled send failed");
                ("failed", Some(format!("{e:#}")))
            }
        };
        sqlx::query("UPDATE outbox SET status = $2, error = $3 WHERE id = $1")
            .bind(&id)
            .bind(status)
            .bind(error)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

async fn snooze_loop(state: AppState) {
    loop {
        if let Err(e) = wake_snoozed(&state).await {
            tracing::error!(error = ?e, "waking snoozed threads failed");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn wake_snoozed(state: &AppState) -> Result<()> {
    let due: Vec<(String, String)> = sqlx::query_as(
        "DELETE FROM snoozes WHERE wake_at <= now() RETURNING connection_id, thread_id",
    )
    .fetch_all(&state.db)
    .await?;
    for (conn, thread) in due {
        mail::modify_threads(
            state,
            &conn,
            &[thread],
            &[labels::INBOX.to_string(), labels::UNREAD.to_string()],
            &[labels::SNOOZED.to_string()],
        )
        .await?;
    }
    Ok(())
}
