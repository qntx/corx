#![no_main]

//! Drive the extractor through `corx::preview`: one 200 `text/html`
//! response whose Content-Type charset, document structure and body
//! chunking come from the fuzzer. The same body served in one chunk
//! must produce exactly the same preview — chunking is transport, not
//! content. `Piece` is structure-aware (tags, attributes, comments) so
//! the corpus reaches the extractor's tag/attribute branches quickly,
//! while `Raw` still admits arbitrary bytes.

use std::borrow::Cow;
use std::pin::Pin;

use bytes::Bytes;
use corx::{Fetch, FetchError, Preview, Target, Trace};
use futures_util::Stream;
use futures_util::stream;
use libfuzzer_sys::arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use url::Url;

/// Keep executions fast: anything larger is transport, not extraction.
const MAX_HTML: usize = 256 * 1024;

/// Labels `Known` indexes modulo its length — a spread of what the web
/// sends, including encodings that reinterpret every byte.
const KNOWN_CHARSETS: &[&str] = &[
    "utf-8",
    "windows-1252",
    "iso-8859-1",
    "shift_jis",
    "euc-kr",
    "gbk",
    "big5",
    "utf-16le",
    "x-user-defined",
    "koi8-r",
    "iso-2022-jp",
    "macintosh",
];

/// How many entries [`known_value`] produces (modulo divisor).
const KNOWN_VALUES: usize = 42;

/// Values `Value::Known` indexes modulo [`KNOWN_VALUES`]: metadata
/// names, rel values, charset labels, URLs, dimension strings, an
/// entity salad, and two boundary-length strings — chosen so a short
/// fuzz run can reach every extractor branch.
fn known_value(i: u8) -> Cow<'static, str> {
    match usize::from(i) % KNOWN_VALUES {
        0 => "og:title",
        1 => "og:description",
        2 => "og:site_name",
        3 => "og:image",
        4 => "og:image:url",
        5 => "og:image:secure_url",
        6 => "og:image:width",
        7 => "og:image:height",
        8 => "og:image:alt",
        9 => "og:type",
        10 => "twitter:title",
        11 => "twitter:description",
        12 => "twitter:image",
        13 => "twitter:image:src",
        14 => "twitter:image:alt",
        15 => "twitter:card",
        16 => "description",
        17 => "theme-color",
        18 => "icon",
        19 => "shortcut icon",
        20 => "apple-touch-icon",
        21 => "apple-touch-icon-precomposed",
        22 => "stylesheet",
        23 => "utf-8",
        24 => "windows-1252",
        25 => "shift_jis",
        26 => "utf-16le",
        27 => "content-type",
        28 => "text/html; charset=windows-1252",
        29 => "/a.png",
        30 => "https://x.example/i.png",
        31 => "//cdn.example/i.png",
        32 => "javascript:alert(1)",
        33 => "data:image/png;base64,AA==",
        34 => "1200",
        35 => "0",
        36 => "-5",
        37 => "en-US",
        38 => "#fff",
        39 => "&amp;&lt;&#39;&#x1F600;",
        // A URL just over the extractor's image-URL length cap.
        40 => return Cow::Owned(format!("https://a.example/{}", "p".repeat(2082))),
        // A text just over the description cap.
        _ => return Cow::Owned("abc ".repeat(175)),
    }
    .into()
}

#[derive(Debug, Arbitrary)]
enum Charset {
    /// No charset parameter.
    None,
    /// A label from [`KNOWN_CHARSETS`].
    Known(u8),
    /// A raw label — may be a value `HeaderValue` or `mime` rejects.
    Raw(String),
}

/// Elements the extractor reads, plus a few it skips.
#[derive(Clone, Copy, Debug, Arbitrary)]
enum Tag {
    Html,
    Head,
    Title,
    Meta,
    Link,
    Base,
    Body,
    Script,
    Style,
    Noscript,
    Template,
    Svg,
    Div,
}

impl Tag {
    fn name(self) -> &'static str {
        match self {
            Tag::Html => "html",
            Tag::Head => "head",
            Tag::Title => "title",
            Tag::Meta => "meta",
            Tag::Link => "link",
            Tag::Base => "base",
            Tag::Body => "body",
            Tag::Script => "script",
            Tag::Style => "style",
            Tag::Noscript => "noscript",
            Tag::Template => "template",
            Tag::Svg => "svg",
            Tag::Div => "div",
        }
    }
}

/// Attribute names the extractor inspects.
#[derive(Clone, Copy, Debug, Arbitrary)]
enum Attr {
    Property,
    Name,
    Content,
    Rel,
    Href,
    Charset,
    HttpEquiv,
    Lang,
}

impl Attr {
    fn name(self) -> &'static str {
        match self {
            Attr::Property => "property",
            Attr::Name => "name",
            Attr::Content => "content",
            Attr::Rel => "rel",
            Attr::Href => "href",
            Attr::Charset => "charset",
            Attr::HttpEquiv => "http-equiv",
            Attr::Lang => "lang",
        }
    }
}

#[derive(Debug, Arbitrary)]
enum Value {
    /// One of [`known_value`]'s entries.
    Known(u8),
    /// Any string.
    Raw(String),
}

