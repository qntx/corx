//! Axum adapter: routing, CORS, rate limits, cache, metrics and the
//! privacy-preserving request log.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use axum::routing::get;
use axum_client_ip::{ClientIp, ClientIpSource, Rejection};
use corx::http::{Origins, Route, render, route};
use corx::{Error, Fetch, Preview, Target, preview};
use governor::clock::{Clock, DefaultClock};
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use moka::future::Cache;

/// Cache value: a preview result or a cacheable error.
pub(crate) type Outcome = Result<Preview, Error>;

type IpLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock, NoOpMiddleware>;
type HostLimiter =
    RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock, NoOpMiddleware>;
type PreviewCache = Cache<String, Outcome>;

const SUCCESS_TTL: Duration = Duration::from_hours(24);
const CACHEABLE_ERROR_TTL: Duration = Duration::from_secs(3600);

/// Tunable limits, supplied from [`crate::config::Config`].
#[derive(Debug, Clone)]
pub(crate) struct Limits {
    /// Sustained per-client-IP requests per minute.
    pub(crate) ip_rate_per_minute: u32,
    /// Per-client-IP burst size.
    pub(crate) ip_burst: u32,
    /// Per-target-host requests per minute.
    pub(crate) host_rate_per_minute: u32,
    /// Max concurrent requests before load shedding.
    pub(crate) max_concurrency: usize,
    /// Preview cache capacity in entries.
    pub(crate) cache_capacity: u64,
    /// Client-IP extraction strategy.
    pub(crate) client_ip_source: ClientIpSource,
}

struct OutcomeExpiry;

impl moka::Expiry<String, Outcome> for OutcomeExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Outcome,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(match value {
            Ok(_) => SUCCESS_TTL,
            Err(_) => CACHEABLE_ERROR_TTL,
        })
    }
}

/// Shared application state.
pub(crate) struct App<F> {
    origins: Origins,
    fetch: F,
    ip_limiter: Arc<IpLimiter>,
    host_limiter: Arc<HostLimiter>,
    cache: PreviewCache,
}

impl<F> core::fmt::Debug for App<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("App").finish_non_exhaustive()
    }
}

/// Builds the full router: health route plus a fallback that implements the
/// whole `/v1/preview` pipeline.
pub(crate) fn router<F>(origins: Origins, fetch: F, limits: &Limits) -> Router
where
    F: Fetch + Send + Sync + 'static,
{
    let nz = |v: u32| std::num::NonZeroU32::new(v).unwrap_or(std::num::NonZeroU32::MIN);
    let ip_limiter = Arc::new(RateLimiter::keyed(
        Quota::per_minute(nz(limits.ip_rate_per_minute)).allow_burst(nz(limits.ip_burst)),
    ));
    let host_limiter = Arc::new(RateLimiter::keyed(Quota::per_minute(nz(
        limits.host_rate_per_minute
    ))));
    spawn_retain(&ip_limiter, &host_limiter);
    let app = Arc::new(App {
        origins,
        fetch,
        ip_limiter,
        host_limiter,
        cache: Cache::builder()
            .max_capacity(limits.cache_capacity)
            .expire_after(OutcomeExpiry)
            .build(),
    });
    Router::new()
        .route("/healthz", get(healthz))
        .fallback(handle::<F>)
        .layer(limits.client_ip_source.clone().into_extension())
        .layer(
            tower::ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_: axum::BoxError| async {
                    to_axum(render(&Err(Error::Overloaded)))
                }))
                .load_shed()
                .concurrency_limit(limits.max_concurrency),
        )
        .with_state(app)
}

async fn healthz() -> &'static str {
    "ok"
}

/// Periodically prunes stale limiter keys; spawned when a runtime is active.
fn spawn_retain(ip: &Arc<IpLimiter>, host: &Arc<HostLimiter>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let ip = Arc::clone(ip);
    let host = Arc::clone(host);
    rt.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            ip.retain_recent();
            host.retain_recent();
        }
    });
}

