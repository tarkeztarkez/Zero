use crate::config::Config;
use crate::crypto::Crypto;
use crate::mail::LockMap;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::Notify;

pub type AppState = Arc<State>;

pub struct State {
    pub db: PgPool,
    pub config: Config,
    pub crypto: Crypto,
    pub http: reqwest::Client,
    /// Wakes the sync loop early (new connection, manual resync).
    pub sync_notify: Notify,
    pub mailbox_locks: LockMap,
}