/// One serialized piece of the document. `Open` writes attribute values
/// verbatim inside double quotes — an embedded quote is fine, that is
/// what fuzzing is for.
#[derive(Debug, Arbitrary)]
enum Piece {
    /// Arbitrary bytes, verbatim.
    Raw(Vec<u8>),
    /// Text node, verbatim.
    Text(String),
    /// `<tag a="v" …>` or `<tag a="v" …/>`.
    Open {
        tag: Tag,
        attrs: Vec<(Attr, Value)>,
        self_close: bool,
    },
    /// `</tag>`.
    Close(Tag),
    /// `<!--…-->`.
    Comment(String),
}

#[derive(Debug, Arbitrary)]
struct Input {
    /// The Content-Type charset parameter.
    charset: Charset,
    /// Cumulative split points (mod body len, sorted, deduped).
    splits: Vec<u16>,
    /// The document, serialized with [`serialize`].
    doc: Vec<Piece>,
}

/// Renders `doc` into response-body bytes.
fn serialize(doc: &[Piece]) -> Vec<u8> {
    let mut out = Vec::new();
    for piece in doc {
        match piece {
            Piece::Raw(bytes) => out.extend_from_slice(bytes),
            Piece::Text(text) => out.extend_from_slice(text.as_bytes()),
            Piece::Open {
                tag,
                attrs,
                self_close,
            } => {
                out.push(b'<');
                out.extend_from_slice(tag.name().as_bytes());
                for (attr, value) in attrs {
                    out.push(b' ');
                    out.extend_from_slice(attr.name().as_bytes());
                    out.extend_from_slice(b"=\"");
                    match value {
                        Value::Known(i) => {
                            out.extend_from_slice(known_value(*i).as_bytes());
                        }
                        Value::Raw(s) => out.extend_from_slice(s.as_bytes()),
                    }
                    out.push(b'"');
                }
                out.push(if *self_close { b'/' } else { b'>' });
                if *self_close {
                    out.push(b'>');
                }
            }
            Piece::Close(tag) => {
                out.extend_from_slice(b"</");
                out.extend_from_slice(tag.name().as_bytes());
                out.push(b'>');
            }
            Piece::Comment(text) => {
                out.extend_from_slice(b"<!--");
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(b"-->");
            }
        }
    }
    out
}

type Body = Pin<Box<dyn Stream<Item = Result<Bytes, FetchError>> + Send>>;

/// Serves exactly one canned response.
struct OneShot {
    /// Content-Type value.
    content_type: http::HeaderValue,
    /// Body chunks.
    chunks: Vec<Bytes>,
}

impl Fetch for OneShot {
    type Body = Body;

    fn get(
        &self,
        _url: &Url,
        _accept: &'static str,
    ) -> impl Future<Output = Result<http::Response<Self::Body>, FetchError>> + Send {
        let chunks = self.chunks.clone();
        let content_type = self.content_type.clone();
        async move {
            let body: Body = Box::pin(stream::iter(chunks.into_iter().map(Ok)));
            let mut res = http::Response::new(body);
            res.headers_mut()
                .insert(http::header::CONTENT_TYPE, content_type);
            Ok(res)
        }
    }
}

/// Splits `html` at the cumulative `splits` boundaries (mod len, sorted,
/// deduped; empty chunks allowed). `len == 0` yields one empty chunk.
fn chop(splits: &[u16], html: &[u8]) -> Vec<Bytes> {
    if html.is_empty() {
        return vec![Bytes::new()];
    }
    let len = html.len();
    let mut points: Vec<usize> = Vec::with_capacity(splits.len());
    let mut acc = 0usize;
    for &s in splits {
        acc = acc.wrapping_add(usize::from(s)) % len;
        points.push(acc);
    }
    points.sort_unstable();
    points.dedup();
    points.push(len);
    let mut out = Vec::with_capacity(points.len());
    let mut prev = 0usize;
    let all = Bytes::copy_from_slice(html);
    for p in points {
        out.push(all.slice(prev..p));
        prev = p;
    }
    out
}

/// Builds the Content-Type header: `text/html` plus `charset=` when the
/// input carries a label that survives `HeaderValue` validation.
fn content_type(charset: &Charset) -> http::HeaderValue {
    let label = match charset {
        Charset::None => None,
        Charset::Known(i) => Some(KNOWN_CHARSETS[usize::from(*i) % KNOWN_CHARSETS.len()]),
        Charset::Raw(s) => Some(s.as_str()),
    };
    label
        .and_then(|l| {
            http::HeaderValue::from_str(&format!("text/html; charset={l}")).ok()
        })
        .unwrap_or_else(|| http::HeaderValue::from_static("text/html"))
}

/// Runs `preview` against a one-shot fetch serving `chunks`.
fn run(chunks: Vec<Bytes>, content_type: &http::HeaderValue) -> Result<Preview, corx::Error> {
    let fetch = OneShot {
        content_type: content_type.clone(),
        chunks,
    };
    let target = Target::parse("https://a.example/").expect("static valid target");
    let mut trace = Trace::default();
    futures_executor::block_on(corx::preview(&fetch, target, 1_700_000_000, &mut trace))
}

fuzz_target!(|input: Input| {
    let html = serialize(&input.doc);
    if html.len() > MAX_HTML {
        return;
    }
    let content_type = content_type(&input.charset);
    let chunked = run(chop(&input.splits, &html), &content_type);
    let unchunked = run(vec![Bytes::from(html)], &content_type);
    assert_eq!(
        chunked, unchunked,
        "same body must extract the same preview regardless of chunking"
    );
});
