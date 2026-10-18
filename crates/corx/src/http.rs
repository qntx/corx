//! Request/response shaping shared by both adapters: routing, JSON
//! rendering, and origin (CORS) policy.

use bytes::Bytes;
use http::header::{
    ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE,
    CACHE_CONTROL, CONTENT_TYPE, VARY, X_CONTENT_TYPE_OPTIONS,
};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use url::Url;

use crate::{CACHE_CONTROL_SUCCESS, Error, Preview, Target};

/// `Access-Control-Allow-Methods` for preflights.
const ALLOW_METHODS: &str = "GET, HEAD, OPTIONS";

/// What a request routed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `GET|HEAD /v1/preview?url=…` with a validated target.
    Preview(Target),
    /// `OPTIONS /v1/preview`.
    Preflight,
}

/// Routes a request. Only `/v1/preview` exists.
///
/// # Errors
/// `NotFound` for other paths, `MethodNotAllowed` for other methods,
/// `InvalidUrl` when the `url` query parameter is missing or invalid.
pub fn route(method: &Method, uri: &Uri) -> Result<Route, Error> {
    if uri.path() != "/v1/preview" {
        return Err(Error::NotFound);
    }
    if method == Method::OPTIONS {
        return Ok(Route::Preflight);
    }
    if method != Method::GET && method != Method::HEAD {
        return Err(Error::MethodNotAllowed);
    }
    let raw = uri
        .query()
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "url")
                .map(|(_, value)| value.into_owned())
        })
        .ok_or(Error::InvalidUrl)?;
    Target::parse(&raw).map(Route::Preview)
}

/// Serializes a preview result into the JSON response contract.
///
/// `Preview` fields on success, `{"error": slug, "message": msg}` on error,
/// with `content-type`, `cache-control` and `x-content-type-options` set.
/// Never contains CORS headers — adapters add those after cache lookups.
#[must_use]
pub fn render(result: &Result<Preview, Error>) -> http::Response<Bytes> {
    let (status, body, cache_control) = match result {
        Ok(preview) => (
            StatusCode::OK,
            serde_json::to_vec(preview).unwrap_or_else(|_| b"{}".to_vec()),
            HeaderValue::from_static(CACHE_CONTROL_SUCCESS),
        ),
        Err(err) => (
            err.status(),
            serde_json::to_vec(&serde_json::json!({
                "error": err.slug(),
                "message": err.message(),
            }))
            .unwrap_or_else(|_| b"{}".to_vec()),
            HeaderValue::from_static(err.cache_control()),
        ),
    };
    let mut response = http::Response::new(Bytes::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if matches!(result, Err(Error::MethodNotAllowed)) {
        headers.insert(http::header::ALLOW, HeaderValue::from_static(ALLOW_METHODS));
    }
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(CACHE_CONTROL, cache_control);
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// Why an origin list failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OriginsError {
    /// An entry is not a valid `scheme://host[:port]` origin.
    #[error("invalid origin entry {0:?}")]
    Invalid(String),
}

/// Origin allowlist for CORS. `Any` reflects `*`; `List` holds normalized
/// `scheme://host[:port]` strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origins {
    /// Any origin is allowed (`*`).
    Any,
    /// Exact-match origin allowlist.
    List(std::collections::HashSet<String>),
}

impl Origins {
    /// Parses a comma-separated origin list; `*` anywhere means any origin.
    /// An empty or whitespace-only list is valid: no `Origin` is allowed.
    ///
    /// # Errors
    /// [`OriginsError`] when an entry is not a bare origin (paths, queries,
    /// credentials and fragments are rejected).
    pub fn parse(input: &str) -> Result<Self, OriginsError> {
        let entries: Vec<&str> = input
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .collect();
        if entries.contains(&"*") {
            return Ok(Self::Any);
        }
        entries
            .iter()
            .map(|entry| normalize_origin(entry))
            .collect::<Result<std::collections::HashSet<_>, _>>()
            .map(Self::List)
    }

