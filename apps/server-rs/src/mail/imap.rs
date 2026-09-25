//! Generic IMAP + SMTP driver.
//!
//! IMAP folders are mapped onto Gmail-style labels (INBOX, SENT, TRASH, SPAM, DRAFT,
//! other folders become user labels) and flags onto UNREAD/STARRED, so the rest of the
//! app can treat every mailbox the same way.

use super::parse;
use super::store::{self, NewMessage};
use crate::model::{Label, labels};
use crate::state::AppState;
use anyhow::{Context, Result, anyhow, bail};
use async_imap::types::{Flag, NameAttribute};
use chrono::Utc;
use futures::TryStreamExt;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Messages fetched per folder on first sync.
const INITIAL_INBOX: usize = 1500;
const INITIAL_OTHER: usize = 300;
const FETCH_CHUNK: usize = 50;
/// Labels that are not backed by IMAP state and survive re-syncs.
const LOCAL_LABELS: &[&str] = &[labels::IMPORTANT, labels::SNOOZED, "MUTE"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImapConfig {
    pub imap_host: String,
    pub imap_port: u16,
    /// "tls", "starttls" or "none"
    pub imap_security: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: String,
    pub username: String,
    #[serde(default)]
    pub smtp_username: Option<String>,
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> Stream for T {}
pub type Session = async_imap::Session<Box<dyn Stream>>;

fn tls_connector() -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn tls_wrap(tcp: TcpStream, host: &str) -> Result<Box<dyn Stream>> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
    Ok(Box::new(tls_connector().connect(name, tcp).await?))
}

pub async fn open_session(cfg: &ImapConfig, password: &str) -> Result<Session> {
    let addr = (cfg.imap_host.as_str(), cfg.imap_port);
    let tcp = tokio::time::timeout(std::time::Duration::from_secs(20), TcpStream::connect(addr))
        .await
        .context("IMAP connection timed out")??;
    let stream: Box<dyn Stream> = match cfg.imap_security.as_str() {
        "tls" => tls_wrap(tcp, &cfg.imap_host).await?,
        "starttls" => {
            let mut client = async_imap::Client::new(tcp);
            client.read_response().await?.context("no IMAP greeting")?;
            client.run_command_and_check_ok("STARTTLS", None).await?;
            tls_wrap(client.into_inner(), &cfg.imap_host).await?
        }
        _ => Box::new(tcp),
    };
    let mut client = async_imap::Client::new(stream);
    if cfg.imap_security != "starttls" {
        client.read_response().await?.context("no IMAP greeting")?;
    }
    client
        .login(&cfg.username, password)
        .await
        .map_err(|(e, _)| anyhow!("IMAP login failed: {e}"))
}

pub async fn send_smtp(cfg: &ImapConfig, password: &str, message: &lettre::Message) -> Result<()> {
    let creds = Credentials::new(
        cfg.smtp_username.clone().unwrap_or_else(|| cfg.username.clone()),
        password.to_string(),
    );
    let builder = match cfg.smtp_security.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)?,
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)?,
        _ => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp_host),
    };
    let transport = builder.port(cfg.smtp_port).credentials(creds).build();
    transport.send(message.clone()).await.context("SMTP send failed")?;
    Ok(())
}

// ------------------------------------------------------------------ folders

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum Role {
    Inbox,
    Sent,
    Trash,
    Junk,
    Drafts,
    Archive,
    /// A folder nested in the archive; synced but not shown as a label.
    ArchiveSub,
    Other,
}

#[derive(Debug, Clone)]
struct Folder {
    name: String,
    role: Role,
    /// Human-friendly name for user labels, e.g. "INBOX.Projects.X" -> "Projects/X".
    display: String,
}

