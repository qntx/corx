#![allow(
    clippy::future_not_send,
    reason = "workers-rs futures are !Send by design on wasm32"
)]

//! Cloudflare Worker adapter for the `corx` link-preview core.
//!
//! Upstream egress is constrained by `global_fetch_strictly_public`
//! (wrangler.jsonc compatibility flag): fetch may only reach public
//! destinations. Per-hop URL validation happens in the pure core.

use core::pin::Pin;

use bytes::Bytes;
use corx::http::{Origins, Route, render, route};
use corx::{Error, Fetch, FetchError, TOTAL_TIMEOUT, Target, preview};
use futures_util::future::Either;
use futures_util::{Stream, StreamExt};
use http::HeaderValue;
use url::Url;
use worker::{
    AbortController, AbortSignal, Cache, Context, Date, Delay, Env, Headers, Method, Request,
    RequestInit, RequestRedirect, Response, console_log, event,
};

const CACHE_PREFIX: &str = "https://corx.cache/v1/preview?url=";

/// Bucket for requests whose `CF-Connecting-IP` cannot be parsed: they share
/// one rate-limit key rather than bypassing the limit.
const UNKNOWN_IP_KEY: &str = "::";

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
    let user_agent = env.var("USER_AGENT").map_or_else(
        |_| concat!("corx/", env!("CARGO_PKG_VERSION")).to_owned(),
        |v| v.to_string(),
    );
    Ok(Cfg {
        origins,
        user_agent,
    })
}

struct RequestLog {
    outcome: &'static str,
    status: u16,
    cache: &'static str,
    host: String,
    fingerprint: String,
}

fn emit_log(method: &str, path: &str, started_ms: u64, log: &RequestLog) {
    let line = serde_json::json!({
        "method": method,
        "path": path,
        "outcome": log.outcome,
        "status": log.status,
        "latency_ms": Date::now().as_millis().saturating_sub(started_ms),
        "cache": log.cache,
        "target_host": log.host,
        "target_fingerprint": log.fingerprint,
    });
    console_log!("{line}");
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

fn to_worker(res: &http::Response<Bytes>) -> worker::Result<Response> {
    let mut out = Response::from_bytes(res.body().to_vec())?.with_status(res.status().as_u16());
    for (name, value) in res.headers() {
        if let Ok(v) = value.to_str() {
            out.headers_mut().set(name.as_str(), v)?;
        }
    }
    Ok(out)
}

/// Rebuilds a cached response: headers on `Cache::get` results are immutable
/// in workerd, so CORS cannot be stamped onto the cached object itself.
async fn freshen(mut cached: Response) -> worker::Result<Response> {
    let entries: Vec<(String, String)> = cached.headers().entries().collect();
    let body = cached.bytes().await?;
    let mut out = Response::from_bytes(body)?.with_status(cached.status_code());
    for (name, value) in entries {
        out.headers_mut().set(&name, &value)?;
    }
    Ok(out)
}

/// Applies CORS headers computed by the core to a worker-side response.
fn apply_cors(
    origins: &Origins,
    origin: Option<&HeaderValue>,
    res: &mut Response,
) -> worker::Result<()> {
    let mut map = http::HeaderMap::new();
    origins.apply(origin, &mut map);
    for (name, value) in &map {
        if let Ok(v) = value.to_str() {
            res.headers_mut().set(name.as_str(), v)?;
        }
    }
    Ok(())
}

fn render_err(
    err: Error,
    origins: &Origins,
    origin: Option<&HeaderValue>,
) -> worker::Result<Response> {
    let res = render(&Err(err));
    let mut out = to_worker(&res)?;
    apply_cors(origins, origin, &mut out)?;
    Ok(out)
}

fn to_fetch_error(signal: &AbortSignal) -> FetchError {
    if signal.aborted() {
        FetchError::Timeout
    } else {
        FetchError::Unreachable
    }
}

fn client_key(req: &Request) -> Option<String> {
    req.headers()
        .get("cf-connecting-ip")
        .ok()
        .flatten()
        .and_then(|raw| raw.trim().parse::<std::net::IpAddr>().ok())
        .map(|ip| corx::ip::rate_key(ip).to_string())
}

struct WorkerFetch<'a> {
    user_agent: &'a str,
    signal: AbortSignal,
}

impl Fetch for WorkerFetch<'_> {
    type Body = Pin<Box<dyn Stream<Item = Result<Bytes, FetchError>>>>;

    async fn get(&self, url: &Url) -> Result<http::Response<Self::Body>, FetchError> {
        let headers = Headers::new();
        let ok = headers.set("user-agent", self.user_agent).is_ok()
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
        let status = res.status_code();
        let resp_headers = res.headers().clone();
        let stream = res.stream().map_err(|_| FetchError::Unreachable)?;
        let signal = self.signal.clone();
        let body: Self::Body = Box::pin(
            stream.map(move |item| item.map(Bytes::from).map_err(|_| to_fetch_error(&signal))),
        );
        let mut out = http::Response::new(body);
        *out.status_mut() = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK);
        for (name, value) in resp_headers.entries() {
            if let Ok(v) = HeaderValue::from_str(&value)
                && let Ok(n) = name.parse::<http::header::HeaderName>()
            {
                out.headers_mut().append(n, v);
            }
        }
        Ok(out)
    }
}

