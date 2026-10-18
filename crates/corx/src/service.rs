#![cfg_attr(
    target_arch = "wasm32",
    allow(
        clippy::future_not_send,
        reason = "MaybeSend is vacuous on wasm32; worker futures are !Send by design"
    )
)]

//! The shared request pipeline: routing, origin check, rate admission,
//! caching, the timed preview fetch, response rendering and the per-request
//! [`Record`].
//!
//! Both adapters implement [`Runtime`] over their platform primitives
//! (governor + moka + tokio natively; Worker bindings + Cache API on
//! Cloudflare) so behaviour is identical everywhere.

use core::future::Future;
use core::net::IpAddr;
use core::time::Duration;

use bytes::Bytes;
use futures_util::future::{Either, select};
use http::header::RETRY_AFTER;
use http::{HeaderValue, Method, StatusCode, Uri};

use crate::fetch::Fetch;
use crate::http::{Origins, Route, render, route};
use crate::ip::rate_key;
use crate::maybe_send::{MaybeSend, MaybeSync};
use crate::preview::preview;
use crate::{Error, TOTAL_TIMEOUT};

/// Client-IP bucket key when the client IP cannot be extracted: unidentifiable
/// clients share one bucket rather than bypassing the limit.
const UNKNOWN_CLIENT_KEY: &str = "::";

/// Which admission bucket a check applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// Per-client-IP limit.
    Client,
    /// Per-target-host limit.
    Host,
}

/// The runtime's verdict on an admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Request may proceed.
    Allowed,
    /// Limit exceeded; `retry_after_secs` fills the `Retry-After` header.
    Denied {
        /// Seconds until the bucket refills.
        retry_after_secs: u64,
    },
}

/// Whether a preview response came from the shared cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    /// Served from cache.
    Hit,
    /// Freshly fetched (possibly then stored).
    Miss,
}

impl CacheStatus {
    /// Stable label for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
        }
    }
}

/// A freshly computed (cacheable or single-use) rendered response.
#[derive(Debug, Clone)]
pub struct Fresh {
    /// CORS-free rendered response.
    pub response: http::Response<Bytes>,
    /// Log/metric outcome: `"ok"` or an error slug.
    pub outcome: &'static str,
    /// Whether the runtime may store the response.
    pub cacheable: bool,
}

/// Result of a runtime cache lookup.
#[derive(Debug)]
pub enum Cached {
    /// Stored response; CORS still needs to be applied by the pipeline.
    Hit(http::Response<Bytes>),
    /// The miss future resolved.
    Miss(Fresh),
}

/// Platform services the pipeline needs; implemented by each adapter.
pub trait Runtime: MaybeSync {
    /// Upstream fetcher.
    type Fetch: Fetch;

    /// The fetcher used for upstream hops.
    fn fetcher(&self) -> &Self::Fetch;

    /// Current Unix time in seconds.
    fn now_unix_secs(&self) -> u64;

    /// Resolves after `duration`; used as the total-budget race.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + MaybeSend;

    /// Admission check for `bucket`/`key`.
    fn admit(&self, bucket: Bucket, key: &str) -> impl Future<Output = Admission> + MaybeSend;

    /// Shared-cache lookup: returns the stored (CORS-free) response for `key`,
    /// or awaits `miss` and stores its response when [`Fresh::cacheable`].
    fn cached<M>(&self, key: &str, miss: M) -> impl Future<Output = Cached> + MaybeSend
    where
        M: Future<Output = Fresh> + MaybeSend;
}

/// One incoming request, already parsed into platform-neutral parts.
#[derive(Debug)]
pub struct Incoming<'a> {
    /// HTTP method.
    pub method: &'a Method,
    /// Request URI (path + query used for routing).
    pub uri: &'a Uri,
    /// `Origin` header when present.
    pub origin: Option<&'a HeaderValue>,
    /// Client IP for rate limiting; `None` shares the `::` bucket.
    pub client_ip: Option<IpAddr>,
}

/// Per-request observability record; adapters log/meter it once.
#[derive(Debug)]
pub struct Record {
    /// `"ok"`, `"preflight"`, `"cached"`, or an error slug.
    pub outcome: &'static str,
    /// Response status.
    pub status: StatusCode,
    /// Cache verdict for preview requests.
    pub cache: Option<CacheStatus>,
    /// Target host for preview requests.
    pub target_host: Option<String>,
    /// Privacy-preserving target fingerprint.
    pub target_fingerprint: Option<String>,
}

/// Renders a rate-limit denial: `429` plus `Retry-After` (at least one
/// second). No CORS — [`handle`] stamps it on every outgoing response.
fn rate_limited(retry_after_secs: u64) -> http::Response<Bytes> {
    let mut res = render(&Err(Error::RateLimited));
    res.headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(retry_after_secs.max(1)));
    res
}

