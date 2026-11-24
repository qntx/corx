//! Black-box conformance: the identical assertions run against the native
//! (`corx` binary) and Workers (`wrangler dev --local`) editions. Every test
//! needs `CORX_URL` pointing at a running service configured with the
//! `https://app.example` origin allowlist; `run.sh` provides it.
//!
//! Header assertions are exact: beyond the transport headers a proxy may
//! add freely, every response must carry precisely the expected set — each
//! name once, with precisely the expected value. No case needs the public
//! web; the only targets ever fetched upstream are `example.invalid`
//! requests whose status is never asserted.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::shadow_unrelated,
    reason = "conformance assertions may panic on black-box replies"
)]

use std::sync::atomic::{AtomicU32, Ordering};

use reqwest::header::HeaderValue;
use serde_json::json;

/// Headers a transport (workerd, hyper, a proxy) may add or fold on its
/// own; everything else must match the expected set exactly.
const TRANSPORT_HEADERS: [&str; 5] = [
    "content-length",
    "date",
    "connection",
    "keep-alive",
    "transfer-encoding",
];

const ALLOWED_ORIGIN: &str = "https://app.example";
const DISALLOWED_ORIGIN: &str = "https://evil.example";
/// A valid target that never resolves to real content; requests sent to it
/// pass admission but their status is never asserted.
const DEAD_TARGET: &str = "https%3A%2F%2Fexample.invalid%2F";

const M_INVALID_URL: &str = "the url parameter is missing, malformed, or unsupported";
const M_FORBIDDEN_ORIGIN: &str = "the request origin is not allowed";
const M_BLOCKED_TARGET: &str = "the target host is not allowed";
const M_NOT_FOUND: &str = "not found";
const M_METHOD_NOT_ALLOWED: &str = "method not allowed";
const M_RATE_LIMITED: &str = "rate limit exceeded";

fn base() -> String {
    std::env::var("CORX_URL").expect("CORX_URL must point at the service under test")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

/// Each test runs under its own client-IP bucket so only the rate-limit
/// case can drain its allowance.
static CLIENT_IP: AtomicU32 = AtomicU32::new(1);

fn unique_ip() -> String {
    format!("203.0.113.{}", CLIENT_IP.fetch_add(1, Ordering::Relaxed))
}

async fn send(
    method: reqwest::Method,
    path: &str,
    origin: Option<&str>,
    client_ip: &str,
) -> reqwest::Response {
    let mut req = client()
        .request(method, format!("{}{path}", base()))
        .header("cf-connecting-ip", client_ip);
    if let Some(origin) = origin {
        req = req.header("origin", origin);
    }
    req.send().await.expect("request failed")
}

/// Every non-transport header name must be expected, every expected header
/// must be present exactly once with exactly the expected value — a folded
/// `application/octet-stream, application/json; charset=utf-8` fails.
fn expect_headers(res: &reqwest::Response, expected: &[(&str, &str)]) {
    let headers = res.headers();
    for name in headers.keys() {
        let n = name.as_str();
        assert!(
            TRANSPORT_HEADERS.contains(&n) || expected.iter().any(|(e, _)| *e == n),
            "unexpected header {n} in {headers:?}"
        );
    }
    for (name, value) in expected {
        let all: Vec<_> = headers.get_all(*name).iter().collect();
        assert_eq!(
            all.len(),
            1,
            "header {name} must appear exactly once in {headers:?}"
        );
        assert_eq!(
            *all[0],
            HeaderValue::from_str(value).unwrap(),
            "header {name} value in {headers:?}"
        );
    }
}

/// The headers every JSON error response carries under the `https://app.example`
/// allowlist (List mode stamps `vary: Origin` on everything), plus `extra`.
fn error_headers<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![
        ("content-type", "application/json; charset=utf-8"),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("vary", "Origin"),
    ];
    headers.extend_from_slice(extra);
    headers
}

async fn expect_error(
    res: reqwest::Response,
    status: u16,
    slug: &str,
    message: &str,
    extra_headers: &[(&str, &str)],
) {
    assert_eq!(res.status().as_u16(), status, "status for {slug}");
    expect_headers(&res, &error_headers(extra_headers));
    let bytes = res.bytes().await.expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(
        body,
        json!({"error": slug, "message": message}),
        "body for {slug}"
    );
}

async fn get(path: &str, origin: Option<&str>) -> reqwest::Response {
    send(reqwest::Method::GET, path, origin, &unique_ip()).await
}

#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn routing_not_found() {
    for path in ["/", "/nope", "/v1/preview/"] {
        let res = get(path, None).await;
        expect_error(res, 404, "not_found", M_NOT_FOUND, &[]).await;
    }
}

#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn method_not_allowed() {
    let path = "/v1/preview?url=https%3A%2F%2Fexample.com%2F";
    for method in [
        reqwest::Method::POST,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
        reqwest::Method::PATCH,
    ] {
        let res = send(method.clone(), path, None, &unique_ip()).await;
        expect_error(
            res,
            405,
            "method_not_allowed",
            M_METHOD_NOT_ALLOWED,
            &[("allow", "GET, HEAD, OPTIONS")],
        )
        .await;
    }
}