fn detect_role(name: &str, attrs: &[NameAttribute<'_>]) -> Option<Role> {
    if attrs.iter().any(|a| matches!(a, NameAttribute::NoSelect)) {
        return None;
    }
    for a in attrs {
        match a {
            NameAttribute::All | NameAttribute::Flagged => return None,
            NameAttribute::Sent => return Some(Role::Sent),
            NameAttribute::Trash => return Some(Role::Trash),
            NameAttribute::Junk => return Some(Role::Junk),
            NameAttribute::Drafts => return Some(Role::Drafts),
            NameAttribute::Archive => return Some(Role::Archive),
            _ => {}
        }
    }
    if name.eq_ignore_ascii_case("INBOX") {
        return Some(Role::Inbox);
    }
    let leaf = name.rsplit(['/', '.']).next().unwrap_or(name).to_lowercase();
    Some(match leaf.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" | "wysłane" | "elementy wysłane" => Role::Sent,
        "trash" | "deleted" | "deleted items" | "deleted messages" | "kosz" => Role::Trash,
        "junk" | "spam" | "junk e-mail" | "bulk mail" => Role::Junk,
        "drafts" | "draft" | "szkice" | "kopie robocze" => Role::Drafts,
        "archive" | "archives" | "archiwum" => Role::Archive,
        _ => Role::Other,
    })
}

async fn list_folders(session: &mut Session) -> Result<Vec<Folder>> {
    let names: Vec<_> = session.list(Some(""), Some("*")).await?.try_collect().await?;
    let mut folders: Vec<Folder> = names
        .iter()
        .filter_map(|n| {
            let delimiter = n.delimiter().unwrap_or("/");
            let display = n
                .name()
                .strip_prefix(&format!("INBOX{delimiter}"))
                .unwrap_or(n.name())
                .replace(delimiter, "/");
            detect_role(n.name(), n.attributes()).map(|role| Folder { name: n.name().to_string(), role, display })
        })
        .collect();
    // Keep a single folder per special role (the first match wins).
    let mut seen = HashSet::new();
    folders.retain(|f| matches!(f.role, Role::Other | Role::ArchiveSub) || seen.insert(format!("{:?}", f.role)));
    // Subfolders of the archive (e.g. Thunderbird's yearly Archives/2026) are archive too.
    let archive = folders.iter().find(|f| f.role == Role::Archive).map(|f| f.name.clone());
    if let Some(archive) = archive {
        for f in folders.iter_mut().filter(|f| f.role == Role::Other) {
            if f.name.len() > archive.len() && f.name.starts_with(&archive) {
                f.role = Role::ArchiveSub;
            }
        }
    }
    Ok(folders)
}

fn folder_labels(f: &Folder) -> Vec<String> {
    match f.role {
        Role::Inbox => vec![labels::INBOX.into()],
        Role::Sent => vec![labels::SENT.into()],
        Role::Trash => vec![labels::TRASH.into()],
        Role::Junk => vec![labels::SPAM.into()],
        Role::Drafts => vec![labels::DRAFT.into()],
        Role::Archive | Role::ArchiveSub => vec![],
        Role::Other => vec![f.name.clone()],
    }
}

fn flag_labels<'a>(flags: impl Iterator<Item = Flag<'a>>) -> Vec<String> {
    let mut seen = false;
    let mut starred = false;
    for f in flags {
        match f {
            Flag::Seen => seen = true,
            Flag::Flagged => starred = true,
            _ => {}
        }
    }
    let mut out = vec![];
    if !seen {
        out.push(labels::UNREAD.to_string());
    }
    if starred {
        out.push(labels::STARRED.to_string());
    }
    out
}

fn short_hash(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))[..24].to_string()
}

fn message_key(message_id_header: Option<&str>, folder: &str, uid_validity: u32, uid: u32) -> String {
    match message_id_header {
        Some(m) if !m.is_empty() => format!("m{}", short_hash(m)),
        _ => format!("u{}", short_hash(&format!("{folder}\0{uid_validity}\0{uid}"))),
    }
}

// ------------------------------------------------------------------ driver

