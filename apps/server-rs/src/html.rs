//! Server-side email HTML sanitizing, mirroring apps/server/src/lib/email-processor.ts.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

pub struct Processed {
    pub html: String,
    pub has_blocked_images: bool,
}

pub fn process(html: &str, load_images: bool, dark: bool) -> Processed {
    let sanitized = sanitize(html);
    let without_trackers = TRACKING_PIXEL.replace_all(&sanitized, "");
    let collapsed = collapse_quotes(&without_trackers);

    let mut has_blocked_images = false;
    let body = if load_images {
        collapsed
    } else {
        let replaced = IMG.replace_all(&collapsed, |caps: &regex::Captures| {
            let tag = &caps[0];
            if SRC_CID.is_match(tag) {
                tag.to_string()
            } else {
                has_blocked_images = true;
                String::new()
            }
        });
        CSS_URL.replace_all(&replaced, "none").into_owned()
    };

    Processed { html: format!("{}{}", theme_styles(dark), body), has_blocked_images }
}

fn sanitize(html: &str) -> String {
    let generic: HashSet<&str> = [
        "class", "style", "align", "valign", "width", "height", "cellpadding", "cellspacing",
        "border", "bgcolor", "colspan", "rowspan", "dir", "color", "face", "size",
    ]
    .into_iter()
    .collect();
    let mut builder = ammonia::Builder::default();
    builder
        .add_tags(["img", "details", "summary", "style", "font", "center", "table", "thead", "tbody", "tfoot", "tr", "td", "th", "span", "div"])
        .rm_clean_content_tags(["style"])
        .generic_attributes(generic)
        .add_tag_attributes("a", ["href", "name", "target"])
        .add_tag_attributes("img", ["src", "alt", "width", "height"])
        .url_schemes(["http", "https", "mailto", "tel", "data", "cid"].into_iter().collect())
        .link_rel(Some("noopener noreferrer"))
        .set_tag_attribute_value("a", "target", "_blank")
        .strip_comments(true);
    builder.clean(html).to_string()
}

static TRACKING_PIXEL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)<img[^>]*\bwidth="[01]"[^>]*\bheight="[01]"[^>]*>|<img[^>]*\bheight="[01]"[^>]*\bwidth="[01]"[^>]*>"#).unwrap()
});
static IMG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<img\b[^>]*>").unwrap());
static SRC_CID: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)\bsrc="(cid:|data:)"#).unwrap());
static CSS_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)url\([^)]*\)").unwrap());
static QUOTE_OPEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)<blockquote\b[^>]*>|<div\b[^>]*class="[^"]*gmail_quote[^"]*"[^>]*>"#).unwrap()
});

/// Wraps the first quoted block and everything after it in a collapsed <details>.
fn collapse_quotes(html: &str) -> String {
    let Some(m) = QUOTE_OPEN.find(html) else { return html.to_string() };
    format!(
        "{}<details class=\"quoted-toggle\" style=\"margin-top:1em;\"><summary style=\"cursor:pointer;\" data-theme-color=\"muted\">Show quoted text</summary>{}</details>",
        &html[..m.start()],
        &html[m.start()..]
    )
}

fn theme_styles(dark: bool) -> String {
    let (bg, fg, link, border, muted) = if dark {
        ("#1A1A1A", "#ffffff", "#60a5fa", "#374151", "#9CA3AF")
    } else {
        ("#ffffff", "#000000", "#2563eb", "#d1d5db", "#6B7280")
    };
    format!(
        r#"<style type="text/css">
:host {{ display: block; line-height: 1.5; background-color: {bg}; color: {fg}; }}
*, *::before, *::after {{ box-sizing: border-box; }}
body {{ margin: 0; padding: 0; }}
a {{ cursor: pointer; color: {link}; text-decoration: underline; }}
table {{ border-collapse: collapse; }}
::selection {{ background: #b3d4fc; text-shadow: none; }}
details.quoted-toggle {{ border-left: 2px solid {border}; padding-left: 8px; margin-top: 0.75rem; }}
details.quoted-toggle summary {{ cursor: pointer; color: {muted}; list-style: none; user-select: none; }}
details.quoted-toggle summary::-webkit-details-marker {{ display: none; }}
[data-theme-color="muted"] {{ color: {muted}; }}
</style>"#
    )
}
