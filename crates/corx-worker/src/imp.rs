#![allow(
    clippy::future_not_send,
    reason = "workers-rs futures are !Send by design on wasm32"
)]

//! Cloudflare Worker adapter for the `corx` link-preview core.
//!
//! The whole request pipeline lives in [`corx::service`]; this module only
//! wires workerd primitives (fetch, Cache API, rate-limit bindings, Delay)
//! into the [`corx::service::Runtime`] trait. Upstream egress is constrained
//! by `global_fetch_strictly_public` (wrangler.jsonc compatibility flag).

use core::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use corx::http::Origins;
use corx::service::{self, Admission, Bucket, Cached, Fresh, Incoming, Runtime};
use corx::{Fetch, FetchError};
use futures_util::{Stream, StreamExt};
use http::HeaderValue;
use url::Url;
use worker::{
    AbortController, AbortSignal, Cache, Context, Date, Delay, Env, Headers, Method, Request,
    RequestInit, RequestRedirect, Response, console_log, event,
};

/// `retry-after` reported by rate-limit bindings; their fixed period is 60 s.
const RETRY_AFTER_SECS: u64 = 60;

struct Cfg {
    origins: Origins,
    user_agent: String,
}

fn cfg(env: &Env) -> Result<Cfg, worker::Error> {
    let raw = env
        .var("ALLOWED_ORIGINS")
        .map(|v| v.to_string())
        .unwrap_or_default();
    let origins = Origins::parse(&raw)
        .map_err(|e| worker::Error::RustError(format!("ALLOWED_ORIGINS: {e}")))?;
    let user_agent = env
        .var("USER_AGENT")
        .map_or_else(|_| corx::DEFAULT_USER_AGENT.to_owned(), |v| v.to_string());
    Ok(Cfg {
        origins,
        user_agent,
    })
}

fn method_of(req: &Request) -> http::Method {
    // Unknown methods must not route: CONNECT is never GET/HEAD/OPTIONS, so
    // `route` answers MethodNotAllowed.
    http::Method::from_bytes(req.method().to_string().as_bytes()).unwrap_or(http::Method::CONNECT)
}

fn origin_of(req: &Request) -> Option<HeaderValue> {
    req.headers()
        .get("origin")
        .ok()
        .flatten()
        .and_then(|o| HeaderValue::from_str(&o).ok())
}

fn request_uri_and_origin(req: &Request) -> (http::Uri, String) {
    req.url().map_or_else(
        |_| (http::Uri::default(), String::new()),
        |u| {
            (
                u.as_str().parse().unwrap_or_default(),
                u.origin().ascii_serialization(),
            )
        },
    )
}

/// worker `Headers` → owned [`http::HeaderMap`]; malformed pairs are skipped.
fn header_map(headers: &Headers) -> http::HeaderMap {
    let mut map = http::HeaderMap::new();
    for (name, value) in headers.entries() {
        if let (Ok(n), Ok(v)) = (
            name.parse::<http::header::HeaderName>(),
            HeaderValue::from_str(&value),
        ) {
            map.append(n, v);
        }
    }
    map
}

/// Copies an [`http::HeaderMap`] onto a worker `Headers` object; header names
/// or values that cannot be represented fail the copy.
fn copy_headers(from: &http::HeaderMap, to: &Headers) -> worker::Result<()> {
    for (name, value) in from {
        if let Ok(v) = value.to_str() {
            to.append(name.as_str(), v)?;
        }
    }
    Ok(())
}

/// `u16` status → [`http::StatusCode`]; invalid codes become 502 so they are
/// never mistaken for success.
fn status(code: u16) -> http::StatusCode {
    http::StatusCode::from_u16(code).unwrap_or(http::StatusCode::BAD_GATEWAY)
}

/// Converts a rendered core response into a fresh, mutable worker response.
/// Empty bodies become a true null body so null-body statuses (204/304)
/// are built correctly.
fn to_worker(res: &http::Response<Bytes>) -> worker::Result<Response> {
    let out = if res.body().is_empty() {
        Response::empty()?
    } else {
        Response::from_bytes(res.body().to_vec())?
    }
    .with_status(res.status().as_u16());
    copy_headers(res.headers(), out.headers())?;
    Ok(out)
}

/// Rebuilds a cached entry as an owned http response: headers on `Cache::get`
/// results are immutable in workerd, so the core renders CORS onto a copy.
async fn cached_to_http(mut cached: Response) -> worker::Result<http::Response<Bytes>> {
    let map = header_map(cached.headers());
    let body = Bytes::from(cached.bytes().await?);
    let mut out = http::Response::new(body);
    *out.status_mut() = status(cached.status_code());
    *out.headers_mut() = map;
    Ok(out)
}

fn to_fetch_error(signal: &AbortSignal) -> FetchError {
    if signal.aborted() {
        FetchError::Timeout
    } else {
        FetchError::Unreachable
    }
}

struct WorkerFetch {
    user_agent: String,
    signal: AbortSignal,
}

impl Fetch for WorkerFetch {
    type Body = Pin<Box<dyn Stream<Item = Result<Bytes, FetchError>>>>;

