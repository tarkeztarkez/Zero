use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    /// Public origin of the app, e.g. https://mail.example.com (frontend and API share it).
    pub app_url: String,
    pub port: u16,
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    /// 32-byte key (base64) used to encrypt refresh tokens and IMAP passwords at rest.
    pub encryption_key: [u8; 32],
    /// Only these addresses may sign in. Empty means anyone with a Google account.
    pub allowed_emails: Vec<String>,
    pub static_dir: String,
    pub sync_interval_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        use base64::Engine;
        let key_b64 = std::env::var("ENCRYPTION_KEY").context("ENCRYPTION_KEY is required")?;
        let key_bytes = base64::engine::general_purpose::STANDARD
            .decode(key_b64.trim())
            .context("ENCRYPTION_KEY must be base64")?;
        let encryption_key: [u8; 32] = key_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("ENCRYPTION_KEY must decode to 32 bytes"))?;

        Ok(Self {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
            app_url: std::env::var("APP_URL")
                .unwrap_or_else(|_| "http://localhost:3000".into())
                .trim_end_matches('/')
                .to_string(),
            port: std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000),
            google_client_id: std::env::var("GOOGLE_CLIENT_ID").ok().filter(|v| !v.is_empty()),
            google_client_secret: std::env::var("GOOGLE_CLIENT_SECRET")
                .ok()
                .filter(|v| !v.is_empty()),
            encryption_key,
            allowed_emails: std::env::var("ALLOWED_EMAILS")
                .unwrap_or_default()
                .split(',')
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
            static_dir: std::env::var("STATIC_DIR").unwrap_or_else(|_| "./public".into()),
            sync_interval_secs: std::env::var("SYNC_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        })
    }

    pub fn secure_cookies(&self) -> bool {
        self.app_url.starts_with("https://")
    }

    pub fn is_email_allowed(&self, email: &str) -> bool {
        self.allowed_emails.is_empty() || self.allowed_emails.contains(&email.to_lowercase())
    }
}
