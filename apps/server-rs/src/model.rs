use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sender {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentMeta {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    /// Base64 body; empty in stored messages, filled by getMessageAttachments.
    pub body: String,
    pub headers: Vec<Header>,
}

/// Mirrors ParsedMessage in apps/server/src/types.ts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedMessage {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub connection_id: Option<String>,
    pub title: String,
    pub subject: String,
    pub tags: Vec<Tag>,
    pub sender: Sender,
    pub to: Vec<Sender>,
    pub cc: Option<Vec<Sender>>,
    pub bcc: Option<Vec<Sender>>,
    pub tls: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_unsubscribe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_unsubscribe_post: Option<String>,
    pub received_on: String,
    pub unread: bool,
    pub body: String,
    pub processed_html: String,
    pub blob_url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub decoded_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub references: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentMeta>,
    #[serde(default)]
    pub is_draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelColor {
    #[serde(rename = "backgroundColor")]
    pub background_color: String,
    #[serde(rename = "textColor")]
    pub text_color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub color: Option<LabelColor>,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedFile {
    pub name: String,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub size: u64,
    #[serde(rename = "lastModified", default)]
    pub last_modified: f64,
    pub base64: String,
}

/// Mirrors IOutgoingMessage plus the send-only fields of mail.send.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingMessage {
    pub to: Vec<Sender>,
    #[serde(default)]
    pub cc: Option<Vec<Sender>>,
    #[serde(default)]
    pub bcc: Option<Vec<Sender>>,
    pub subject: String,
    pub message: String,
    #[serde(default)]
    pub attachments: Vec<SerializedFile>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub from_email: Option<String>,
    #[serde(default)]
    pub draft_id: Option<String>,
    #[serde(default)]
    pub is_forward: Option<bool>,
    #[serde(default)]
    pub original_message: Option<String>,
    #[serde(default)]
    pub schedule_at: Option<String>,
}

/// System label ids shared by Gmail and our IMAP mapping.
pub mod labels {
    pub const INBOX: &str = "INBOX";
    pub const SENT: &str = "SENT";
    pub const TRASH: &str = "TRASH";
    pub const SPAM: &str = "SPAM";
    pub const DRAFT: &str = "DRAFT";
    pub const UNREAD: &str = "UNREAD";
    pub const STARRED: &str = "STARRED";
    pub const IMPORTANT: &str = "IMPORTANT";
    pub const SNOOZED: &str = "SNOOZED";
}
