//! Streaming `<head>` metadata extractor built on `lol_html`.
//!
//! Collects Open Graph / Twitter Card / standard meta fields, the first
//! `<title>`, icon links, `html[lang]` and `base[href]`. Parsing stops early
//! at `</head>` or the first `<body>` start tag; rewriter errors keep whatever
//! was collected instead of failing.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use lol_html::html_content::{Element, EndTag, TextChunk};
use lol_html::{AsciiCompatibleEncoding, Selector, element, end_tag, send, text};

use crate::MAX_URL_LEN;

const MAX_TITLE_CHARS: usize = 300;
const MAX_DESCRIPTION_CHARS: usize = 600;
const MAX_SITE_NAME_CHARS: usize = 100;
const MAX_ALT_CHARS: usize = 300;
const MAX_KIND_CHARS: usize = 50;
const MAX_LANG_CHARS: usize = 35;
const MAX_THEME_COLOR_CHARS: usize = 32;

/// `meta` `name`/`property` values collected from the document head.
const META_KEYS: &[&str] = &[
    "og:title",
    "og:description",
    "og:site_name",
    "og:image",
    "og:image:url",
    "og:image:secure_url",
    "og:image:width",
    "og:image:height",
    "og:image:alt",
    "og:type",
    "twitter:title",
    "twitter:description",
    "twitter:image",
    "twitter:image:src",
    "twitter:image:alt",
    "twitter:card",
    "description",
    "theme-color",
];

type Handler = (
    Cow<'static, Selector>,
    send::ElementContentHandlers<'static>,
);

type Rewriter = send::HtmlRewriter<'static, fn(&[u8])>;

/// Whether the extractor wants more bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep feeding chunks.
    Continue,
    /// `<body>` or `</head>` was reached (or the rewriter failed); further
    /// writes are ignored.
    Done,
}

/// Raw metadata candidates collected from the document head. First
/// occurrence wins for every key; values are entity-decoded,
/// whitespace-collapsed, trimmed and truncated.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Metadata {
    /// `meta` key → content, keys lowercased.
    pub meta: HashMap<String, String>,
    /// First `<title>` text.
    pub title: Option<String>,
    /// First `link` with a plain `icon`-class `rel`.
    pub icon: Option<String>,
    /// First `link` with an `apple-touch-icon*` `rel`.
    pub touch_icon: Option<String>,
    /// `html[lang]`.
    pub lang: Option<String>,
    /// First `base[href]`.
    pub base: Option<String>,
}

#[derive(Default, Debug)]
struct Inner {
    done: bool,
    title_seen: bool,
    collecting_title: bool,
    title_buf: String,
    title: Option<String>,
    meta: HashMap<String, String>,
    icon: Option<String>,
    touch_icon: Option<String>,
    lang: Option<String>,
    base: Option<String>,
}

fn lock(state: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_done(state: &Mutex<Inner>) -> bool {
    lock(state).done
}

/// Decodes HTML entities, collapses whitespace, trims; empty → `None`,
/// longer than `max_chars` → truncated on a char boundary.
fn clean(raw: &str, max_chars: usize) -> Option<String> {
    let decoded = html_escape::decode_html_entities(raw);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then(|| collapsed.chars().take(max_chars).collect())
}

fn max_chars_for(key: &str) -> usize {
    match key {
        "description" | "og:description" | "twitter:description" => MAX_DESCRIPTION_CHARS,
        "og:site_name" => MAX_SITE_NAME_CHARS,
        "og:image:alt" | "twitter:image:alt" => MAX_ALT_CHARS,
        "og:type" | "twitter:card" => MAX_KIND_CHARS,
        "theme-color" => MAX_THEME_COLOR_CHARS,
        _ => MAX_URL_LEN,
    }
}

const SINK: fn(&[u8]) = |_| {};

fn on_meta(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    element!("meta", move |el: &mut Element<'_, '_, _>| {
        if is_done(&st) {
            return Ok(());
        }
        let Some(name) = el
            .get_attribute("property")
            .or_else(|| el.get_attribute("name"))
        else {
            return Ok(());
        };
        let key = name.to_ascii_lowercase();
        if !META_KEYS.contains(&key.as_str()) {
            return Ok(());
        }
        let Some(content) = el.get_attribute("content") else {
            return Ok(());
        };
        if let Some(value) = clean(&content, max_chars_for(&key)) {
            lock(&st).meta.entry(key).or_insert(value);
        }
        Ok(())
    })
}

fn on_title(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    element!("title", move |el: &mut Element<'_, '_, _>| {
        {
            let mut g = lock(&st);
            if g.done || g.title_seen {
                return Ok(());
            }
            g.title_seen = true;
            g.collecting_title = true;
        }
        let st2 = Arc::clone(&st);
        el.on_end_tag(end_tag!(move |_: &mut EndTag<'_>| {
            let mut g = lock(&st2);
            g.collecting_title = false;
            let raw = std::mem::take(&mut g.title_buf);
            g.title = clean(&raw, MAX_TITLE_CHARS);
            drop(g);
            Ok(())
        }))
    })
}