/// The miss side of [`Runtime::cached`]: host admission, then the timed
/// preview. Rate denials render but are never cacheable.
async fn miss<R: Runtime>(rt: &R, target: crate::Target) -> Fresh {
    if let Admission::Denied { retry_after_secs } = rt.admit(Bucket::Host, target.host()).await {
        return Fresh {
            response: rate_limited(retry_after_secs),
            outcome: Error::RateLimited.slug(),
            cacheable: false,
        };
    }
    let work = preview(rt.fetcher(), target, rt.now_unix_secs());
    let timeout = rt.sleep(TOTAL_TIMEOUT);
    let mut work = core::pin::pin!(work);
    let mut timeout = core::pin::pin!(timeout);
    let result = match select(work.as_mut(), timeout.as_mut()).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(Error::UpstreamTimeout),
    };
    let outcome = result.as_ref().map_or_else(|e| e.slug(), |_| "ok");
    let cacheable = result.as_ref().map_or_else(|e| e.is_cacheable(), |_| true);
    Fresh {
        response: render(&result),
        outcome,
        cacheable,
    }
}

/// Runs the full request pipeline and returns the response plus the
/// observability record.
///
/// The response never carries CORS state in the cache; `Access-Control-*` is
/// applied to every outgoing response here. Every failure is rendered into
/// the response; this function never fails.
pub async fn handle<R: Runtime>(
    rt: &R,
    origins: &Origins,
    req: Incoming<'_>,
) -> (http::Response<Bytes>, Record) {
    let (mut response, outcome, cache, target) = 'pipeline: {
        let routed = match route(req.method, req.uri) {
            Err(e) => break 'pipeline (render(&Err(e)), e.slug(), None, None),
            Ok(routed) => routed,
        };
        if let Err(e) = origins.check(req.origin) {
            break 'pipeline (render(&Err(e)), e.slug(), None, None);
        }
        let Route::Preview(target) = routed else {
            break 'pipeline (origins.preflight(req.origin), "preflight", None, None);
        };

        let client_key = req.client_ip.map_or_else(
            || UNKNOWN_CLIENT_KEY.to_owned(),
            |ip| rate_key(ip).to_string(),
        );
        if let Admission::Denied { retry_after_secs } = rt.admit(Bucket::Client, &client_key).await
        {
            let info = (target.host().to_owned(), target.fingerprint());
            break 'pipeline (
                rate_limited(retry_after_secs),
                Error::RateLimited.slug(),
                None,
                Some(info),
            );
        }

        let host = target.host().to_owned();
        let fingerprint = target.fingerprint();
        let key = target.as_str().to_owned();
        match rt.cached(&key, miss(rt, target)).await {
            Cached::Hit(res) => (
                res,
                "cached",
                Some(CacheStatus::Hit),
                Some((host, fingerprint)),
            ),
            Cached::Miss(fresh) => (
                fresh.response,
                fresh.outcome,
                Some(CacheStatus::Miss),
                Some((host, fingerprint)),
            ),
        }
    };
    origins.apply(req.origin, response.headers_mut());
    let record = Record {
        outcome,
        status: response.status(),
        cache,
        target_host: target.as_ref().map(|(host, _)| host.clone()),
        target_fingerprint: target.map(|(_, fingerprint)| fingerprint),
    };
    (response, record)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_core::Stream;
    use futures_util::stream;
    use http::header::CONTENT_TYPE;
    use http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, ALLOW, VARY};
    use url::Url;

    use super::*;
    use crate::FetchError;

    type Body = Pin<Box<dyn Stream<Item = Result<Bytes, FetchError>> + Send>>;

    struct FakeFetch {
        calls: AtomicUsize,
        routes: HashMap<String, http::Response<Vec<Bytes>>>,
        hang: bool,
    }

    impl Fetch for FakeFetch {
        type Body = Body;

        fn get(
            &self,
            url: &Url,
        ) -> impl Future<Output = Result<http::Response<Self::Body>, FetchError>> + Send {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.hang {
                return Either::Left(std::future::pending());
            }
            let out = self
                .routes
                .get(url.as_str())
                .cloned()
                .map(|res| {
                    let (parts, chunks) = res.into_parts();
                    let body: Body = Box::pin(stream::iter(chunks.into_iter().map(Ok)));
                    http::Response::from_parts(parts, body)
                })
                .ok_or(FetchError::Unreachable);
            Either::Right(std::future::ready(out))
        }
    }

    struct FakeRt {
        fetcher: FakeFetch,
        cache: Mutex<HashMap<String, http::Response<Bytes>>>,
        denied: Mutex<HashMap<(bool, String), u64>>,
        admit_keys: Mutex<Vec<(Bucket, String)>>,
        sleep_ready: bool,
    }

    impl FakeRt {
        fn new() -> Self {
            Self {
                fetcher: FakeFetch {
                    calls: AtomicUsize::new(0),
                    routes: HashMap::new(),
                    hang: false,
                },
                cache: Mutex::new(HashMap::new()),
                denied: Mutex::new(HashMap::new()),
                admit_keys: Mutex::new(Vec::new()),
                sleep_ready: false,
            }
        }

        fn deny(&self, bucket: Bucket, key: &str, secs: u64) {
            self.denied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((bucket == Bucket::Host, key.to_owned()), secs);
        }

        fn keys(&self, bucket: Bucket) -> Vec<String> {
            self.admit_keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|(b, _)| *b == bucket)
                .map(|(_, k)| k.clone())
                .collect()
        }

        fn calls(&self) -> usize {
            self.fetcher.calls.load(Ordering::Relaxed)
        }
    }

    impl Runtime for FakeRt {
        type Fetch = FakeFetch;

        fn fetcher(&self) -> &FakeFetch {
            &self.fetcher
        }

        fn now_unix_secs(&self) -> u64 {
            42
        }

        fn sleep(&self, _duration: Duration) -> impl Future<Output = ()> + Send {
            if self.sleep_ready {
                Either::Left(std::future::ready(()))
            } else {
                Either::Right(std::future::pending())
            }
        }

        fn admit(&self, bucket: Bucket, key: &str) -> impl Future<Output = Admission> + Send {
            self.admit_keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((bucket, key.to_owned()));
            let denied = self
                .denied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&(bucket == Bucket::Host, key.to_owned()))
                .copied();
            std::future::ready(denied.map_or(Admission::Allowed, |retry_after_secs| {
                Admission::Denied { retry_after_secs }
            }))
        }

        async fn cached<M>(&self, key: &str, miss: M) -> Cached
        where
            M: Future<Output = Fresh> + Send,
        {
            let existing = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
                .cloned();
            if let Some(res) = existing {
                return Cached::Hit(res);
            }
            let fresh = miss.await;
            if fresh.cacheable {
                self.cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key.to_owned(), fresh.response.clone());
            }
            Cached::Miss(fresh)
        }
    }

    fn page(title: &str) -> http::Response<Vec<Bytes>> {
        let mut res = http::Response::new(vec![Bytes::from(format!(
            "<html><head><title>{title}</title></head><body/></html>"
        ))]);
        *res.status_mut() = StatusCode::OK;
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
        res
    }

    const ORIGINS: &str = "https://app.example";
    const URI: &str = "https://svc/v1/preview?url=https%3A%2F%2Fa.com%2F";

    fn incoming<'a>(
        method: &'a Method,
        uri: &'a Uri,
        origin: Option<&'a HeaderValue>,
        ip: Option<IpAddr>,
    ) -> Incoming<'a> {
        Incoming {
            method,
            uri,
            origin,
            client_ip: ip,
        }
    }

    fn fixture() -> (FakeRt, Origins, http::Response<Vec<Bytes>>) {
        let mut rt = FakeRt::new();
        rt.fetcher
            .routes
            .insert("https://a.com/".into(), page("Hi"));
        (rt, Origins::parse(ORIGINS).expect("origins"), page("Hi"))
    }

    #[test]
    fn forbidden_origin_never_fetches() {
        let (rt, origins, _) = fixture();
        let method = Method::GET;
        let uri: Uri = URI.parse().expect("uri");
        let origin = HeaderValue::from_static("https://evil.example");
        let (res, rec) = futures_executor::block_on(handle(
            &rt,
            &origins,
            incoming(&method, &uri, Some(&origin), None),
        ));
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(rec.outcome, "forbidden_origin");
        assert_eq!(rt.calls(), 0);
    }

    #[test]
    fn preflight_and_405() {
        let (rt, origins, _) = fixture();
        let method = Method::OPTIONS;
        let uri: Uri = "https://svc/v1/preview".parse().expect("uri");
        let (res, rec) =
            futures_executor::block_on(handle(&rt, &origins, incoming(&method, &uri, None, None)));
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(rec.outcome, "preflight");

        let post = Method::POST;
        let uri2: Uri = URI.parse().expect("uri");
        let (denied, rec2) =
            futures_executor::block_on(handle(&rt, &origins, incoming(&post, &uri2, None, None)));
        assert_eq!(denied.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            denied.headers().get(ALLOW).expect("allow"),
            "GET, HEAD, OPTIONS"
        );
        assert_eq!(rec2.outcome, "method_not_allowed");
    }

    #[test]
    fn client_429_has_retry_after_and_cors() {
        let (rt, origins, _) = fixture();
        rt.deny(Bucket::Client, "203.0.115.9", 7);
        let method = Method::GET;
        let uri: Uri = URI.parse().expect("uri");
        let origin = HeaderValue::from_static("https://app.example");
        let (res, rec) = futures_executor::block_on(handle(
            &rt,
            &origins,
            incoming(
                &method,
                &uri,
                Some(&origin),
                Some(IpAddr::V4(Ipv4Addr::new(203, 0, 115, 9))),
            ),
        ));
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers().get(RETRY_AFTER).expect("retry"), "7");
        assert_eq!(
            res.headers()
                .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                .expect("acao"),
            "https://app.example"
        );
        assert_eq!(rec.outcome, "rate_limited");
        assert_eq!(rt.calls(), 0);
    }

    #[test]
    fn missing_client_ip_shares_bucket() {
        let (rt, origins, _) = fixture();
        let method = Method::GET;
        let uri: Uri = URI.parse().expect("uri");
        let _ =
            futures_executor::block_on(handle(&rt, &origins, incoming(&method, &uri, None, None)));
        assert_eq!(rt.keys(Bucket::Client), ["::".to_owned()]);
    }

    #[test]
    fn host_429_is_not_cached() {
        let (rt, origins, _) = fixture();
        rt.deny(Bucket::Host, "a.com", 30);
        let method = Method::GET;
        let uri: Uri = URI.parse().expect("uri");
        for _ in 0..2 {
            let (res, _) = futures_executor::block_on(handle(
                &rt,
                &origins,
                incoming(&method, &uri, None, None),
            ));
            assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(res.headers().get(RETRY_AFTER).expect("retry"), "30");
        }
        assert_eq!(rt.calls(), 0);
    }

    #[test]
    fn success_is_cached_then_hit_carries_cors() {
        let (rt, origins, _) = fixture();
        let method = Method::GET;
        let uri: Uri = URI.parse().expect("uri");
        let origin = HeaderValue::from_static("https://app.example");
        let (res, rec) = futures_executor::block_on(handle(
            &rt,
            &origins,
            incoming(&method, &uri, Some(&origin), None),
        ));
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(rec.cache, Some(CacheStatus::Miss));
        assert_eq!(rec.target_host.as_deref(), Some("a.com"));
        assert!(rec.target_fingerprint.is_some());

        let (res2, rec2) = futures_executor::block_on(handle(
            &rt,
            &origins,
            incoming(&method, &uri, Some(&origin), None),
        ));
        assert_eq!(res2.status(), StatusCode::OK);
        assert_eq!(rec2.outcome, "cached");
        assert_eq!(rec2.cache, Some(CacheStatus::Hit));
        assert_eq!(
            res2.headers()
                .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                .expect("acao"),
            "https://app.example"
        );
        assert_eq!(res2.headers().get(VARY).expect("vary"), "Origin");
        assert_eq!(rt.calls(), 1, "hit must not refetch");
    }

    #[test]
    fn no_metadata_cached_unreachable_not() {
        let (mut rt, origins, _) = fixture();
        rt.fetcher.routes.insert(
            "https://a.com/".into(),
            page(""), // empty title → also no meta → NoMetadata
        );
        let method = Method::GET;
        let uri: Uri = URI.parse().expect("uri");
        let (not_found, _) =
            futures_executor::block_on(handle(&rt, &origins, incoming(&method, &uri, None, None)));
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        let (_, rec) =
            futures_executor::block_on(handle(&rt, &origins, incoming(&method, &uri, None, None)));
        assert_eq!(rec.outcome, "cached");
        assert_eq!(rt.calls(), 1);

        // unreachable → 502, never cached
        let (mut rt2, _, _) = fixture();
        rt2.fetcher.routes.clear();
        for _ in 0..2 {
            let (resp, _) = futures_executor::block_on(handle(
                &rt2,
                &origins,
                incoming(&method, &uri, None, None),
            ));
            assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        }
        assert_eq!(rt2.calls(), 2);
    }

    #[test]
    fn timeout_is_504_and_not_cached() {
        let (mut rt, origins, _) = fixture();
        rt.fetcher.hang = true;
        rt.sleep_ready = true;
        let method = Method::GET;
        let uri: Uri = URI.parse().expect("uri");
        let (res, rec) =
            futures_executor::block_on(handle(&rt, &origins, incoming(&method, &uri, None, None)));
        assert_eq!(res.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(rec.outcome, "upstream_timeout");
        rt.sleep_ready = false;
        rt.fetcher.hang = false;
        // next request misses again — nothing was stored
        let (res2, rec2) =
            futures_executor::block_on(handle(&rt, &origins, incoming(&method, &uri, None, None)));
        assert_eq!(res2.status(), StatusCode::OK);
        assert_eq!(rec2.cache, Some(CacheStatus::Miss));
    }
}