    /// Whether `origin` may use the service. A missing `Origin` header
    /// (native apps, curl) is always allowed.
    ///
    /// # Errors
    /// `Error::ForbiddenOrigin` when a present `Origin` is not allowed.
    pub fn check(&self, origin: Option<&HeaderValue>) -> Result<(), Error> {
        let Some(origin) = origin else { return Ok(()) };
        match self {
            Self::Any => Ok(()),
            Self::List(list) => {
                let allowed = origin.to_str().is_ok_and(|value| list.contains(value));
                if allowed {
                    Ok(())
                } else {
                    Err(Error::ForbiddenOrigin)
                }
            }
        }
    }

    /// Stamps CORS headers onto a response. List mode always adds
    /// `Vary: Origin` and echoes the origin when allowed.
    pub fn apply(&self, origin: Option<&HeaderValue>, headers: &mut HeaderMap) {
        match self {
            Self::Any => {
                headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            }
            Self::List(list) => {
                headers.insert(VARY, HeaderValue::from_static("Origin"));
                if let Some(value) = origin
                    .and_then(|v| v.to_str().ok())
                    .filter(|o| list.contains(*o))
                    .and_then(|o| HeaderValue::from_str(o).ok())
                {
                    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, value);
                }
            }
        }
    }

    /// 204 preflight response: CORS headers plus `Allow-Methods` and
    /// `Max-Age`.
    #[must_use]
    pub fn preflight(&self, origin: Option<&HeaderValue>) -> http::Response<Bytes> {
        let mut response = http::Response::new(Bytes::new());
        *response.status_mut() = StatusCode::NO_CONTENT;
        self.apply(origin, response.headers_mut());
        let headers = response.headers_mut();
        headers.insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static(ALLOW_METHODS),
        );
        headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
        response
    }
}