    async fn get(&self, url: &Url) -> Result<http::Response<Self::Body>, FetchError> {
        let headers = Headers::new();
        let ok = headers.set("user-agent", &self.user_agent).is_ok()
            && headers.set("accept", corx::ACCEPT).is_ok();
        if !ok {
            return Err(FetchError::Unreachable);
        }
        let mut init = RequestInit::new();
        init.with_method(Method::Get)
            .with_redirect(RequestRedirect::Manual)
            .with_headers(headers);
        let req =
            Request::new_with_init(url.as_str(), &init).map_err(|_| FetchError::Unreachable)?;
        let mut res = worker::Fetch::Request(req)
            .send_with_signal(&self.signal)
            .await
            .map_err(|_| to_fetch_error(&self.signal))?;
        let code = res.status_code();
        let resp_headers = res.headers().clone();
        let stream = res.stream().map_err(|_| FetchError::Unreachable)?;
        let signal = self.signal.clone();
        let body: Self::Body = Box::pin(
            stream.map(move |item| item.map(Bytes::from).map_err(|_| to_fetch_error(&signal))),
        );
        let mut out = http::Response::new(body);
        *out.status_mut() = status(code);
        *out.headers_mut() = header_map(&resp_headers);
        Ok(out)
    }
}

/// workerd-side [`Runtime`]: rate-limit bindings, the default Cache and
/// `Delay` timers. Resolved once per request; binding/config errors surface
/// as 500 before the core pipeline runs.
struct WorkerRuntime {
    fetch: WorkerFetch,
    client: worker::RateLimiter,
    host: worker::RateLimiter,
    ctx: Context,
    cache_origin: String,
}

impl WorkerRuntime {
    fn cache_url(&self, target: &str) -> String {
        let encoded: String = url::form_urlencoded::byte_serialize(target.as_bytes()).collect();
        format!("{}/v1/preview?url={encoded}", self.cache_origin)
    }
}

impl Runtime for WorkerRuntime {
    type Fetch = WorkerFetch;

    fn fetcher(&self) -> &WorkerFetch {
        &self.fetch
    }

    fn now_unix_secs(&self) -> u64 {
        Date::now().as_millis() / 1000
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        Delay::from(duration)
    }

    /// A failing `limit` call stays fail-open; denial reports the binding's
    /// 60-second period.
    async fn admit(&self, bucket: Bucket, key: &str) -> Admission {
        let limiter = match bucket {
            Bucket::Client => &self.client,
            Bucket::Host => &self.host,
        };
        match limiter.limit(key.to_owned()).await {
            Ok(outcome) if outcome.success => Admission::Allowed,
            Ok(_) => Admission::Denied {
                retry_after_secs: RETRY_AFTER_SECS,
            },
            Err(_) => Admission::Allowed,
        }
    }

    async fn cached<M>(&self, key: &str, miss: M) -> Cached
    where
        M: Future<Output = Fresh>,
    {
        let url = self.cache_url(key);
        if let Ok(Some(cached)) = Cache::default().get(&url, true).await
            && let Ok(res) = cached_to_http(cached).await
        {
            return Cached::Hit(res);
        }
        let fresh = miss.await;
        if fresh.cacheable
            && let Ok(storable) = to_worker(&fresh.response)
        {
            let put_url = url.clone();
            self.ctx.wait_until(async move {
                let _stored = Cache::default().put(&put_url, storable).await;
            });
        }
        Cached::Miss(fresh)
    }
}

async fn handle(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let started_ms = Date::now().as_millis();
    let method_str = req.method().to_string();
    let path = req.path();
    let (uri, cache_origin) = request_uri_and_origin(&req);

    let cfg = cfg(&env)?;
    let client = env.rate_limiter("IP_LIMITER")?;
    let host = env.rate_limiter("HOST_LIMITER")?;

    let controller = AbortController::default();
    let rt = WorkerRuntime {
        fetch: WorkerFetch {
            user_agent: cfg.user_agent,
            signal: controller.signal(),
        },
        client,
        host,
        ctx,
        cache_origin,
    };

    let method = method_of(&req);
    let origin = origin_of(&req);
    let client_ip = req
        .headers()
        .get("cf-connecting-ip")
        .ok()
        .flatten()
        .and_then(|raw| raw.trim().parse::<std::net::IpAddr>().ok());
    let incoming = Incoming {
        method: &method,
        uri: &uri,
        origin: origin.as_ref(),
        client_ip,
    };
    let (res, record) = service::handle(&rt, &cfg.origins, incoming).await;
    controller.abort();

    let line = serde_json::json!({
        "method": method_str,
        "path": path,
        "outcome": record.outcome,
        "status": record.status.as_u16(),
        "latency_ms": Date::now().as_millis().saturating_sub(started_ms),
        "cache": record.cache.map_or("none", |c| c.as_str()),
        "target_host": record.target_host,
        "target_fingerprint": record.target_fingerprint,
    });
    console_log!("{line}");
    to_worker(&res)
}

/// Worker entrypoint.
#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    handle(req, env, ctx).await
}
