//! Axum adapter: adapts requests into [`corx::service::Incoming`], runs the
//! shared core pipeline, and emits per-request logs/metrics from the record.

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{Request, State};
use axum::http::header::ORIGIN;
use axum::response::Response;
use axum::routing::get;
use axum_client_ip::{ClientIp, ClientIpSource, Rejection};
use corx::Fetch;
use corx::http::Origins;
use corx::service::{self, Incoming};
use tower::ServiceBuilder;

use crate::config::Config;
use crate::runtime::NativeRuntime;

/// Per-IP and per-host admission quotas plus request budget.
#[derive(Debug, Clone)]
pub(crate) struct Limits {
    /// Maximum concurrently served requests.
    pub(crate) max_concurrency: usize,
    /// Preview cache capacity in entries.
    pub(crate) cache_capacity: u64,
    /// Requests per minute allowed for each client IP.
    pub(crate) ip_rate_per_minute: u32,
    /// Burst allowance on top of `ip_rate_per_minute`.
    pub(crate) ip_burst: u32,
    /// Preview requests per minute allowed for each target host.
    pub(crate) host_rate_per_minute: u32,
    /// Request header used to identify the client IP.
    pub(crate) client_ip_source: ClientIpSource,
}

impl From<&Config> for Limits {
    fn from(cfg: &Config) -> Self {
        Self {
            max_concurrency: cfg.max_concurrency,
            cache_capacity: cfg.cache_capacity,
            ip_rate_per_minute: cfg.ip_rate_per_minute,
            ip_burst: cfg.ip_burst,
            host_rate_per_minute: cfg.host_rate_per_minute,
            client_ip_source: cfg.client_ip_source.clone(),
        }
    }
}

async fn healthz() -> &'static str {
    "ok"
}

/// Builds the router. `/healthz` stays outside the load shed so orchestrator
/// probes still succeed under saturation; everything else is limited.
pub(crate) fn router<F>(origins: Origins, fetch: F, limits: &Limits) -> Router
where
    F: Fetch + Send + Sync + 'static,
    F::Body: Send + 'static,
{
    let state = Arc::new(AppState {
        runtime: NativeRuntime::new(fetch, limits),
        origins,
    });
    let inner = Router::new()
        .fallback(preview::<F>)
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_overload))
                .load_shed()
                .concurrency_limit(limits.max_concurrency),
        );
    Router::new()
        .route("/healthz", get(healthz))
        .fallback_service(inner)
        .layer(limits.client_ip_source.clone().into_extension())
}

async fn handle_overload(_: axum::BoxError) -> Response {
    corx::http::render(&Err(corx::Error::Overloaded)).map(Body::from)
}

/// Request state: the platform runtime plus request-policy origins.
#[derive(Debug)]
struct AppState<F> {
    runtime: NativeRuntime<F>,
    origins: Origins,
}