fn to_axum(res: http::Response<bytes::Bytes>) -> Response {
    res.map(Body::from)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn render_error<F>(st: &App<F>, error: Error, origin: Option<&HeaderValue>) -> Response {
    let mut res = to_axum(render(&Err(error)));
    st.origins.apply(origin, res.headers_mut());
    res
}

struct RequestLog {
    outcome: &'static str,
    status: u16,
    cache: Option<&'static str>,
    host: Option<String>,
    fingerprint: Option<String>,
}

impl RequestLog {
    fn emit(&self, method: &Method, path: &str, started: Instant) {
        metrics::counter!("corx_requests_total", "outcome" => self.outcome).increment(1);
        tracing::info!(
            method = %method,
            path,
            outcome = self.outcome,
            status = self.status,
            latency_ms = started.elapsed().as_millis(),
            cache = self.cache.unwrap_or("n/a"),
            target_host = self.host.as_deref().unwrap_or("n/a"),
            target_fingerprint = self.fingerprint.as_deref().unwrap_or("n/a"),
            "request"
        );
    }
}

async fn handle<F>(
    State(st): State<Arc<App<F>>>,
    client_ip: Result<ClientIp, Rejection>,
    req: Request,
) -> Response
where
    F: Fetch + Send + Sync + 'static,
{
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let origin = req.headers().get(header::ORIGIN).cloned();

    let mut log = RequestLog {
        outcome: "ok",
        status: 200,
        cache: None,
        host: None,
        fingerprint: None,
    };

    let res = match route(&method, req.uri()) {
        Err(e) => {
            log.outcome = e.slug();
            log.status = e.status().as_u16();
            render_error(&st, e, origin.as_ref())
        }
        Ok(routed) => match st.origins.check(origin.as_ref()) {
            Err(e) => {
                log.outcome = e.slug();
                log.status = e.status().as_u16();
                render_error(&st, e, origin.as_ref())
            }
            Ok(()) => match routed {
                Route::Preflight => {
                    log.outcome = "preflight";
                    log.status = 204;
                    to_axum(st.origins.preflight(origin.as_ref()))
                }
                Route::Preview(target) => {
                    log.host = Some(target.host().to_owned());
                    log.fingerprint = Some(target.fingerprint());
                    preview_path(&st, &mut log, client_ip, target, origin.as_ref()).await
                }
            },
        },
    };
    log.emit(&method, &path, started);
    res
}

/// Bucket used when the client IP cannot be extracted: unidentifiable
/// clients share one rate-limit key rather than bypassing the limit.
const UNKNOWN_IP: IpAddr = IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);

async fn preview_path<F>(
    st: &Arc<App<F>>,
    log: &mut RequestLog,
    client_ip: Result<ClientIp, Rejection>,
    target: Target,
    origin: Option<&HeaderValue>,
) -> Response
where
    F: Fetch + Send + Sync + 'static,
{
    let ip_key = client_ip.map_or(UNKNOWN_IP, |ClientIp(ip)| corx::ip::rate_key(ip));
    if let Err(not_until) = st.ip_limiter.check_key(&ip_key) {
        let retry = not_until
            .wait_time_from(DefaultClock::default().now())
            .as_secs();
        let e = Error::RateLimited;
        log.outcome = e.slug();
        log.status = e.status().as_u16();
        let mut res = render_error(st, e, origin);
        res.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(retry.max(1)));
        return res;
    }

    let key = target.as_str().to_owned();
    let (outcome, hit) = if let Some(outcome) = st.cache.get(&key).await {
        (outcome, true)
    } else {
        match st.cache.try_get_with(key, miss(st, target)).await {
            Ok(outcome) => (outcome, false),
            Err(e) => (Err(*e), false),
        }
    };
    log.cache = Some(if hit { "hit" } else { "miss" });
    metrics::counter!("corx_cache_total", "result" => if hit { "hit" } else { "miss" })
        .increment(1);

    let slug = match &outcome {
        Ok(_) => "ok",
        Err(e) => e.slug(),
    };
    log.outcome = slug;
    log.status = match &outcome {
        Ok(_) => 200,
        Err(e) => e.status().as_u16(),
    };

    let mut res = to_axum(render(&outcome));
    st.origins.apply(origin, res.headers_mut());
    res
}

/// The cache miss path: per-host limit, total-timeout race, and classification
/// of which errors may be cached.
async fn miss<F>(st: &Arc<App<F>>, target: Target) -> Result<Outcome, Error>
where
    F: Fetch + Send + Sync + 'static,
{
    if st
        .host_limiter
        .check_key(&target.host().to_owned())
        .is_err()
    {
        return Err(Error::RateLimited);
    }
    let started = Instant::now();
    let result =
        tokio::time::timeout(corx::TOTAL_TIMEOUT, preview(&st.fetch, target, now_secs())).await;
    metrics::histogram!("corx_upstream_duration_seconds").record(started.elapsed().as_secs_f64());
    match result {
        Ok(Ok(p)) => Ok(Ok(p)),
        Ok(Err(e)) if e.is_cacheable() => Ok(Err(e)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::UpstreamTimeout),
    }
}

