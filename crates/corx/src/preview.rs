//! The preview pipeline: follow redirects, stream the head through the
//! extractor, resolve fields.

use futures_util::StreamExt;
use serde::Serialize;
use url::Url;

use crate::extract::{Extractor, Flow, Metadata};
use crate::{Error, Fetch, MAX_HTML_BYTES, MAX_REDIRECTS, MAX_URL_LEN, Target};

/// A resolved link preview, tagged on `kind`.
#[allow(
    clippy::large_enum_variant,
    reason = "built once per request and never stored; boxing would only complicate the API"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Preview {
    /// An HTML page's metadata.
    Page(Page),
    /// A non-HTML resource described by its response headers.
    Media(Media),
}

/// A resolved HTML page preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    /// The requested URL, as validated.
    pub url: String,
    /// The URL of the final hop after redirects.
    pub final_url: String,
    /// `og:title` → `twitter:title` → `<title>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `og:description` → `twitter:description` → `description`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `og:site_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    /// Best-effort preview image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
    /// Icon link; never synthesized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    /// `twitter:card`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// `og:type`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `theme-color` meta.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_color: Option<String>,
    /// `html[lang]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Where the metadata came from.
    pub source: Source,
    /// Unix seconds when the final hop was fetched.
    pub fetched_at: u64,
}

/// Where a preview's metadata was extracted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The target's own HTML `<head>`.
    Html,
    /// An oEmbed endpoint (`page` only; produced by the oEmbed fallback).
    Oembed,
}

/// A resolved non-HTML resource preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Media {
    /// The requested URL, as validated.
    pub url: String,
    /// The URL of the final hop after redirects.
    pub final_url: String,
    /// The response's MIME essence, lowercased.
    pub media_type: String,
    /// `Content-Length`, present only when the body is unencoded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Unix seconds when the final hop was fetched.
    pub fetched_at: u64,
}

/// A resolved image candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Image {
    /// Absolute image URL.
    pub url: String,
    /// `og:image:width` parsed as `u32`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// `og:image:height` parsed as `u32`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// `og:image:alt` → `twitter:image:alt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
}

fn meta<'m>(md: &'m Metadata, keys: &[&str]) -> Option<&'m str> {
    keys.iter()
        .find_map(|key| md.meta.get(*key).map(String::as_str))
}

/// Resolves `raw` against `base`; keeps only http(s) URLs within
/// `MAX_URL_LEN`.
fn resolve(raw: &str, base: &Url) -> Option<String> {
    let url = base.join(raw).ok()?;
    if matches!(url.scheme(), "http" | "https") && url.as_str().len() <= MAX_URL_LEN {
        Some(String::from(url.as_str()))
    } else {
        None
    }
}

fn is_redirect(status: ::http::StatusCode) -> bool {
    matches!(
        status,
        ::http::StatusCode::MOVED_PERMANENTLY
            | ::http::StatusCode::FOUND
            | ::http::StatusCode::SEE_OTHER
            | ::http::StatusCode::TEMPORARY_REDIRECT
            | ::http::StatusCode::PERMANENT_REDIRECT
    )
}