pub struct Imap {
    state: AppState,
    pub conn_id: String,
    pub config: ImapConfig,
    password: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FolderState {
    #[serde(rename = "uidValidity")]
    uid_validity: u32,
    #[serde(rename = "lastUid")]
    last_uid: u32,
}

impl Imap {
    pub async fn load(state: &AppState, conn_id: &str) -> Result<Self> {
        let (config, secret, email, name): (Option<Value>, Option<String>, String, Option<String>) =
            sqlx::query_as("SELECT imap_config, secret, email, name FROM connections WHERE id = $1")
                .bind(conn_id)
                .fetch_one(&state.db)
                .await?;
        let config: ImapConfig =
            serde_json::from_value(config.context("mailbox has no IMAP settings")?)?;
        let password = state.crypto.decrypt(&secret.context("mailbox has no password")?)?;
        Ok(Self { state: state.clone(), conn_id: conn_id.to_string(), config, password, email, name })
    }

    async fn session(&self) -> Result<Session> {
        open_session(&self.config, &self.password).await
    }

    pub async fn sync(&self) -> Result<()> {
        let mut session = self.session().await?;
        let result = self.sync_with(&mut session).await;
        let _ = session.logout().await;
        result
    }

    async fn sync_with(&self, session: &mut Session) -> Result<()> {
        let folders = list_folders(session).await?;
        let db = &self.state.db;

        let mut label_list: Vec<Label> = [
            (labels::INBOX, "Inbox"),
            (labels::SENT, "Sent"),
            (labels::TRASH, "Trash"),
            (labels::SPAM, "Spam"),
            (labels::DRAFT, "Drafts"),
            (labels::UNREAD, "Unread"),
            (labels::STARRED, "Starred"),
            (labels::IMPORTANT, "Important"),
        ]
        .iter()
        .map(|(id, name)| Label { id: id.to_string(), name: name.to_string(), color: None, kind: "system".into() })
        .collect();
        for f in folders.iter().filter(|f| f.role == Role::Other) {
            label_list.push(Label { id: f.name.clone(), name: f.display.clone(), color: None, kind: "user".into() });
        }
        store::replace_labels(db, &self.conn_id, &label_list).await?;

        let sync_state: Value = sqlx::query_scalar("SELECT sync_state FROM connections WHERE id = $1")
            .bind(&self.conn_id)
            .fetch_one(db)
            .await?;
        let mut states: HashMap<String, FolderState> =
            serde_json::from_value(sync_state["folders"].clone()).unwrap_or_default();

        // Phase 1: pick up new messages everywhere, so moves are seen before deletions.
        let mut selected = HashMap::new();
        for folder in &folders {
            let mailbox = match session.select(&folder.name).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(conn = %self.conn_id, folder = %folder.name, error = %e, "cannot select folder");
                    continue;
                }
            };
            let uid_validity = mailbox.uid_validity.unwrap_or(0);
            let st = states.entry(folder.name.clone()).or_default();
            if st.uid_validity != uid_validity {
                *st = FolderState { uid_validity, last_uid: 0 };
            }
            let new_uids = if st.last_uid == 0 {
                let mut all: Vec<u32> = session.uid_search("ALL").await?.into_iter().collect();
                all.sort_unstable();
                let limit = if folder.role == Role::Inbox { INITIAL_INBOX } else { INITIAL_OTHER };
                all.split_off(all.len().saturating_sub(limit))
            } else {
                let mut uids: Vec<u32> = session
                    .uid_search(format!("UID {}:*", st.last_uid + 1))
                    .await?
                    .into_iter()
                    .filter(|u| *u > st.last_uid)
                    .collect();
                uids.sort_unstable();
                uids
            };
            if !new_uids.is_empty() {
                tracing::info!(conn = %self.conn_id, folder = %folder.name, count = new_uids.len(), "imap new messages");
                self.fetch_new(session, folder, uid_validity, &new_uids).await?;
                st.last_uid = *new_uids.last().unwrap();
            }
            selected.insert(folder.name.clone(), uid_validity);
            self.save_states(&states).await?;
        }