fn on_title_text(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    text!("title", move |chunk: &mut TextChunk<'_>| {
        let mut g = lock(&st);
        if g.collecting_title && !g.done {
            g.title_buf.push_str(chunk.as_str());
        }
        drop(g);
        Ok(())
    })
}

fn on_link(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    element!("link", move |el: &mut Element<'_, '_, _>| {
        if is_done(&st) {
            return Ok(());
        }
        let Some(rel) = el.get_attribute("rel") else {
            return Ok(());
        };
        let is_icon = rel
            .split_whitespace()
            .any(|t| t.eq_ignore_ascii_case("icon"));
        let is_touch = rel.split_whitespace().any(|t| {
            t.get(..16)
                .is_some_and(|p| p.eq_ignore_ascii_case("apple-touch-icon"))
        });
        if !(is_icon || is_touch) {
            return Ok(());
        }
        let Some(href) = el.get_attribute("href") else {
            return Ok(());
        };
        let Some(href) = clean(&href, MAX_URL_LEN) else {
            return Ok(());
        };
        let mut g = lock(&st);
        if is_icon {
            if g.icon.is_none() {
                g.icon = Some(href);
            }
        } else if g.touch_icon.is_none() {
            g.touch_icon = Some(href);
        }
        drop(g);
        Ok(())
    })
}

fn on_html(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    element!("html", move |el: &mut Element<'_, '_, _>| {
        if is_done(&st) {
            return Ok(());
        }
        if let Some(lang) = el.get_attribute("lang") {
            let mut g = lock(&st);
            if g.lang.is_none() {
                g.lang = clean(&lang, MAX_LANG_CHARS);
            }
        }
        Ok(())
    })
}

fn on_base(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    element!("base", move |el: &mut Element<'_, '_, _>| {
        if is_done(&st) {
            return Ok(());
        }
        if let Some(href) = el.get_attribute("href") {
            let mut g = lock(&st);
            if g.base.is_none() {
                g.base = clean(&href, MAX_URL_LEN);
            }
        }
        Ok(())
    })
}

fn on_head(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    element!("head", move |el: &mut Element<'_, '_, _>| {
        let st2 = Arc::clone(&st);
        el.on_end_tag(end_tag!(move |_: &mut EndTag<'_>| {
            lock(&st2).done = true;
            Ok(())
        }))
    })
}

fn on_body(state: &Arc<Mutex<Inner>>) -> Handler {
    let st = Arc::clone(state);
    element!("body", move |_: &mut Element<'_, '_, _>| {
        lock(&st).done = true;
        Ok(())
    })
}

/// Streaming extractor. Feed response body bytes via [`Extractor::write`],
/// then call [`Extractor::finish`].
#[derive(Debug)]
pub(crate) struct Extractor {
    rewriter: Option<Rewriter>,
    state: Arc<Mutex<Inner>>,
}