#[cfg(test)]
mod tests {
    use core::pin::Pin;
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, PoisonError};

    use axum::extract::connect_info::ConnectInfo;
    use bytes::Bytes;
    use futures_util::Stream;
    use futures_util::stream;
    use http::Request as HttpRequest;
    use http::{Method, StatusCode};
    use tower::ServiceExt;
    use url::Url;

    use super::*;

    type FakeBody = Pin<Box<dyn Stream<Item = Result<Bytes, corx::FetchError>> + Send>>;
    type FakeResp = http::Response<Vec<Bytes>>;

    #[derive(Debug, Default)]
    struct Shared {
        routes: Mutex<HashMap<String, FakeResp>>,
        calls: AtomicUsize,
    }

    #[derive(Debug, Clone, Default)]
    struct Fake(Arc<Shared>);

    impl Fake {
        fn respond(&self, url: &str, res: FakeResp) {
            self.0
                .routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(url.to_owned(), res);
        }

        fn calls(&self) -> usize {
            self.0.calls.load(Ordering::Relaxed)
        }
    }

    impl Fetch for Fake {
        type Body = FakeBody;

        fn get(
            &self,
            url: &Url,
        ) -> impl Future<Output = Result<http::Response<Self::Body>, corx::FetchError>> + Send
        {
            self.0.calls.fetch_add(1, Ordering::Relaxed);
            let out = self
                .0
                .routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(url.as_str())
                .cloned()
                .map(|res| {
                    let (parts, chunks) = res.into_parts();
                    let body: FakeBody = Box::pin(stream::iter(chunks.into_iter().map(Ok)));
                    http::Response::from_parts(parts, body)
                });
            std::future::ready(out.ok_or(corx::FetchError::Unreachable))
        }
    }

    fn page(title: &str) -> FakeResp {
        let mut res = http::Response::new(vec![Bytes::from(format!(
            "<html><head><title>{title}</title></head><body/></html>"
        ))]);
        *res.status_mut() = StatusCode::OK;
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        res
    }

    const CLIENT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 115, 9)), 9);

    fn test_limits() -> Limits {
        Limits {
            ip_rate_per_minute: 60,
            ip_burst: 8,
            host_rate_per_minute: 4,
            max_concurrency: 64,
            cache_capacity: 1024,
            client_ip_source: ClientIpSource::ConnectInfo,
        }
    }

    fn test_app() -> (Router, Fake) {
        let fake = Fake::default();
        let r = router(
            Origins::parse("https://app.example").expect("origins"),
            fake.clone(),
            &test_limits(),
        );
        (r, fake)
    }

    fn req(method: Method, uri: &str) -> HttpRequest<Body> {
        let mut r = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("req");
        r.extensions_mut().insert(ConnectInfo(CLIENT));
        r
    }

    async fn send(app: &Router, r: HttpRequest<Body>) -> Response {
        app.clone().oneshot(r).await.expect("response")
    }

    async fn send_origin(app: &Router, r: HttpRequest<Body>, origin: &str) -> Response {
        let mut r = r;
        r.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(origin).expect("origin"),
        );
        send(app, r).await
    }

    async fn body_json(res: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    const GOOD_URL: &str = "/v1/preview?url=https%3A%2F%2Fa.com%2F";

    #[tokio::test]
    async fn success_json_and_cors() {
        let (app, fake) = test_app();
        fake.respond("https://a.com/", page("Hello"));
        let res = send_origin(&app, req(Method::GET, GOOD_URL), "https://app.example").await;
        assert_eq!(res.status(), StatusCode::OK);
        let h = res.headers();
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).expect("acao"),
            "https://app.example"
        );
        assert_eq!(h.get(header::VARY).expect("vary"), "Origin");
        assert_eq!(
            h.get(header::CACHE_CONTROL).expect("cc"),
            corx::CACHE_CONTROL_SUCCESS
        );
        let json = body_json(res).await;
        assert_eq!(json.get("title").and_then(|v| v.as_str()), Some("Hello"));
        assert_eq!(
            json.get("url").and_then(|v| v.as_str()),
            Some("https://a.com/")
        );
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn forbidden_and_absent_origin() {
        let (app, fake) = test_app();
        fake.respond("https://a.com/", page("Hello"));
        let res = send_origin(&app, req(Method::GET, GOOD_URL), "https://evil.example").await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let json = body_json(res).await;
        assert_eq!(
            json.get("error").and_then(|v| v.as_str()),
            Some("forbidden_origin")
        );
        // No Origin header → native apps are allowed.
        let res2 = send(&app, req(Method::GET, GOOD_URL)).await;
        assert_eq!(res2.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn preflight() {
        let (app, _) = test_app();
        let res = send_origin(
            &app,
            req(Method::OPTIONS, "/v1/preview"),
            "https://app.example",
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .expect("acam"),
            "GET, HEAD, OPTIONS"
        );
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_MAX_AGE)
                .expect("acma"),
            "86400"
        );
        let res2 = send_origin(
            &app,
            req(Method::OPTIONS, "/v1/preview"),
            "https://evil.example",
        )
        .await;
        assert_eq!(res2.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn healthz_and_unknown_paths() {
        let (app, _) = test_app();
        let res = send(&app, req(Method::GET, "/healthz")).await;
        assert_eq!(res.status(), StatusCode::OK);
        let res2 = send(&app, req(Method::GET, "/nope")).await;
        assert_eq!(res2.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn method_not_allowed_has_allow() {
        let (app, _) = test_app();
        let res = send(
            &app,
            req(Method::POST, "/v1/preview?url=https%3A%2F%2Fa.com"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            res.headers().get(header::ALLOW).expect("allow"),
            "GET, HEAD, OPTIONS"
        );
    }

    #[tokio::test]
    async fn head_behaves_like_get() {
        let (app, fake) = test_app();
        fake.respond("https://a.com/", page("Hello"));
        let res = send(&app, req(Method::HEAD, GOOD_URL)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get(header::CONTENT_LENGTH).is_some());
    }

    #[tokio::test]
    async fn second_request_hits_cache() {
        let (app, fake) = test_app();
        fake.respond("https://a.com/", page("Hello"));
        let r1 = send(&app, req(Method::GET, GOOD_URL)).await;
        assert_eq!(r1.status(), StatusCode::OK);
        let r2 = send(&app, req(Method::GET, GOOD_URL)).await;
        assert_eq!(r2.status(), StatusCode::OK);
        assert_eq!(fake.calls(), 1, "second request must be served from cache");
    }

    #[tokio::test]
    async fn cacheable_error_cached_otherwise_not() {
        let (app, fake) = test_app();
        fake.respond("https://a.com/", page(""));
        // empty page → NoMetadata → cached
        for _ in 0..2 {
            let res = send(&app, req(Method::GET, GOOD_URL)).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND);
        }
        assert_eq!(fake.calls(), 1);
        // a non-cacheable error must not be cached: Fake with no routes
        // yields UpstreamUnreachable (502) every time.
        let (app2, fake2) = test_app();
        let res = send(&app2, req(Method::GET, GOOD_URL)).await;
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
        let res2 = send(&app2, req(Method::GET, GOOD_URL)).await;
        assert_eq!(res2.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(fake2.calls(), 2);
    }

    async fn burst_until_429(app: &Router) -> Response {
        let mut last = send(app, req(Method::GET, GOOD_URL)).await;
        for i in 0..64 {
            if last.status() == StatusCode::TOO_MANY_REQUESTS {
                break;
            }
            let uri = format!("/v1/preview?url=https%3A%2F%2Fh-{i}.example.com/");
            last = send(app, req(Method::GET, &uri)).await;
        }
        last
    }

    #[tokio::test]
    async fn ip_burst_exhaustion_yields_429() {
        let (app, fake) = test_app();
        fake.respond("https://a.com/", page("Hello"));
        let last = burst_until_429(&app).await;
        assert_eq!(last.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(last.headers().contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn unidentifiable_clients_share_one_bucket() {
        // With CfConnectingIp and no header, ClientIp extraction fails; the
        // request must still be rate-limited under a shared key.
        let fake = Fake::default();
        let limits = Limits {
            client_ip_source: ClientIpSource::CfConnectingIp,
            ..test_limits()
        };
        let app = router(Origins::Any, fake.clone(), &limits);
        fake.respond("https://a.com/", page("Hello"));
        let last = burst_until_429(&app).await;
        assert_eq!(last.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn host_limit_yields_429() {
        let (app, fake) = test_app();
        fake.respond("https://a.com/", page("Hello"));
        // 4/min host limit; hit = first request cached? No: the same URL is
        // cached after the first request, so use a fresh URL each time on the
        // same host — distinct query strings make distinct targets.
        let mut status = StatusCode::OK;
        for i in 0..8 {
            let uri = format!("/v1/preview?url=https%3A%2F%2Fa.com%2F{i}");
            fake.respond(&format!("https://a.com/{i}"), page("x"));
            status = send(&app, req(Method::GET, &uri)).await.status();
        }
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    struct Slow;

    impl Fetch for Slow {
        type Body = FakeBody;

        async fn get(&self, _url: &Url) -> Result<http::Response<Self::Body>, corx::FetchError> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn slow_upstream_times_out() {
        let app = router(Origins::Any, Slow, &test_limits());
        let res = send(&app, req(Method::GET, GOOD_URL)).await;
        assert_eq!(res.status(), StatusCode::GATEWAY_TIMEOUT);
        let json = body_json(res).await;
        assert_eq!(
            json.get("error").and_then(|v| v.as_str()),
            Some("upstream_timeout")
        );
    }
}