async fn preview<F: Fetch>(
    State(state): State<Arc<AppState<F>>>,
    client_ip: Result<ClientIp, Rejection>,
    req: Request,
) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let incoming = Incoming {
        method: req.method(),
        uri: req.uri(),
        origin: req.headers().get(ORIGIN),
        client_ip: client_ip.map(|ClientIp(ip): ClientIp| ip).ok(),
    };
    let (res, record) = service::handle(&state.runtime, &state.origins, incoming).await;
    let cache = record.cache.map_or("none", |c| c.as_str());
    let elapsed = started.elapsed();
    metrics::counter!("corx_requests_total", "outcome" => record.outcome).increment(1);
    metrics::histogram!("corx_request_duration_seconds", "cache" => cache)
        .record(elapsed.as_secs_f64());
    tracing::info!(
        method = %method,
        path = path.as_str(),
        outcome = record.outcome,
        status = record.status.as_u16(),
        latency_ms = elapsed.as_millis(),
        cache = cache,
        target_host = record.target_host.as_deref().unwrap_or("n/a"),
        target_fingerprint = record.target_fingerprint.as_deref().unwrap_or("n/a"),
        "request"
    );
    res.map(Body::from)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::extract::connect_info::ConnectInfo;
    use axum::http::{HeaderValue, Method, Request as HttpRequest, StatusCode, header};
    use bytes::Bytes;
    use corx::Fetch;
    use futures_util::stream::{self, Stream};
    use tower::ServiceExt;
    use url::Url;

    use super::*;

    type FakeBody = Pin<Box<dyn Stream<Item = Result<Bytes, corx::FetchError>> + Send>>;

    /// Counting fetcher with an optional delay/hang to hold concurrency slots.
    #[derive(Debug)]
    struct Shared {
        calls: AtomicUsize,
        delay: Duration,
        hang: bool,
    }

    #[derive(Debug, Clone)]
    struct Fake(Arc<Shared>);

    impl Fake {
        fn new(delay: Duration, hang: bool) -> Self {
            Self(Arc::new(Shared {
                calls: AtomicUsize::new(0),
                delay,
                hang,
            }))
        }

        fn calls(&self) -> usize {
            self.0.calls.load(Ordering::Relaxed)
        }
    }

    impl Fetch for Fake {
        type Body = FakeBody;

        async fn get(&self, _url: &Url) -> Result<http::Response<Self::Body>, corx::FetchError> {
            self.0.calls.fetch_add(1, Ordering::Relaxed);
            if self.0.hang {
                std::future::pending::<()>().await;
            }
            if !self.0.delay.is_zero() {
                tokio::time::sleep(self.0.delay).await;
            }
            let body: FakeBody = Box::pin(stream::iter([Ok(Bytes::from(
                "<html><head><title>x</title></head><body/></html>",
            ))]));
            let mut res = http::Response::new(body);
            *res.status_mut() = StatusCode::OK;
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
            Ok(res)
        }
    }

    const CLIENT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 115, 9)), 9);
    const GOOD_URL: &str = "/v1/preview?url=https%3A%2F%2Fa.com%2F";

    fn test_limits() -> Limits {
        Limits {
            max_concurrency: 64,
            cache_capacity: 1024,
            ip_rate_per_minute: 60,
            ip_burst: 8,
            host_rate_per_minute: 4,
            client_ip_source: ClientIpSource::ConnectInfo,
        }
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

    async fn send(app: Router, r: HttpRequest<Body>) -> Response {
        app.oneshot(r).await.expect("response")
    }

    #[tokio::test]
    async fn concurrent_identical_misses_coalesce() {
        let fake = Fake::new(Duration::from_millis(100), false);
        let app = router(Origins::Any, fake.clone(), &test_limits());
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let app = app.clone();
            tasks.push(tokio::spawn(send(app, req(Method::GET, GOOD_URL))));
        }
        for task in tasks {
            assert_eq!(task.await.expect("join").status(), StatusCode::OK);
        }
        assert_eq!(fake.calls(), 1, "moka must coalesce the shared miss");
    }

    #[tokio::test]
    async fn unidentifiable_clients_share_one_bucket() {
        let mut limits = test_limits();
        limits.client_ip_source = ClientIpSource::CfConnectingIp;
        let app = router(Origins::Any, Fake::new(Duration::ZERO, false), &limits);
        let mut last = StatusCode::OK;
        for _ in 0..limits.ip_burst + 2 {
            // No ConnectInfo extension, no CF-Connecting-IP header.
            let r = HttpRequest::builder()
                .method(Method::GET)
                .uri(GOOD_URL)
                .body(Body::empty())
                .expect("req");
            last = send(app.clone(), r).await.status();
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn rate_limited_carries_retry_after() {
        let mut limits = test_limits();
        limits.ip_burst = 1;
        limits.ip_rate_per_minute = 1;
        let app = router(Origins::Any, Fake::new(Duration::ZERO, false), &limits);
        for _ in 0..2 {
            send(app.clone(), req(Method::GET, GOOD_URL)).await;
        }
        let res = send(app, req(Method::GET, GOOD_URL)).await;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(res.headers().contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn healthz_served_while_concurrency_saturated() {
        let mut limits = test_limits();
        limits.max_concurrency = 1;
        let app = router(Origins::Any, Fake::new(Duration::ZERO, true), &limits);
        let pending = tokio::spawn(send(app.clone(), req(Method::GET, GOOD_URL)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let res = send(app.clone(), req(Method::GET, "/healthz")).await;
        assert_eq!(res.status(), StatusCode::OK);
        pending.abort();
    }

    #[tokio::test]
    async fn overload_yields_503() {
        let mut limits = test_limits();
        limits.max_concurrency = 1;
        let app = router(Origins::Any, Fake::new(Duration::ZERO, true), &limits);
        let pending = tokio::spawn(send(app.clone(), req(Method::GET, GOOD_URL)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let res = send(app.clone(), req(Method::GET, GOOD_URL)).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some("overloaded")
        );
        pending.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_upstream_times_out() {
        let app = router(
            Origins::Any,
            Fake::new(Duration::ZERO, true),
            &test_limits(),
        );
        let pending = tokio::spawn(send(app, req(Method::GET, GOOD_URL)));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        let res = pending.await.expect("join");
        assert_eq!(res.status(), StatusCode::GATEWAY_TIMEOUT);
    }
}