impl Extractor {
    /// `content_type_charset` is the `charset` parameter of the upstream
    /// `Content-Type`, if any. A recognized ASCII-compatible label fixes the
    /// decoder; otherwise UTF-8 is used and `<meta charset>` hints adjust it.
    #[must_use]
    pub(crate) fn new(content_type_charset: Option<&str>) -> Self {
        let state = Arc::new(Mutex::new(Inner::default()));
        let encoding = content_type_charset
            .and_then(|label| encoding_rs::Encoding::for_label(label.trim().as_bytes()))
            .and_then(AsciiCompatibleEncoding::new);
        let mut settings = send::Settings::new_send().with_strict(false);
        settings = match encoding {
            Some(enc) => settings
                .with_encoding(enc)
                .with_adjust_charset_on_meta_tag(false),
            None => settings.with_adjust_charset_on_meta_tag(true),
        };
        let settings = settings
            .append_element_content_handler(on_meta(&state))
            .append_element_content_handler(on_title(&state))
            .append_element_content_handler(on_title_text(&state))
            .append_element_content_handler(on_link(&state))
            .append_element_content_handler(on_html(&state))
            .append_element_content_handler(on_base(&state))
            .append_element_content_handler(on_head(&state))
            .append_element_content_handler(on_body(&state));
        Self {
            rewriter: Some(send::HtmlRewriter::new(settings, SINK)),
            state,
        }
    }

    /// Feeds one chunk. Once [`Flow::Done`] is returned, further writes are
    /// no-ops; a rewriter error also yields `Done` while keeping everything
    /// collected so far.
    pub(crate) fn write(&mut self, chunk: &[u8]) -> Flow {
        if is_done(&self.state) {
            return Flow::Done;
        }
        if let Some(rewriter) = self.rewriter.as_mut() {
            if rewriter.write(chunk).is_err() {
                lock(&self.state).done = true;
            }
        } else {
            lock(&self.state).done = true;
        }
        if is_done(&self.state) {
            Flow::Done
        } else {
            Flow::Continue
        }
    }

    /// Finalizes parsing and returns the collected candidates.
    #[must_use]
    pub(crate) fn finish(mut self) -> Metadata {
        if let Some(rewriter) = self.rewriter.take() {
            let _ended = rewriter.end();
        }
        let mut inner = lock(&self.state);
        Metadata {
            meta: std::mem::take(&mut inner.meta),
            title: inner.title.take(),
            icon: inner.icon.take(),
            touch_icon: inner.touch_icon.take(),
            lang: inner.lang.take(),
            base: inner.base.take(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]], charset: Option<&str>) -> Metadata {
        let mut ex = Extractor::new(charset);
        for chunk in chunks {
            if ex.write(chunk) == Flow::Done {
                break;
            }
        }
        ex.finish()
    }