        // Phase 2: refresh flags and drop messages that disappeared from their folder.
        for folder in &folders {
            let Some(uid_validity) = selected.get(&folder.name) else { continue };
            session.select(&folder.name).await?;
            self.refresh_flags(session, folder, *uid_validity).await?;
        }
        Ok(())
    }

    async fn save_states(&self, states: &HashMap<String, FolderState>) -> Result<()> {
        sqlx::query("UPDATE connections SET sync_state = jsonb_set(sync_state, '{folders}', $2) WHERE id = $1")
            .bind(&self.conn_id)
            .bind(serde_json::to_value(states)?)
            .execute(&self.state.db)
            .await?;
        Ok(())
    }

    async fn fetch_new(&self, session: &mut Session, folder: &Folder, uid_validity: u32, uids: &[u32]) -> Result<()> {
        let db = &self.state.db;
        for chunk in uids.chunks(FETCH_CHUNK) {
            let set = uid_set(chunk);
            let fetches: Vec<_> = session
                .uid_fetch(&set, "(UID FLAGS INTERNALDATE BODY.PEEK[])")
                .await?
                .try_collect()
                .await?;
            for f in fetches {
                let (Some(uid), Some(body)) = (f.uid, f.body()) else { continue };
                let internal = f.internal_date().map(|d| d.with_timezone(&Utc));
                let Some(parsed) = parse::parse(body, "", internal) else {
                    tracing::warn!(conn = %self.conn_id, uid, "unparseable message");
                    continue;
                };
                let id = message_key(parsed.message_id_header.as_deref(), &folder.name, uid_validity, uid);
                let mut label_ids = folder_labels(folder);
                label_ids.extend(flag_labels(f.flags()));
                if let Some(existing) = store::message_ref(db, &self.conn_id, &id).await? {
                    label_ids.extend(existing.label_ids.into_iter().filter(|l| LOCAL_LABELS.contains(&l.as_str())));
                }

                let own_root = parsed.message_id_header.clone().unwrap_or_else(|| id.clone());
                let thread_id = match store::thread_for_message_ids(db, &self.conn_id, &parsed.ancestor_ids).await? {
                    Some(t) => t,
                    None => {
                        let root = parsed.ancestor_ids.first().cloned().unwrap_or(own_root);
                        format!("t{}", short_hash(&root))
                    }
                };
                let mut data = parsed.message;
                data.id = id.clone();
                store::upsert_message(
                    db,
                    &self.conn_id,
                    NewMessage {
                        id,
                        thread_id,
                        message_id_header: parsed.message_id_header,
                        received_on: internal.unwrap_or(parsed.received_on),
                        label_ids,
                        provider_ref: json!({ "folder": folder.name, "uidValidity": uid_validity, "uid": uid }),
                        data,
                        search_text: parsed.search_text,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn refresh_flags(&self, session: &mut Session, folder: &Folder, uid_validity: u32) -> Result<()> {
        let db = &self.state.db;
        let local: Vec<(String, Vec<String>, i64)> = sqlx::query_as(
            "SELECT id, label_ids, (provider_ref->>'uid')::bigint FROM messages
             WHERE connection_id = $1 AND provider_ref->>'folder' = $2
               AND (provider_ref->>'uidValidity')::bigint = $3 AND provider_ref->>'uid' IS NOT NULL",
        )
        .bind(&self.conn_id)
        .bind(&folder.name)
        .bind(uid_validity as i64)
        .fetch_all(db)
        .await?;
        if local.is_empty() {
            return Ok(());
        }
        let min_uid = local.iter().map(|(_, _, u)| *u).min().unwrap_or(1).max(1);
        let fetches: Vec<_> = session
            .uid_fetch(format!("{min_uid}:*"), "(UID FLAGS)")
            .await?
            .try_collect()
            .await?;
        let remote: HashMap<u32, Vec<String>> = fetches
            .iter()
            .filter_map(|f| Some((f.uid?, flag_labels(f.flags()))))
            .collect();

        for (id, current, uid) in local {
            match remote.get(&(uid as u32)) {
                None => store::delete_message(db, &self.conn_id, &id).await?,
                Some(flags) => {
                    let mut wanted = folder_labels(folder);
                    wanted.extend(flags.iter().cloned());
                    wanted.extend(current.iter().filter(|l| LOCAL_LABELS.contains(&l.as_str())).cloned());
                    let mut a = wanted.clone();
                    let mut b = current.clone();
                    a.sort();
                    b.sort();
                    if a != b {
                        store::set_message_labels(db, &self.conn_id, &id, &wanted).await?;
                    }
                }
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- actions

    /// Mirrors a Gmail-style label change onto IMAP flags and folders.
    pub async fn modify_thread(&self, thread_id: &str, add: &[String], remove: &[String]) -> Result<()> {
        let refs = store::thread_message_refs(&self.state.db, &self.conn_id, thread_id).await?;
        if refs.is_empty() {
            return Ok(());
        }
        let mut session = self.session().await?;
        let folders = list_folders(&mut session).await?;
        let result = self.apply_changes(&mut session, &folders, &refs, add, remove).await;
        let _ = session.logout().await;
        result
    }

    async fn apply_changes(
        &self,
        session: &mut Session,
        folders: &[Folder],
        refs: &[store::MessageRef],
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        let has = |list: &[String], l: &str| list.iter().any(|x| x == l);
        let role_folder = |role: Role| folders.iter().find(|f| f.role == role).map(|f| f.name.clone());

        let mut flag_ops: Vec<(&str, &str)> = vec![];
        if has(remove, labels::UNREAD) {
            flag_ops.push(("+FLAGS.SILENT", "(\\Seen)"));
        }
        if has(add, labels::UNREAD) {
            flag_ops.push(("-FLAGS.SILENT", "(\\Seen)"));
        }
        if has(add, labels::STARRED) {
            flag_ops.push(("+FLAGS.SILENT", "(\\Flagged)"));
        }
        if has(remove, labels::STARRED) {
            flag_ops.push(("-FLAGS.SILENT", "(\\Flagged)"));
        }

        let user_add = add.iter().find(|l| folders.iter().any(|f| f.role == Role::Other && &f.name == *l));
        let destination: Option<String> = if has(add, labels::TRASH) {
            role_folder(Role::Trash)
        } else if has(add, labels::SPAM) {
            role_folder(Role::Junk)
        } else if has(add, labels::INBOX) || has(remove, labels::TRASH) || has(remove, labels::SPAM) {
            Some("INBOX".into())
        } else if let Some(l) = user_add {
            Some(l.clone())
        } else if has(remove, labels::INBOX) {
            Some(match role_folder(Role::Archive) {
                Some(a) => a,
                None => {
                    session.create("Archive").await.ok();
                    "Archive".into()
                }
            })
        } else {
            None
        };

        let archiving = destination.is_some()
            && has(remove, labels::INBOX)
            && !has(add, labels::TRASH)
            && !has(add, labels::SPAM)
            && !has(add, labels::INBOX)
            && user_add.is_none();

        // Group messages by their current folder.
        let mut by_folder: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for r in refs {
            let (Some(folder), Some(uid)) = (r.provider_ref["folder"].as_str(), r.provider_ref["uid"].as_u64())
            else {
                continue;
            };
            by_folder.entry(folder.to_string()).or_default().push((r.id.clone(), uid as u32));
        }

        for (folder, msgs) in by_folder {
            let role = folders.iter().find(|f| f.name == folder).map(|f| f.role.clone()).unwrap_or(Role::Other);
            let should_move = match destination.as_deref() {
                Some(d) if d == folder => false,
                None => false,
                // Archiving only takes messages out of the inbox.
                Some(_) if archiving => role == Role::Inbox,
                // Restoring to the inbox leaves sent copies and drafts where they are.
                Some("INBOX") => !matches!(role, Role::Sent | Role::Drafts),
                Some(_) => true,
            };
            if flag_ops.is_empty() && !should_move {
                continue;
            }
            session.select(&folder).await?;
            let uids: Vec<u32> = msgs.iter().map(|(_, u)| *u).collect();
            let set = uid_set(&uids);
            for (op, flags) in &flag_ops {
                let _: Vec<_> = session.uid_store(&set, format!("{op} {flags}")).await?.try_collect().await?;
            }
            if should_move {
                let dest = destination.as_deref().unwrap();
                self.move_uids(session, &set, dest).await?;
                for (id, _) in &msgs {
                    // The new UID is learned on the next sync; until then the message is "in transit".
                    sqlx::query(
                        "UPDATE messages SET provider_ref = jsonb_build_object('folder', $3::text) WHERE connection_id = $1 AND id = $2",
                    )
                    .bind(&self.conn_id)
                    .bind(id)
                    .bind(dest)
                    .execute(&self.state.db)
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn move_uids(&self, session: &mut Session, set: &str, dest: &str) -> Result<()> {
        if session.uid_mv(set, dest).await.is_ok() {
            return Ok(());
        }
        session.uid_copy(set, dest).await?;
        let _: Vec<_> = session.uid_store(set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
        let _: Vec<_> = session.uid_expunge(set).await?.try_collect().await?;
        Ok(())
    }

    pub async fn fetch_raw(&self, message_id: &str) -> Result<Vec<u8>> {
        let r = store::message_ref(&self.state.db, &self.conn_id, message_id)
            .await?
            .context("message not found")?;
        let folder = r.provider_ref["folder"].as_str().context("message location unknown")?.to_string();
        let uid = r.provider_ref["uid"].as_u64().context("message is being moved, try again shortly")?;
        let mut session = self.session().await?;
        session.select(&folder).await?;
        let fetches: Vec<_> = session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .await?
            .try_collect()
            .await?;
        let _ = session.logout().await;
        fetches
            .into_iter()
            .find_map(|f| f.body().map(|b| b.to_vec()))
            .context("message not found on server")
    }

    /// Sends via SMTP and stores a copy in the Sent folder.
    pub async fn send(&self, message: &lettre::Message) -> Result<()> {
        send_smtp(&self.config, &self.password, message).await?;
        let mut session = self.session().await?;
        let folders = list_folders(&mut session).await?;
        if let Some(sent) = folders.iter().find(|f| f.role == Role::Sent) {
            if let Err(e) = session.append(&sent.name, Some("(\\Seen)"), None, message.formatted()).await {
                tracing::warn!(conn = %self.conn_id, error = %e, "could not store sent copy");
            }
        }
        let _ = session.logout().await;
        Ok(())
    }

    pub async fn create_folder(&self, name: &str) -> Result<()> {
        let mut session = self.session().await?;
        session.create(name).await?;
        let _ = session.logout().await;
        Ok(())
    }

    pub async fn delete_folder(&self, name: &str) -> Result<()> {
        let mut session = self.session().await?;
        session.run_command_and_check_ok(format!("DELETE \"{}\"", name.replace('"', "\\\""))).await?;
        let _ = session.logout().await;
        Ok(())
    }

    pub async fn empty_spam(&self) -> Result<usize> {
        let mut session = self.session().await?;
        let folders = list_folders(&mut session).await?;
        let Some(junk) = folders.iter().find(|f| f.role == Role::Junk) else {
            bail!("no spam folder");
        };
        session.select(&junk.name).await?;
        let uids: Vec<u32> = session.uid_search("ALL").await?.into_iter().collect();
        if !uids.is_empty() {
            let set = uid_set(&uids);
            let _: Vec<_> = session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
            let _: Vec<_> = session.uid_expunge(&set).await?.try_collect().await?;
        }
        let _ = session.logout().await;
        Ok(uids.len())
    }
}

/// Verifies IMAP and SMTP settings before a mailbox is saved.
pub async fn test_settings(cfg: &ImapConfig, password: &str) -> Result<()> {
    let mut session = open_session(cfg, password).await?;
    session.select("INBOX").await.context("cannot open INBOX")?;
    let _ = session.logout().await;

    let creds = Credentials::new(cfg.smtp_username.clone().unwrap_or_else(|| cfg.username.clone()), password.to_string());
    let builder = match cfg.smtp_security.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)?,
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)?,
        _ => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp_host),
    };
    let transport: AsyncSmtpTransport<Tokio1Executor> = builder.port(cfg.smtp_port).credentials(creds).build();
    let ok = transport.test_connection().await;
    match ok {
        Ok(true) => Ok(()),
        Ok(false) => bail!("SMTP server did not accept the connection"),
        Err(e) => Err(anyhow!("SMTP check failed: {e}")),
    }
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",")
}