#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn invalid_url() {
    // A 2049-byte target URL exceeds MAX_URL_LEN (2048).
    let long = format!("/v1/preview?url=https%3A%2F%2F{}%2F", "a".repeat(2040));
    for path in [
        "/v1/preview",
        "/v1/preview?url=ftp%3A%2F%2Fexample.com%2F",
        "/v1/preview?url=not%20a%20url",
        long.as_str(),
    ] {
        let res = get(path, None).await;
        expect_error(res, 400, "invalid_url", M_INVALID_URL, &[]).await;
    }
}

#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn blocked_target() {
    for path in [
        "/v1/preview?url=http%3A%2F%2F127.0.0.1%2F",
        "/v1/preview?url=http%3A%2F%2F%5B%3A%3A1%5D%2F",
        "/v1/preview?url=https%3A%2F%2Flocalhost%2F",
        "/v1/preview?url=http%3A%2F%2Fprinter.local%2F",
    ] {
        let res = get(path, None).await;
        expect_error(res, 403, "blocked_target", M_BLOCKED_TARGET, &[]).await;
    }
}

#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn cors() {
    // Allowed origin on an error response: ACAO echoes the allowlist entry.
    let res = get("/nope", Some(ALLOWED_ORIGIN)).await;
    expect_error(
        res,
        404,
        "not_found",
        M_NOT_FOUND,
        &[("access-control-allow-origin", ALLOWED_ORIGIN)],
    )
    .await;

    // Disallowed origin on a routed request: 403, Vary present, no ACAO.
    let res = get(
        "/v1/preview?url=https%3A%2F%2Fexample.com%2F",
        Some(DISALLOWED_ORIGIN),
    )
    .await;
    expect_error(res, 403, "forbidden_origin", M_FORBIDDEN_ORIGIN, &[]).await;

    // No Origin: never an ACAO, but List mode still sends Vary.
    let res = get("/nope", None).await;
    expect_error(res, 404, "not_found", M_NOT_FOUND, &[]).await;
}

#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn preflight() {
    let path = "/v1/preview";

    let res = send(
        reqwest::Method::OPTIONS,
        path,
        Some(ALLOWED_ORIGIN),
        &unique_ip(),
    )
    .await;
    assert_eq!(res.status().as_u16(), 204);
    expect_headers(
        &res,
        &[
            ("vary", "Origin"),
            ("access-control-allow-origin", ALLOWED_ORIGIN),
            ("access-control-allow-methods", "GET, HEAD, OPTIONS"),
            ("access-control-max-age", "86400"),
        ],
    );
    assert_eq!(res.text().await.expect("body"), "", "preflight body");

    let res = send(
        reqwest::Method::OPTIONS,
        path,
        Some(DISALLOWED_ORIGIN),
        &unique_ip(),
    )
    .await;
    expect_error(res, 403, "forbidden_origin", M_FORBIDDEN_ORIGIN, &[]).await;

    let res = send(reqwest::Method::OPTIONS, path, None, &unique_ip()).await;
    assert_eq!(res.status().as_u16(), 204);
    expect_headers(
        &res,
        &[
            ("vary", "Origin"),
            ("access-control-allow-methods", "GET, HEAD, OPTIONS"),
            ("access-control-max-age", "86400"),
        ],
    );
}

#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn head_matches_get() {
    for (path, status, slug) in [
        ("/v1/preview?url=ftp%3A%2F%2Fx", 400, "invalid_url"),
        ("/nope", 404, "not_found"),
    ] {
        let res = send(reqwest::Method::HEAD, path, None, &unique_ip()).await;
        assert_eq!(res.status().as_u16(), status, "HEAD status for {slug}");
        expect_headers(&res, &error_headers(&[]));
        let body = res.text().await.expect("body");
        assert_eq!(body, "", "HEAD must not carry a body");
    }
}

/// The service runs with a 5-request client-IP bucket; this test owns a
/// unique `cf-connecting-ip`, so only its own requests can drain it. The
/// requests that pass admission fetch `example.invalid` — their status is
/// deliberately not asserted (no real upstream content is needed).
#[tokio::test]
#[ignore = "black-box: run via crates/corx-conformance/run.sh"]
async fn rate_limited() {
    let ip = unique_ip();
    let path = format!("/v1/preview?url={DEAD_TARGET}");
    for _ in 0..5 {
        let res = send(reqwest::Method::GET, &path, Some(ALLOWED_ORIGIN), &ip).await;
        assert_ne!(
            res.status().as_u16(),
            429,
            "request inside the bucket must not be limited"
        );
    }
    let res = send(reqwest::Method::GET, &path, Some(ALLOWED_ORIGIN), &ip).await;
    assert_eq!(res.status().as_u16(), 429);
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("retry-after must be an integer");
    assert!(retry_after >= 1, "retry-after must be >= 1");
    let retry_value = retry_after.to_string();
    expect_error(
        res,
        429,
        "rate_limited",
        M_RATE_LIMITED,
        &[
            ("retry-after", retry_value.as_str()),
            ("access-control-allow-origin", ALLOWED_ORIGIN),
        ],
    )
    .await;
}
