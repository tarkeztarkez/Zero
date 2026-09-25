//! RFC 822 parsing shared by the Gmail and IMAP drivers.

use crate::model::{AttachmentMeta, Header, ParsedMessage, Sender};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, TimeZone, Utc};
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};

/// Inline images above this size are left as attachments instead of being embedded.
const MAX_INLINE_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEARCH_TEXT: usize = 20_000;

pub struct Parsed {
    pub message: ParsedMessage,
    pub message_id_header: Option<String>,
    /// In-Reply-To and References ids, oldest first, without angle brackets.
    pub ancestor_ids: Vec<String>,
    pub received_on: DateTime<Utc>,
    pub search_text: String,
}

pub struct RawAttachment {
    pub meta: AttachmentMeta,
    pub data: Vec<u8>,
}

pub fn parse(raw: &[u8], id: &str, fallback_date: Option<DateTime<Utc>>) -> Option<Parsed> {
    let msg = MessageParser::default().parse(raw)?;

    let received_on = msg
        .date()
        .and_then(|d| Utc.timestamp_opt(d.to_timestamp(), 0).single())
        .or(fallback_date)
        .unwrap_or_else(Utc::now);

    let subject = msg.subject().map(|s| s.trim().to_string()).unwrap_or_default();
    let sender = msg
        .from()
        .and_then(|a| addresses(Some(a)).into_iter().next())
        .unwrap_or(Sender { name: None, email: "unknown".into() });
    let to = addresses(msg.to());
    let cc = addresses(msg.cc());
    let bcc = addresses(msg.bcc());
    let reply_to = msg.reply_to().and_then(|a| addresses(Some(a)).into_iter().next());

    let message_id_header = msg.message_id().map(clean_id);
    let in_reply_to = id_list(msg.in_reply_to());
    let references = id_list(msg.references());
    let mut ancestor_ids = references.clone();
    for id in &in_reply_to {
        if !ancestor_ids.contains(id) {
            ancestor_ids.push(id.clone());
        }
    }

    let text_body = msg.body_text(0).map(|t| t.into_owned()).unwrap_or_default();
    let mut html = match msg.body_html(0) {
        Some(h) => h.into_owned(),
        None => text_to_html(&text_body),
    };

    // Embed inline (cid:) images so the frontend can render them without extra requests.
    let mut attachments = Vec::new();
    for (index, part) in msg.attachments().enumerate() {
        let content_type = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".into());
        let data = part.contents();
        if let Some(cid) = part.content_id() {
            let needle = format!("cid:{}", cid.trim_matches(|c| c == '<' || c == '>'));
            if html.contains(&needle) {
                if data.len() <= MAX_INLINE_IMAGE_BYTES {
                    let data_url = format!("data:{content_type};base64,{}", STANDARD.encode(data));
                    html = html.replace(&needle, &data_url);
                }
                continue;
            }
        }
        attachments.push(AttachmentMeta {
            attachment_id: index.to_string(),
            filename: part.attachment_name().unwrap_or("attachment").to_string(),
            mime_type: content_type,
            size: data.len() as u64,
            body: String::new(),
            headers: part
                .headers()
                .iter()
                .map(|h| Header {
                    name: h.name().to_string(),
                    value: header_text(h.value()),
                })
                .collect(),
        });
    }

    let snippet = snippet_from(&text_body, &html);
    let received_headers: Vec<String> = msg
        .header_values("Received")
        .map(header_text)
        .collect();
    let tls = received_headers
        .iter()
        .any(|r| r.contains("TLS") || r.contains("ESMTPS") || r.contains("ESMTPSA"))
        || msg.header("TLS-Report").is_some();

    let search_text = truncate(
        &format!(
            "{subject}\n{} {}\n{}\n{}",
            sender.name.clone().unwrap_or_default(),
            sender.email,
            to.iter().map(|s| s.email.as_str()).collect::<Vec<_>>().join(" "),
            text_body_or_stripped(&text_body, &html)
        ),
        MAX_SEARCH_TEXT,
    );

    let message = ParsedMessage {
        id: id.to_string(),
        connection_id: None,
        title: snippet,
        subject: if subject.is_empty() { "(no subject)".into() } else { subject },
        tags: vec![],
        sender,
        to,
        cc: if cc.is_empty() { None } else { Some(cc) },
        bcc: Some(bcc),
        tls,
        list_unsubscribe: msg.header_raw("List-Unsubscribe").map(|v| v.trim().to_string()),
        list_unsubscribe_post: msg
            .header_raw("List-Unsubscribe-Post")
            .map(|v| v.trim().to_string()),
        received_on: received_on.to_rfc3339(),
        unread: false,
        body: String::new(),
        processed_html: String::new(),
        blob_url: String::new(),
        decoded_body: Some(html),
        references: if references.is_empty() {
            None
        } else {
            Some(references.iter().map(|r| format!("<{r}>")).collect::<Vec<_>>().join(" "))
        },
        in_reply_to: in_reply_to.first().map(|r| format!("<{r}>")),
        reply_to: reply_to.map(|r| format_sender(&r)),
        message_id: message_id_header.as_ref().map(|m| format!("<{m}>")),
        thread_id: None,
        attachments,
        is_draft: false,
    };

    Some(Parsed { message, message_id_header, ancestor_ids, received_on, search_text })
}

