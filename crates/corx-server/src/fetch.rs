//! Native [`corx::Fetch`] adapter over reqwest with public-address DNS.

use core::error::Error as StdError;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::Stream;
use http::header;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::Url;

/// Marker error raised by [`PublicResolver`] when a hostname resolves to zero
/// public addresses.
#[derive(Debug)]
pub(crate) struct BlockedAddress;

impl core::fmt::Display for BlockedAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("hostname resolved to no public address")
    }
}

impl StdError for BlockedAddress {}

fn boxed<E: StdError + Send + Sync + 'static>(e: E) -> Box<dyn StdError + Send + Sync> {
    Box::new(e)
}

/// DNS resolver that filters every answer through [`corx::ip::is_public`].
///
/// reqwest connects only to the addresses returned here, so non-public
/// records never reach the socket layer (no DNS-rebinding TOCTOU).
#[derive(Debug)]
pub(crate) struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(boxed)?
                .filter(|addr| corx::ip::is_public(addr.ip()))
                .collect();
            if addrs.is_empty() {
                Err(boxed(BlockedAddress))
            } else {
                let it: Addrs = Box::new(addrs.into_iter());
                Ok(it)
            }
        })
    }
}

/// Response body stream mapped to [`corx::FetchError`].
pub(crate) type Body = Pin<Box<dyn Stream<Item = Result<Bytes, corx::FetchError>> + Send>>;

fn map_reqwest(e: &reqwest::Error) -> corx::FetchError {
    let mut src: Option<&(dyn StdError + 'static)> = Some(e);
    while let Some(err) = src {
        if err.is::<BlockedAddress>() {
            return corx::FetchError::Blocked;
        }
        src = err.source();
    }
    if e.is_timeout() {
        corx::FetchError::Timeout
    } else {
        corx::FetchError::Unreachable
    }
}

/// reqwest-backed fetch: no redirect following, no cookies, no proxy,
/// transparent decompression, validated DNS only.
#[derive(Debug, Clone)]
pub(crate) struct NativeFetch {
    client: reqwest::Client,
}

impl NativeFetch {
    /// Builds the client with the shared [`PublicResolver`].
    pub(crate) fn new(user_agent: &str) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(corx::CONNECT_TIMEOUT)
            .user_agent(user_agent)
            .referer(false)
            .dns_resolver(Arc::new(PublicResolver))
            .gzip(true)
            .brotli(true)
            .zstd(true)
            .deflate(true)
            .build()?;
        Ok(Self { client })
    }
}

impl corx::Fetch for NativeFetch {
    type Body = Body;

    async fn get(&self, url: &Url) -> Result<http::Response<Self::Body>, corx::FetchError> {
        let res = self
            .client
            .get(url.clone())
            .header(header::ACCEPT, corx::ACCEPT)
            .send()
            .await
            .map_err(|e| map_reqwest(&e))?;
        let status = res.status();
        let version = res.version();
        let headers = res.headers().clone();
        let body: Body = Box::pin(
            res.bytes_stream()
                .map(|item| item.map_err(|e| map_reqwest(&e))),
        );
        let mut out = http::Response::new(body);
        *out.status_mut() = status;
        *out.version_mut() = version;
        *out.headers_mut() = headers;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::Response;
    use axum::routing::get;
    use corx::Fetch;
    use tokio::net::TcpListener;

    use super::*;

    async fn spawn() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        async fn redirect() -> Response {
            let mut res = Response::new(axum::body::Body::empty());
            *res.status_mut() = StatusCode::FOUND;
            res.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_static("https://elsewhere.invalid/"),
            );
            res
        }

        async fn echo_headers(req: Request) -> Response {
            let ua = req
                .headers()
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let accept = req
                .headers()
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let mut res = Response::new(axum::body::Body::from(format!("{ua}|{accept}")));
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
            res
        }

        async fn gzipped() -> Response {
            // minimal gzip member wrapping "hello" (pre-compressed bytes)
            const GZ: &[u8] = &[
                0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x01, 0x05, 0x00, 0xfa,
                0xff, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00,
            ];
            let mut res = Response::new(axum::body::Body::from(GZ));
            res.headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
            res
        }

        let app = Router::new()
            .route("/redirect", get(redirect))
            .route("/echo", get(echo_headers))
            .route("/gzip", get(gzipped));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (addr, task)
    }

    fn fetch() -> NativeFetch {
        NativeFetch::new("corx-test/0.0.0").expect("client")
    }

    #[tokio::test]
    async fn redirect_is_returned_not_followed() {
        let (addr, task) = spawn().await;
        let url = Url::parse(&format!("http://{addr}/redirect")).expect("url");
        let res = fetch().get(&url).await.expect("response");
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(
            res.headers().get(header::LOCATION).expect("location"),
            "https://elsewhere.invalid/"
        );
        task.abort();
    }

    #[tokio::test]
    async fn sends_user_agent_and_accept() {
        let (addr, task) = spawn().await;
        let url = Url::parse(&format!("http://{addr}/echo")).expect("url");
        let res = fetch().get(&url).await.expect("response");
        let mut body = res.into_body();
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk.expect("chunk"));
        }
        let echoed = String::from_utf8(buf).expect("utf8");
        assert_eq!(echoed, format!("corx-test/0.0.0|{}", corx::ACCEPT));
        task.abort();
    }

    #[tokio::test]
    async fn gzip_body_is_decompressed() {
        let (addr, task) = spawn().await;
        let url = Url::parse(&format!("http://{addr}/gzip")).expect("url");
        let res = fetch().get(&url).await.expect("response");
        let mut body = res.into_body();
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(buf, b"hello");
        task.abort();
    }

    #[tokio::test]
    async fn non_public_hostname_is_blocked() {
        // "localhost" resolves to loopback only → the resolver yields
        // BlockedAddress → FetchError::Blocked.
        let url = Url::parse("http://localhost:9/").expect("url");
        assert_eq!(
            fetch().get(&url).await.err(),
            Some(corx::FetchError::Blocked)
        );
    }
}
