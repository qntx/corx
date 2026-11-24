//! The oEmbed fallback: when a provider's own HTML is a bot wall (no
//! description, no image, or no answer at all), its oEmbed endpoint can
//! still describe the page. The exchange goes through the same [`Fetch`]
//! port and [`Target`] validation as any upstream hop.

use futures_util::StreamExt;
use serde::Deserialize;
use url::Url;

use crate::extract::{
    MAX_DESCRIPTION_CHARS, MAX_SITE_NAME_CHARS, MAX_TITLE_CHARS, clean_text, clean_url,
};
use crate::preview::resolve;
use crate::{
    ACCEPT_JSON, Error, Fetch, Image, MAX_OEMBED_BYTES, Page, Preview, Source, Target, Trace,
};

/// A site whose oEmbed endpoint is worth consulting.
struct Provider {
    /// Registered host; subdomains match too.
    host: &'static str,
    /// oEmbed endpoint base URL.
    endpoint: &'static str,
}

const PROVIDERS: &[Provider] = &[
    // reddit: post HTML is a bot wall titled "Reddit"; oEmbed returns the
    // post title + author (subreddit URLs get 400).
    Provider {
        host: "reddit.com",
        endpoint: "https://www.reddit.com/oembed",
    },
    // tiktok: HTML title is "TikTok - Make Your Day"; oEmbed returns the
    // profile/video title and author.
    Provider {
        host: "tiktok.com",
        endpoint: "https://www.tiktok.com/oembed",
    },
];

/// The provider for `target`'s host — exact or any subdomain.
fn provider_for(target: &Target) -> Option<&'static Provider> {
    let host = target.host();
    PROVIDERS.iter().find(|p| {
        host.strip_suffix(p.host)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
    })
}

/// The oEmbed request URL: the endpoint with `url` and `format=json`
/// query pairs, validated like any target.
fn request_url(provider: &Provider, page_url: &str) -> Option<Target> {
    let mut url = Url::parse(provider.endpoint).ok()?;
    url.query_pairs_mut()
        .append_pair("url", page_url)
        .append_pair("format", "json");
    Target::parse(url.as_str()).ok()
}

/// Whether the HTML outcome is poor enough to ask the provider instead.
const fn fallback_worthy(result: &Result<Preview, Error>) -> bool {
    match result {
        Err(Error::UpstreamStatus(_) | Error::NoMetadata) => true,
        Ok(Preview::Page(p)) => p.description.is_none() && p.image.is_none(),
        _ => false,
    }
}

/// A dimension as oEmbed providers emit it: a JSON number or a numeric
/// string; anything but a positive u32 is dropped.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Dim {
    /// A JSON number.
    Num(serde_json::Number),
    /// A numeric string.
    Str(String),
}

impl Dim {
    fn value(&self) -> Option<u32> {
        let n = match self {
            Self::Num(n) => n.as_u64()?,
            Self::Str(s) => s.parse::<u64>().ok()?,
        };
        u32::try_from(n).ok().filter(|&v| v > 0)
    }
}

/// The subset of an oEmbed response corx reads; unknown fields (`html`
/// included) are never deserialized.
#[derive(Debug, Deserialize)]
struct Response {
    /// Page title → `title`.
    title: Option<String>,
    /// Author → `description`.
    author_name: Option<String>,
    /// Provider name → `siteName`.
    provider_name: Option<String>,
    /// Thumbnail → `image` (with dimensions when sane).
    thumbnail_url: Option<String>,
    /// `image.width`.
    thumbnail_width: Option<Dim>,
    /// `image.height`.
    thumbnail_height: Option<Dim>,
}

/// The oEmbed exchange: one GET, no redirects, 2xx required, ≤
/// [`MAX_OEMBED_BYTES`], parseable as JSON, yielding a page that satisfies
/// the title-or-image rule. Any failure returns `None` and the caller
/// keeps the original HTML result.
#[allow(
    clippy::future_not_send,
    reason = "Send-ness is arch-conditional via MaybeSend/MaybeSync; wasm futures are intentionally !Send"
)]
async fn exchange<F: Fetch>(fetch: &F, target: &Target, trace: &mut Trace) -> Option<Page> {
    let res = fetch.get(target.as_url(), ACCEPT_JSON).await.ok()?;
    trace.oembed_status = Some(res.status());
    if !res.status().is_success() {
        return None;
    }
    let mut body = res.into_body();
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.ok()?;
        if buf.len() + chunk.len() > MAX_OEMBED_BYTES {
            return None;
        }
        buf.extend_from_slice(&chunk);
    }
    drop(body);
    let parsed: Response = serde_json::from_slice(&buf).ok()?;
    Some(map(&parsed, target.as_url()))
}

/// Maps a parsed oEmbed response onto a [`Page`]. `original`/`final_url`
/// are filled in by [`fallback`].
fn map(res: &Response, base: &Url) -> Page {
    let image = res
        .thumbnail_url
        .as_deref()
        .and_then(clean_url)
        .and_then(|u| resolve(&u, base))
        .map(|url| Image {
            url,
            width: res.thumbnail_width.as_ref().and_then(Dim::value),
            height: res.thumbnail_height.as_ref().and_then(Dim::value),
            alt: None,
        });
    Page {
        url: String::new(),
        final_url: String::new(),
        title: res
            .title
            .as_deref()
            .and_then(|t| clean_text(t, MAX_TITLE_CHARS)),
        description: res
            .author_name
            .as_deref()
            .and_then(|t| clean_text(t, MAX_DESCRIPTION_CHARS)),
        site_name: res
            .provider_name
            .as_deref()
            .and_then(|t| clean_text(t, MAX_SITE_NAME_CHARS)),
        image,
        favicon: None,
        card: None,
        kind: None,
        theme_color: None,
        lang: None,
        source: Source::Oembed,
        fetched_at: 0,
    }
}

/// The oEmbed fallback: runs when the final HTML hop's host has a provider
/// and the HTML result is an [`Error::UpstreamStatus`], an
/// [`Error::NoMetadata`], or a page with neither description nor image.
/// Returns the oEmbed page on success, the original result on any failure.
#[allow(
    clippy::future_not_send,
    reason = "Send-ness is arch-conditional via MaybeSend/MaybeSync; wasm futures are intentionally !Send"
)]
pub(crate) async fn fallback<F: Fetch>(
    fetch: &F,
    original: &str,
    final_hop: &Target,
    now_unix_secs: u64,
    html: Result<Preview, Error>,
    trace: &mut Trace,
) -> Result<Preview, Error> {
    let Some(provider) = provider_for(final_hop) else {
        return html;
    };
    if !fallback_worthy(&html) {
        return html;
    }
    let Some(target) = request_url(provider, final_hop.as_str()) else {
        return html;
    };
    match exchange(fetch, &target, trace).await {
        Some(mut page) if page.title.is_some() || page.image.is_some() => {
            original.clone_into(&mut page.url);
            final_hop.as_str().clone_into(&mut page.final_url);
            page.fetched_at = now_unix_secs;
            Ok(Preview::Page(page))
        }
        _ => html,
    }
}
