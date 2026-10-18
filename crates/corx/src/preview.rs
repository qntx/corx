//! The preview pipeline: follow redirects, stream the head through the
//! extractor, resolve fields.

use futures_util::StreamExt;
use serde::Serialize;
use url::Url;

use crate::extract::{Extractor, Flow, Metadata};
use crate::{Error, Fetch, MAX_HTML_BYTES, MAX_REDIRECTS, MAX_URL_LEN, Target};

/// A resolved link preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
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

fn charset(content_type: Option<&str>) -> Option<&str> {
    content_type.and_then(|ct| {
        ct.split(';').skip(1).find_map(|param| {
            let (name, value) = param.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches('"'))
        })
    })
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
        let Some(essence) = content_type
            .as_deref()
            .and_then(|ct| ct.split(';').next())
            .map(|e| e.trim().to_ascii_lowercase())
        else {
            return Err(Error::NotHtml);
        };
        if essence != "text/html" && essence != "application/xhtml+xml" {
            return Err(Error::NotHtml);
        }
        let cs = charset(content_type.as_deref()).map(str::to_owned);
        let md = drain(response.into_body(), cs.as_deref()).await?;
        return build(&original, &current, md, now_unix_secs);
    }
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
    let image = meta(
        &md,
        &[
            "og:image:secure_url",
            "og:image:url",
            "og:image",
            "twitter:image",
            "twitter:image:src",
        ],
    )
    .and_then(|raw| resolve(raw, &base))
    .map(|url| Image {
        url,
        width: meta(&md, &["og:image:width"]).and_then(|v| v.parse().ok()),
        height: meta(&md, &["og:image:height"]).and_then(|v| v.parse().ok()),
        alt: meta(&md, &["og:image:alt", "twitter:image:alt"]).map(str::to_owned),
    });
    let favicon = md
        .icon
        .as_deref()
        .or(md.touch_icon.as_deref())
        .and_then(|raw| resolve(raw, &base));

    if title.is_none()
        && description.is_none()
        && site_name.is_none()
        && image.is_none()
        && favicon.is_none()
    {
        return Err(Error::NoMetadata);
    }

    Ok(Preview {
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
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use futures_core::Stream;
    use futures_util::stream;
    use http::{HeaderValue, StatusCode, header::CONTENT_TYPE, header::LOCATION};
    use url::Url;

    use super::{Image, preview};
    use crate::{Error, Fetch, FetchError, MAX_HTML_BYTES, Target};

    type Body = Pin<Box<dyn Stream<Item = Result<Bytes, FetchError>> + Send>>;

    fn block_on<F: Future>(fut: F) -> F::Output {
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    struct Fake {
        routes: HashMap<String, Result<http::Response<Vec<Bytes>>, FetchError>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                routes: HashMap::new(),
            }
        }

        fn respond(&mut self, url: &str, res: http::Response<Vec<Bytes>>) -> &mut Self {
            self.routes.insert(url.to_owned(), Ok(res));
            self
        }

        fn fail(&mut self, url: &str, err: FetchError) -> &mut Self {
            self.routes.insert(url.to_owned(), Err(err));
            self
        }
    }

    impl Fetch for Fake {
        type Body = Body;

        fn get(
            &self,
            url: &Url,
        ) -> impl Future<Output = Result<http::Response<Self::Body>, FetchError>> + Send {
            let out = match self.routes.get(url.as_str()) {
                Some(Ok(res)) => {
                    let (parts, chunks) = res.clone().into_parts();
                    let body: Body = Box::pin(stream::iter(chunks.into_iter().map(Ok)));
                    Ok(http::Response::from_parts(parts, body))
                }
                Some(Err(err)) => Err(*err),
                None => Err(FetchError::Unreachable),
            };
            std::future::ready(out)
        }
    }

    fn response(
        status: StatusCode,
        headers: &[(&'static str, &'static str)],
        body: &str,
    ) -> http::Response<Vec<Bytes>> {
        let mut res = http::Response::new(vec![Bytes::copy_from_slice(body.as_bytes())]);
        *res.status_mut() = status;
        for (name, value) in headers {
            res.headers_mut()
                .insert(*name, HeaderValue::from_static(value));
        }
        res
    }

    fn html(body: &str) -> http::Response<Vec<Bytes>> {
        response(
            StatusCode::OK,
            &[(CONTENT_TYPE.as_str(), "text/html")],
            body,
        )
    }

    fn redirect_to(location: &'static str) -> http::Response<Vec<Bytes>> {
        response(StatusCode::FOUND, &[(LOCATION.as_str(), location)], "")
    }

    fn target(raw: &str) -> Target {
        Target::parse(raw).expect("valid target")
    }

    const OG: &str = r##"<html lang="en"><head><title>Doc</title>
        <meta property="og:title" content="OGT"><meta property="og:image" content="/img.png">
        <meta property="og:image:width" content="640"><meta name="theme-color" content="#abc">
        <link rel="icon" href="favicon.ico"></head><body></body></html>"##;

    #[test]
    fn basic_og_page() {
        let mut fake = Fake::new();
        fake.respond("https://a.com/", html(OG));
        let p = block_on(preview(&fake, target("https://a.com"), 42)).expect("preview");
        assert_eq!(p.url, "https://a.com/");
        assert_eq!(p.final_url, "https://a.com/");
        assert_eq!(p.title.as_deref(), Some("OGT"));
        assert_eq!(p.lang.as_deref(), Some("en"));
        assert_eq!(p.theme_color.as_deref(), Some("#abc"));
        assert_eq!(p.favicon.as_deref(), Some("https://a.com/favicon.ico"));
        assert_eq!(
            p.image,
            Some(Image {
                url: "https://a.com/img.png".into(),
                width: Some(640),
                height: None,
                alt: None,
            })
        );
        assert_eq!(p.fetched_at, 42);
    }

    #[test]
    fn title_fallback_chain() {
        let mut fake = Fake::new();
        fake.respond(
            "https://a.com/",
            html("<head><meta name=\"twitter:title\" content=\"Tw\"><meta name=\"description\" content=\"d\"><title>TT</title></head>"),
        );
        let p = block_on(preview(&fake, target("https://a.com"), 0)).expect("preview");
        assert_eq!(p.title.as_deref(), Some("Tw"));
        assert_eq!(p.description.as_deref(), Some("d"));

        let mut fake2 = Fake::new();
        fake2.respond("https://a.com/", html("<head><title>TT</title></head>"));
        let p2 = block_on(preview(&fake2, target("https://a.com"), 0)).expect("preview");
        assert_eq!(p2.title.as_deref(), Some("TT"));
    }

    #[test]
    fn base_href_resolution() {
        let mut fake = Fake::new();
        fake.respond(
            "https://a.com/dir/page",
            html("<head><base href=\"/assets/\"><meta property=\"og:image\" content=\"i.png\"><link rel=\"icon\" href=\"f.ico\"></head>"),
        );
        let p = block_on(preview(&fake, target("https://a.com/dir/page"), 0)).expect("preview");
        assert_eq!(p.image.expect("image").url, "https://a.com/assets/i.png");
        assert_eq!(p.favicon.as_deref(), Some("https://a.com/assets/f.ico"));
        // A non-http base is ignored; resolution falls back to the final URL.
        let mut fake2 = Fake::new();
        fake2.respond(
            "https://a.com/",
            html("<head><base href=\"javascript:x\"><meta property=\"og:image\" content=\"/i.png\"></head>"),
        );
        let p2 = block_on(preview(&fake2, target("https://a.com"), 0)).expect("preview");
        assert_eq!(p2.image.expect("image").url, "https://a.com/i.png");
    }

    #[test]
    fn redirect_chain_followed() {
        let mut fake = Fake::new();
        fake.respond("https://a.com/", redirect_to("https://b.com/x"));
        fake.respond("https://b.com/x", redirect_to("/final"));
        fake.respond("https://b.com/final", html(OG));
        let p = block_on(preview(&fake, target("https://a.com"), 1)).expect("preview");
        assert_eq!(p.url, "https://a.com/");
        assert_eq!(p.final_url, "https://b.com/final");
    }

    #[test]
    fn redirect_to_blocked_target() {
        let mut fake = Fake::new();
        fake.respond("https://a.com/", redirect_to("http://127.0.0.1/"));
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::BlockedTarget)
        );
    }

    #[test]
    fn https_downgrade_redirect_blocked() {
        let mut fake = Fake::new();
        fake.respond("https://a.com/", redirect_to("http://a.com/x"));
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::BlockedTarget)
        );
    }

    #[test]
    fn too_many_redirects() {
        let mut fake = Fake::new();
        fake.respond("https://a.com/", redirect_to("https://a.com/"));
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::UpstreamStatus)
        );
    }

    #[test]
    fn redirect_without_location() {
        let mut fake = Fake::new();
        fake.respond("https://a.com/", response(StatusCode::FOUND, &[], ""));
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::UpstreamStatus)
        );
    }

    #[test]
    fn upstream_error_status() {
        let mut fake = Fake::new();
        fake.routes.insert(
            "https://a.com/".into(),
            Ok(response(StatusCode::NOT_FOUND, &[], "")),
        );
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::UpstreamStatus)
        );
    }

    #[test]
    fn not_html_variants() {
        let mut fake = Fake::new();
        fake.respond(
            "https://a.com/",
            response(StatusCode::OK, &[(CONTENT_TYPE.as_str(), "image/png")], "x"),
        );
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::NotHtml)
        );
        let mut fake2 = Fake::new();
        fake2.respond("https://a.com/", response(StatusCode::OK, &[], "x"));
        assert_eq!(
            block_on(preview(&fake2, target("https://a.com"), 0)),
            Err(Error::NotHtml)
        );
        // charset parameters still count as HTML
        let mut fake3 = Fake::new();
        fake3.respond(
            "https://a.com/",
            response(
                StatusCode::OK,
                &[(CONTENT_TYPE.as_str(), "text/html; Charset=UTF-8")],
                "<head><title>t</title></head>",
            ),
        );
        assert!(block_on(preview(&fake3, target("https://a.com"), 0)).is_ok());
    }

    #[test]
    fn oversized_body() {
        let mut fake = Fake::new();
        let big = " ".repeat(MAX_HTML_BYTES + 10);
        let doc = format!("<head><title>x</title>{big}");
        fake.respond("https://a.com/", html(&doc));
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn empty_page_is_no_metadata() {
        let mut fake = Fake::new();
        fake.respond("https://a.com/", html("<head></head><body>hi</body>"));
        assert_eq!(
            block_on(preview(&fake, target("https://a.com"), 0)),
            Err(Error::NoMetadata)
        );
    }

    #[test]
    fn fetch_error_mapping() {
        for (fe, want) in [
            (FetchError::Blocked, Error::BlockedTarget),
            (FetchError::Timeout, Error::UpstreamTimeout),
            (FetchError::Unreachable, Error::UpstreamUnreachable),
        ] {
            let mut fake = Fake::new();
            fake.fail("https://a.com/", fe);
            assert_eq!(
                block_on(preview(&fake, target("https://a.com"), 0)),
                Err(want)
            );
        }
    }

    #[test]
    fn chunked_body() {
        let mut fake = Fake::new();
        let mut res = http::Response::new(
            OG.as_bytes()
                .chunks(7)
                .map(Bytes::copy_from_slice)
                .collect::<Vec<_>>(),
        );
        *res.status_mut() = StatusCode::OK;
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
        fake.respond("https://a.com/", res);
        let p = block_on(preview(&fake, target("https://a.com"), 0)).expect("preview");
        assert_eq!(p.title.as_deref(), Some("OGT"));
    }
}