/// Returns non-inline attachments with their data, indexed like `parse` does.
pub fn attachments(raw: &[u8]) -> Vec<RawAttachment> {
    let Some(parsed) = parse(raw, "", None) else { return vec![] };
    let Some(msg) = MessageParser::default().parse(raw) else { return vec![] };
    let parts: Vec<_> = msg.attachments().collect();
    parsed
        .message
        .attachments
        .into_iter()
        .filter_map(|meta| {
            let index: usize = meta.attachment_id.parse().ok()?;
            let data = parts.get(index)?.contents().to_vec();
            Some(RawAttachment { meta, data })
        })
        .collect()
}

fn addresses(addr: Option<&Address>) -> Vec<Sender> {
    let Some(addr) = addr else { return vec![] };
    addr.iter()
        .filter_map(|a| {
            let email = a.address.as_ref()?.trim().to_string();
            if email.is_empty() {
                return None;
            }
            Some(Sender {
                name: a.name.as_ref().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
                email,
            })
        })
        .collect()
}

pub fn format_sender(s: &Sender) -> String {
    match &s.name {
        Some(n) => format!("{n} <{}>", s.email),
        None => s.email.clone(),
    }
}

fn clean_id(id: &str) -> String {
    id.trim().trim_matches(|c| c == '<' || c == '>').to_string()
}

fn id_list(v: &HeaderValue) -> Vec<String> {
    match v {
        HeaderValue::Text(t) => vec![clean_id(t)],
        HeaderValue::TextList(list) => list.iter().map(|t| clean_id(t)).collect(),
        _ => vec![],
    }
}

fn header_text(v: &HeaderValue) -> String {
    match v {
        HeaderValue::Text(t) => t.to_string(),
        HeaderValue::TextList(l) => l.join(", "),
        HeaderValue::Address(a) => a
            .iter()
            .filter_map(|x| x.address.as_ref().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        HeaderValue::ContentType(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        },
        HeaderValue::DateTime(d) => d.to_rfc3339(),
        _ => String::new(),
    }
}

fn text_to_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<div style=\"white-space: pre-wrap\">{escaped}</div>")
}

fn text_body_or_stripped(text: &str, html: &str) -> String {
    if !text.trim().is_empty() { text.to_string() } else { strip_tags(html) }
}

fn strip_tags(html: &str) -> String {
    static TAGS: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?is)<(style|script)[^>]*>.*?</(style|script)>|<[^>]+>").unwrap()
    });
    TAGS.replace_all(html, " ")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn snippet_from(text: &str, html: &str) -> String {
    let source = text_body_or_stripped(text, html);
    let collapsed = source.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, 200)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
