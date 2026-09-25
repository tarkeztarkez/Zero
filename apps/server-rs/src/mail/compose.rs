//! Builds outgoing RFC 822 messages.

use crate::model::{OutgoingMessage, Sender};
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use lettre::Message;
use lettre::message::header::{ContentType, InReplyTo, References};
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};

pub struct Composed {
    /// Full message including a Bcc header (Gmail needs it to route blind copies).
    pub raw_with_bcc: Vec<u8>,
    /// Message without Bcc, for SMTP delivery (bcc goes in the envelope) and IMAP Sent copies.
    pub message: Message,
}

pub fn build(msg: &OutgoingMessage, from: &Sender) -> Result<Composed> {
    let from_email = msg.from_email.clone().filter(|e| !e.is_empty());
    let from_mailbox: Mailbox = match from_email {
        Some(e) => e.parse().with_context(|| format!("invalid from address {e}"))?,
        None => mailbox(from)?,
    };
    let domain = from_mailbox.email.domain().to_string();
    let message_id = format!("<{}@{}>", uuid::Uuid::new_v4(), domain);

    let mut builder = Message::builder()
        .from(from_mailbox)
        .subject(msg.subject.clone())
        .message_id(Some(message_id.clone()));
    for r in &msg.to {
        builder = builder.to(mailbox(r)?);
    }
    for r in msg.cc.iter().flatten() {
        builder = builder.cc(mailbox(r)?);
    }
    for r in msg.bcc.iter().flatten() {
        builder = builder.bcc(mailbox(r)?);
    }
    for (name, value) in &msg.headers {
        match name.to_lowercase().as_str() {
            "in-reply-to" => builder = builder.header(InReplyTo::from(value.clone())),
            "references" => builder = builder.header(References::from(value.clone())),
            _ => {}
        }
    }

    let mut html = msg.message.clone();
    if msg.is_forward.unwrap_or(false) {
        if let Some(original) = msg.original_message.as_deref().filter(|o| !o.is_empty()) {
            html.push_str("<br><br>---------- Forwarded message ---------<br>");
            html.push_str(original);
        }
    }
    let text = html_to_text(&html);
    let alternative = MultiPart::alternative()
        .singlepart(SinglePart::plain(text))
        .singlepart(SinglePart::html(html));

    let message = if msg.attachments.is_empty() {
        builder.multipart(alternative)?
    } else {
        let mut mixed = MultiPart::mixed().multipart(alternative);
        for a in &msg.attachments {
            let data = STANDARD.decode(a.base64.trim()).context("invalid attachment data")?;
            let content_type = ContentType::parse(&a.mime_type)
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
            mixed = mixed.singlepart(Attachment::new(a.name.clone()).body(data, content_type));
        }
        builder.multipart(mixed)?
    };

    let mut raw = message.formatted();
    let bcc: Vec<String> = msg
        .bcc
        .iter()
        .flatten()
        .filter_map(|b| mailbox(b).ok().map(|m| m.to_string()))
        .collect();
    if !bcc.is_empty() {
        let mut with_bcc = format!("Bcc: {}\r\n", bcc.join(", ")).into_bytes();
        with_bcc.append(&mut raw);
        raw = with_bcc;
    }

    Ok(Composed { raw_with_bcc: raw, message })
}

fn mailbox(s: &Sender) -> Result<Mailbox> {
    let address = s.email.trim().parse().with_context(|| format!("invalid address {}", s.email))?;
    Ok(Mailbox::new(s.name.clone().filter(|n| !n.is_empty()), address))
}

fn html_to_text(html: &str) -> String {
    static BREAKS: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?i)<br\s*/?>|</p>|</div>|</li>|</h[1-6]>").unwrap()
    });
    static TAGS: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?s)<[^>]+>").unwrap());
    let with_breaks = BREAKS.replace_all(html, "\n");
    TAGS.replace_all(&with_breaks, "")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .trim()
        .to_string()
}