fn normalize_origin(entry: &str) -> Result<String, OriginsError> {
    let url = Url::parse(entry).map_err(|_| OriginsError::Invalid((*entry).to_owned()))?;
    let bare = url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (url.path().is_empty() || url.path() == "/");
    if !bare {
        return Err(OriginsError::Invalid((*entry).to_owned()));
    }
    let Some(host) = url.host_str() else {
        return Err(OriginsError::Invalid((*entry).to_owned()));
    };
    let port = url.port().map_or(String::new(), |p| format!(":{p}"));
    Ok(format!("{}://{host}{port}", url.scheme()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, uri: &str) -> (Method, Uri) {
        (method.parse().expect("method"), uri.parse().expect("uri"))
    }

    fn route_str(method: &str, uri: &str) -> Result<Route, Error> {
        let (m, u) = req(method, uri);
        route(&m, &u)
    }

    #[test]
    fn route_table() {
        let cases = [
            ("GET", "/v1/preview?url=https%3A%2F%2Fexample.com", true),
            ("HEAD", "/v1/preview?url=https%3A%2F%2Fexample.com", true),
            ("GET", "/v1/preview?url=https://example.com", true),
            ("OPTIONS", "/v1/preview", true),
            ("GET", "/v1/preview", false),
            ("GET", "/v1/preview?x=1", false),
            ("GET", "/v1/preview?url=ftp://x", false),
            ("GET", "/v1/preview?url=http://127.0.0.1/", false),
            ("GET", "/other", false),
            ("GET", "/v1/preview/", false),
            ("POST", "/v1/preview?url=https://e.com", false),
            ("DELETE", "/v1/preview?url=https://e.com", false),
        ];
        for (method, uri, ok) in cases {
            assert_eq!(route_str(method, uri).is_ok(), ok, "{method} {uri}");
        }
        assert_eq!(
            route_str("OPTIONS", "/v1/preview?url=https://e.com"),
            Ok(Route::Preflight)
        );
        assert_eq!(
            route_str("POST", "/v1/preview"),
            Err(Error::MethodNotAllowed)
        );
        assert_eq!(route_str("GET", "/nope"), Err(Error::NotFound));
        // first `url` param wins
        assert_eq!(
            route_str("GET", "/v1/preview?url=https://a.com&url=https://b.com"),
            Ok(Route::Preview(Target::parse("https://a.com").expect("t")))
        );
    }

    #[test]
    fn render_success() {
        let p = Preview {
            url: "https://a.com".into(),
            final_url: "https://a.com/".into(),
            title: Some("T".into()),
            description: None,
            site_name: None,
            image: None,
            favicon: None,
            card: None,
            kind: None,
            theme_color: None,
            lang: None,
            fetched_at: 7,
        };
        let res = render(&Ok(p));
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(CACHE_CONTROL).unwrap(),
            "public, max-age=86400, stale-while-revalidate=604800"
        );
        assert_eq!(
            res.headers().get(X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        let body = serde_json::from_slice::<serde_json::Value>(res.body()).unwrap();
        assert_eq!(
            body.get("title").and_then(serde_json::Value::as_str),
            Some("T")
        );
        assert_eq!(
            body.get("finalUrl").and_then(serde_json::Value::as_str),
            Some("https://a.com/")
        );
        assert_eq!(
            body.get("fetchedAt").and_then(serde_json::Value::as_u64),
            Some(7)
        );
        assert!(body.get("description").is_none());
        assert!(res.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[test]
    fn render_errors() {
        let res = render(&Err(Error::BlockedTarget));
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(res.headers().get(CACHE_CONTROL).unwrap(), "no-store");
        let body = serde_json::from_slice::<serde_json::Value>(res.body()).unwrap();
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some("blocked_target")
        );
        assert!(
            body.get("message")
                .is_some_and(serde_json::Value::is_string)
        );
        let res2 = render(&Err(Error::NoMetadata));
        assert_eq!(
            res2.headers().get(CACHE_CONTROL).unwrap(),
            "public, max-age=3600"
        );
    }

    #[test]
    fn origins() {
        assert_eq!(Origins::parse("*"), Ok(Origins::Any));
        assert_eq!(
            Origins::parse(" https://A.com, http://b.com:8080 ,HTTPS://c.com/"),
            Ok(Origins::List(std::collections::HashSet::from([
                "https://a.com".into(),
                "http://b.com:8080".into(),
                "https://c.com".into()
            ])))
        );
        assert_eq!(
            Origins::parse(""),
            Ok(Origins::List(std::collections::HashSet::new()))
        );
        assert_eq!(
            Origins::parse("  ,  "),
            Ok(Origins::List(std::collections::HashSet::new()))
        );
        for bad in [
            "http://a.com/path",
            "http://a.com/?q=1",
            "http://u@a.com",
            "notaurl",
        ] {
            assert!(Origins::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn origins_behaviour() {
        let list = Origins::parse("https://a.com").unwrap();
        let good = HeaderValue::from_static("https://a.com");
        let bad = HeaderValue::from_static("https://evil.com");
        assert!(list.check(None).is_ok());
        assert_eq!(list.check(Some(&good)), Ok(()));
        assert_eq!(list.check(Some(&bad)), Err(Error::ForbiddenOrigin));
        assert_eq!(Origins::Any.check(Some(&bad)), Ok(()));

        let mut headers = HeaderMap::new();
        list.apply(Some(&good), &mut headers);
        assert_eq!(
            headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://a.com"
        );
        assert_eq!(headers.get(VARY).unwrap(), "Origin");
        let mut headers2 = HeaderMap::new();
        list.apply(Some(&bad), &mut headers2);
        assert!(headers2.get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        assert_eq!(headers2.get(VARY).unwrap(), "Origin");
        let mut headers3 = HeaderMap::new();
        Origins::Any.apply(Some(&bad), &mut headers3);
        assert_eq!(headers3.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*");

        let res = list.preflight(Some(&good));
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            res.headers().get(ACCESS_CONTROL_ALLOW_METHODS).unwrap(),
            ALLOW_METHODS
        );
        assert_eq!(res.headers().get(ACCESS_CONTROL_MAX_AGE).unwrap(), "86400");
        assert_eq!(
            res.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://a.com"
        );
        let res2 = list.preflight(Some(&bad));
        assert!(res2.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }
}