fn cache_key(target: &Target) -> String {
    let encoded: String =
        url::form_urlencoded::byte_serialize(target.as_str().as_bytes()).collect();
    format!("{CACHE_PREFIX}{encoded}")
}

/// A missing binding is a misconfiguration (propagated); a failing `limit`
/// call stays fail-open.
async fn maybe_limit(env: &Env, binding: &str, key: String) -> Result<bool, worker::Error> {
    let limiter = env.rate_limiter(binding)?;
    Ok(limiter.limit(key).await.map_or(true, |o| o.success))
}

async fn miss(
    cfg: &Cfg,
    env: &Env,
    target: Target,
    signal: AbortSignal,
) -> worker::Result<Result<corx::Preview, Error>> {
    if !maybe_limit(env, "HOST_LIMITER", target.host().to_owned()).await? {
        return Ok(Err(Error::RateLimited));
    }
    let fetch = WorkerFetch {
        user_agent: &cfg.user_agent,
        signal,
    };
    let now = Date::now().as_millis() / 1000;
    let work = preview(&fetch, target, now);
    let delay = Delay::from(TOTAL_TIMEOUT);
    let mut work = core::pin::pin!(work);
    let mut delay = core::pin::pin!(delay);
    let out = match futures_util::future::select(work.as_mut(), delay.as_mut()).await {
        Either::Left((out, _)) => out,
        Either::Right(_) => Err(Error::UpstreamTimeout),
    };
    Ok(out)
}

async fn handle(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let started_ms = Date::now().as_millis();
    let method_str = req.method().to_string();
    let path = req.path().clone();
    let origin = origin_of(&req);
    let method = method_of(&req);
    let uri: http::Uri = req
        .url()
        .ok()
        .and_then(|u| u.as_str().parse().ok())
        .unwrap_or_else(|| http::Uri::from_static("/"));

    let cfg = cfg(&env)?;

    let mut log = RequestLog {
        outcome: "ok",
        status: 200,
        cache: "n/a",
        host: "n/a".to_owned(),
        fingerprint: "n/a".to_owned(),
    };

    let res = match route(&method, &uri) {
        Err(e) => {
            log.outcome = e.slug();
            log.status = e.status().as_u16();
            render_err(e, &cfg.origins, origin.as_ref())?
        }
        Ok(routed) => match cfg.origins.check(origin.as_ref()) {
            Err(e) => {
                log.outcome = e.slug();
                log.status = e.status().as_u16();
                render_err(e, &cfg.origins, origin.as_ref())?
            }
            Ok(()) => match routed {
                Route::Preflight => {
                    log.outcome = "preflight";
                    log.status = 204;
                    to_worker(&cfg.origins.preflight(origin.as_ref()))?
                }
                Route::Preview(target) => {
                    log.host = target.host().to_owned();
                    log.fingerprint = target.fingerprint();
                    preview_path(&req, &env, &ctx, &cfg, target, origin.as_ref(), &mut log).await?
                }
            },
        },
    };
    emit_log(&method_str, &path, started_ms, &log);
    Ok(res)
}

async fn preview_path(
    req: &Request,
    env: &Env,
    ctx: &Context,
    cfg: &Cfg,
    target: Target,
    origin: Option<&HeaderValue>,
    log: &mut RequestLog,
) -> worker::Result<Response> {
    let ip_key = client_key(req).unwrap_or_else(|| UNKNOWN_IP_KEY.to_owned());
    if !maybe_limit(env, "IP_LIMITER", ip_key).await? {
        let e = Error::RateLimited;
        log.outcome = e.slug();
        log.status = e.status().as_u16();
        return render_err(e, &cfg.origins, origin);
    }

    let key = cache_key(&target);
    let cache = Cache::default();
    if let Ok(Some(cached)) = cache.get(&key, true).await {
        log.cache = "hit";
        log.status = cached.status_code();
        log.outcome = if cached.status_code() == 200 {
            "ok"
        } else {
            "cached_error"
        };
        let mut res = freshen(cached).await?;
        apply_cors(&cfg.origins, origin, &mut res)?;
        return Ok(res);
    }
    log.cache = "miss";

    let controller = AbortController::default();
    let rendered = miss(cfg, env, target, controller.signal()).await?;
    controller.abort();
    match &rendered {
        Ok(_) => {}
        Err(e) => {
            log.outcome = e.slug();
            log.status = e.status().as_u16();
        }
    }
    let res = render(&rendered);
    if rendered
        .as_ref()
        .map_or_else(|e| e.is_cacheable(), |_| true)
        && let Ok(storable) = to_worker(&res)
    {
        let key2 = key.clone();
        ctx.wait_until(async move {
            let _stored = cache.put(&key2, storable).await;
        });
    }
    let mut out = to_worker(&res)?;
    apply_cors(&cfg.origins, origin, &mut out)?;
    Ok(out)
}

/// Worker entrypoint.
#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    handle(req, env, ctx).await
}