/// Fetches `target` through `fetch`, following at most `MAX_REDIRECTS`
/// redirects, streaming at most `MAX_HTML_BYTES` of the body into the
/// extractor.
///
/// # Errors
/// Any [`Error`] the pipeline can hit: validation failures on redirect hops,
/// upstream transport problems, wrong media type, oversized or metadata-less
/// pages.
#[allow(
    clippy::future_not_send,
    reason = "Send-ness is arch-conditional via MaybeSend/MaybeSync; wasm futures are intentionally !Send"
)]
pub async fn preview<F: Fetch>(
    fetch: &F,
    target: Target,
    now_unix_secs: u64,
) -> Result<Preview, Error> {
    let original = target.as_str().to_owned();
    let mut current = target;
    let mut hops = 0usize;
    loop {
        let response = fetch.get(current.as_url()).await?;
        if is_redirect(response.status()) {
            hops += 1;
            let Some(location) = response
                .headers()
                .get(::http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(Error::UpstreamStatus);
            };
            if hops > MAX_REDIRECTS {
                return Err(Error::UpstreamStatus);
            }
            current = current.redirect(location)?;
            continue;
        }
        if !response.status().is_success() {
            return Err(Error::UpstreamStatus);
        }
        let content_type = response
            .headers()
            .get(::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mime = content_type
            .as_deref()
            .and_then(|ct| ct.parse::<mime::Mime>().ok());
        let Some(mime) = mime else {
            return Err(Error::UnsupportedContent);
        };
        if mime.essence_str() == "text/html" || mime.essence_str() == "application/xhtml+xml" {
            let cs = mime.get_param(mime::CHARSET).map(|c| c.as_str().to_owned());
            let md = drain(response.into_body(), cs.as_deref()).await?;
            return build(&original, &current, md, now_unix_secs);
        }
        // Not HTML: the resource itself is the preview — its body is never
        // read, so dropping the response closes the connection.
        let size = media_size(response.headers());
        let media = Preview::Media(Media {
            url: original,
            final_url: current.as_str().to_owned(),
            media_type: mime.essence_str().to_owned(),
            size,
            fetched_at: now_unix_secs,
        });
        drop(response);
        return Ok(media);
    }
}

/// `Content-Length` as `u64`, present only when the body is unencoded — an
/// encoded body's length would describe the compressed form.
fn media_size(headers: &::http::HeaderMap) -> Option<u64> {
    if let Some(encoding) = headers.get(::http::header::CONTENT_ENCODING)
        && !encoding.as_bytes().eq_ignore_ascii_case(b"identity")
    {
        return None;
    }
    headers
        .get(::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.trim().parse::<u64>().ok())
}

async fn drain<S>(mut body: S, charset: Option<&str>) -> Result<Metadata, Error>
where
    S: futures_core::Stream<Item = Result<bytes::Bytes, crate::FetchError>> + Unpin,
{
    let mut extractor = Extractor::new(charset);
    let mut total = 0usize;
    let mut flow = Flow::Continue;
    while flow == Flow::Continue {
        let Some(chunk) = body.next().await else {
            break;
        };
        let chunk = chunk?;
        let remaining = MAX_HTML_BYTES - total;
        let take = chunk.len().min(remaining);
        if take > 0 {
            flow = extractor.write(chunk.get(..take).unwrap_or(&chunk));
        }
        total += take;
        if flow == Flow::Continue && total >= MAX_HTML_BYTES {
            return Err(Error::TooLarge);
        }
    }
    Ok(extractor.finish())
}

const IMAGE_KEYS: [&str; 5] = [
    "og:image:secure_url",
    "og:image:url",
    "og:image",
    "twitter:image",
    "twitter:image:src",
];

/// `og:image:{width,height}` as a positive dimension; zero and malformed
/// values are dropped.
fn dim(md: &Metadata, key: &str) -> Option<u32> {
    meta(md, &[key])
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
}

fn build(original: &str, current: &Target, md: Metadata, now: u64) -> Result<Preview, Error> {
    let base = md
        .base
        .as_deref()
        .and_then(|b| current.as_url().join(b).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or_else(|| current.as_url().clone());

    let title = meta(&md, &["og:title", "twitter:title"])
        .or(md.title.as_deref())
        .map(str::to_owned);
    let description = meta(
        &md,
        &["og:description", "twitter:description", "description"],
    )
    .map(str::to_owned);
    let site_name = meta(&md, &["og:site_name"]).map(str::to_owned);
    let image = IMAGE_KEYS
        .iter()
        .find_map(|k| {
            md.meta
                .get(*k)
                .and_then(|raw| resolve(raw, &base))
                .map(|url| (*k, url))
        })
        .map(|(key, url)| {
            // og:image:width/height describe the og:image* URL only.
            let og = key.starts_with("og:image");
            Image {
                url,
                width: og.then(|| dim(&md, "og:image:width")).flatten(),
                height: og.then(|| dim(&md, "og:image:height")).flatten(),
                alt: meta(&md, &["og:image:alt", "twitter:image:alt"]).map(str::to_owned),
            }
        });
    let favicon = md
        .icon
        .as_deref()
        .or(md.touch_icon.as_deref())
        .and_then(|raw| resolve(raw, &base));

    // A page requires a title or an image; description/siteName/favicon
    // alone are not a preview.
    if title.is_none() && image.is_none() {
        return Err(Error::NoMetadata);
    }

    Ok(Preview::Page(Page {
        url: original.to_owned(),
        final_url: current.as_str().to_owned(),
        title,
        description,
        site_name,
        image,
        favicon,
        card: meta(&md, &["twitter:card"]).map(str::to_owned),
        kind: meta(&md, &["og:type"]).map(str::to_owned),
        theme_color: meta(&md, &["theme-color"]).map(str::to_owned),
        lang: md.lang,
        source: Source::Html,
        fetched_at: now,
    }))
}