    fn meta<'m>(md: &'m Metadata, key: &str) -> Option<&'m str> {
        md.meta.get(key).map(String::as_str)
    }

    const FULL: &str = r##"<!doctype html><html lang="en-US"><head>
        <title> My &amp; Title </title>
        <base href="/sub/">
        <meta property="og:title" content="OG &quot;Title&quot;">
        <meta property="og:description" content="  spaced   out  ">
        <meta property="og:site_name" content="Site">
        <meta property="og:image" content="img.png">
        <meta property="og:image:width" content="1200">
        <meta property="og:type" content="article">
        <meta name="twitter:card" content="summary_large_image">
        <meta name="theme-color" content="#fff">
        <meta name="description" content="plain desc">
        <link rel="apple-touch-icon" href="/apple.png">
        <link rel="shortcut icon" href="/f.ico">
        <link rel="stylesheet" href="/s.css">
        </head><body></body></html>"##;

    #[test]
    fn full_head() {
        let md = run(&[FULL.as_bytes()], None);
        assert_eq!(md.title.as_deref(), Some("My & Title"));
        assert_eq!(meta(&md, "og:title"), Some("OG \"Title\""));
        assert_eq!(meta(&md, "og:description"), Some("spaced out"));
        assert_eq!(meta(&md, "og:site_name"), Some("Site"));
        assert_eq!(meta(&md, "og:image"), Some("img.png"));
        assert_eq!(meta(&md, "og:image:width"), Some("1200"));
        assert_eq!(meta(&md, "og:type"), Some("article"));
        assert_eq!(meta(&md, "twitter:card"), Some("summary_large_image"));
        assert_eq!(meta(&md, "theme-color"), Some("#fff"));
        assert_eq!(meta(&md, "description"), Some("plain desc"));
        assert_eq!(md.icon.as_deref(), Some("/f.ico"));
        assert_eq!(md.touch_icon.as_deref(), Some("/apple.png"));
        assert_eq!(md.lang.as_deref(), Some("en-US"));
        assert_eq!(md.base.as_deref(), Some("/sub/"));
    }

    #[test]
    fn first_occurrence_wins() {
        let doc = r#"<head><meta property="og:title" content="first"><meta property="og:title" content="second"><title>One</title><title>Two</title></head>"#;
        let md = run(&[doc.as_bytes()], None);
        assert_eq!(meta(&md, "og:title"), Some("first"));
        assert_eq!(md.title.as_deref(), Some("One"));
    }

    #[test]
    fn rel_tokens_are_ascii_case_insensitive() {
        let doc = r#"<head><link rel="Shortcut Icon" href="/s.ico"><link rel="ICON" href="/i.ico"></head>"#;
        let md = run(&[doc.as_bytes()], None);
        assert_eq!(md.icon.as_deref(), Some("/s.ico"));
    }

    #[test]
    fn empty_and_missing_values() {
        let doc = r#"<head><meta property="og:title"><meta name="description" content="   "><title></title></head>"#;
        let md = run(&[doc.as_bytes()], None);
        assert!(md.meta.is_empty(), "{:?}", md.meta);
        assert!(md.title.is_none());
    }

    #[test]
    fn entities_in_title() {
        let doc = "<head><title>Tom &#39; Jerry &amp; Co</title></head>";
        let md = run(&[doc.as_bytes()], None);
        assert_eq!(md.title.as_deref(), Some("Tom ' Jerry & Co"));
    }

    #[test]
    fn charset_from_content_type() {
        // "caf\xe9" is "café" in windows-1252.
        let doc = b"<head><title>caf\xe9</title></head>";
        let md = run(&[doc], Some("windows-1252"));
        assert_eq!(md.title.as_deref(), Some("café"));
        // Without the label the byte is not valid UTF-8 and must not decode as é.
        let md2 = run(&[doc], None);
        assert_ne!(md2.title.as_deref(), Some("café"));
    }

    #[test]
    fn charset_from_meta_tag() {
        let doc = b"<head><meta charset=\"windows-1252\"><title>caf\xe9</title></head>";
        let md = run(&[doc], None);
        assert_eq!(md.title.as_deref(), Some("café"));
    }

    #[test]
    fn charset_label_beats_meta_tag() {
        // A declared UTF-8 charset wins over a conflicting meta tag.
        let doc = b"<head><meta charset=\"windows-1252\"><title>caf\xc3\xa9</title></head>";
        let md = run(&[doc], Some("utf-8"));
        assert_eq!(md.title.as_deref(), Some("café"));
    }

    #[test]
    fn no_head_body_start_stops() {
        let md = run(&[b"<body>"], None);
        assert!(md.meta.is_empty() && md.title.is_none());
        // Everything after <body> is ignored, even within one write.
        let md2 = run(
            &[b"<body><meta property=\"og:title\" content=\"late\">"],
            None,
        );
        assert!(meta(&md2, "og:title").is_none());
    }

    #[test]
    fn head_end_stops() {
        let mut ex = Extractor::new(None);
        assert_eq!(ex.write(b"<head></head>"), Flow::Done);
        assert_eq!(
            ex.write(b"<meta property=\"og:title\" content=\"x\">"),
            Flow::Done
        );
        assert!(ex.finish().meta.is_empty());
    }

    #[test]
    fn huge_head_keeps_going() {
        let mut doc = b"<head><meta property=\"og:title\" content=\"t\">".to_vec();
        doc.resize(2 * 1024 * 1024, b' ');
        let mut ex = Extractor::new(None);
        assert_eq!(ex.write(&doc), Flow::Continue);
        let md = ex.finish();
        assert_eq!(meta(&md, "og:title"), Some("t"));
    }

    #[test]
    fn truncation() {
        let title = "x".repeat(400);
        let desc = "y".repeat(700);
        let doc = format!(
            "<head><title>{title}</title><meta name=\"description\" content=\"{desc}\"></head>"
        );
        let md = run(&[doc.as_bytes()], None);
        assert_eq!(md.title.as_deref().map(str::len), Some(300));
        assert_eq!(meta(&md, "description").map(str::len), Some(600));
    }

    #[test]
    fn identical_at_every_split_point() {
        let bytes = FULL.as_bytes();
        let want = run(&[bytes], None);
        for i in 1..bytes.len() {
            let (head, tail) = bytes.split_at(i);
            let parts: [&[u8]; 2] = (head, tail).into();
            assert_eq!(run(&parts, None), want, "split at byte {i}");
        }
    }
}
