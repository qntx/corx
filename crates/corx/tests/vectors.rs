//! Golden-vector runner: every JSON file under `contract/vectors` describes
//! a request, a scripted upstream, and the expected response, each executed
//! through `corx::service::handle` on a fake runtime. A vector is the
//! contract: the runner reports every failing file at once.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "a malformed vector or a poisoned lock aborts the run"
)]

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use corx::http::Origins;
use corx::service::{self, Admission, Bucket, CacheStatus, Cached, Fresh, Incoming, Runtime};
use corx::{Fetch, FetchError};
use futures_util::stream::{self, Stream};
use http::{HeaderName, HeaderValue, Method, StatusCode, Uri};
use serde_json::Value;
use url::Url;

/// Fixed clock for every vector.
const NOW: u64 = 1_700_000_000;
/// `retry_after_secs` a denied admission reports.
const DENY_RETRY_SECS: u64 = 7;

type Body = Pin<Box<dyn Stream<Item = Result<Bytes, FetchError>> + Send>>;

/// One scripted upstream hop: a canned response or a transport failure.
enum Hop {
    Respond(http::Response<Vec<Bytes>>),
    Fail(FetchError),
}

struct ScriptedFetch {
    hops: Mutex<VecDeque<(String, Option<String>, Hop)>>,
    /// Fetches beyond the script and URL/`accept` mismatches.
    problems: Mutex<Vec<String>>,
}

impl ScriptedFetch {
    fn next(
        &self,
        url: &Url,
        got_accept: &'static str,
    ) -> Result<http::Response<Body>, FetchError> {
        let mut problems = self
            .problems
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = self
            .hops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        let Some((want_url, accept, hop)) = next else {
            problems.push(format!("unscripted fetch of {}", url.as_str()));
            return Err(FetchError::Unreachable);
        };
        if url.as_str() != want_url {
            problems.push(format!(
                "upstream request URL {} != scripted {want_url}",
                url.as_str()
            ));
            return Err(FetchError::Unreachable);
        }
        // A hop's declared `accept` is checked against what `preview`
        // actually passed for that fetch.
        if let Some(accept) = accept
            && accept != got_accept
        {
            problems.push(format!("hop accept {accept:?} != sent {got_accept:?}"));
            return Err(FetchError::Unreachable);
        }
        drop(problems);
        match hop {
            Hop::Respond(res) => {
                let (parts, chunks) = res.into_parts();
                let body: Body = Box::pin(stream::iter(chunks.into_iter().map(Ok)));
                Ok(http::Response::from_parts(parts, body))
            }
            Hop::Fail(err) => Err(err),
        }
    }
}

impl Fetch for ScriptedFetch {
    type Body = Body;

    fn get(
        &self,
        url: &Url,
        accept: &'static str,
    ) -> impl Future<Output = Result<http::Response<Self::Body>, FetchError>> + Send {
        std::future::ready(self.next(url, accept))
    }
}

/// Fresh per-vector runtime: in-memory cache, scripted fetch, scripted
/// admission, a clock fixed at [`NOW`], a timer that never fires.
struct FakeRuntime {
    fetch: ScriptedFetch,
    cache: Mutex<HashMap<String, http::Response<Bytes>>>,
    deny: Option<Bucket>,
}

impl Runtime for FakeRuntime {
    type Fetch = ScriptedFetch;

    fn fetcher(&self) -> &ScriptedFetch {
        &self.fetch
    }

    fn now_unix_secs(&self) -> u64 {
        NOW
    }

    fn sleep(&self, _duration: Duration) -> impl Future<Output = ()> + Send {
        std::future::pending()
    }

    fn admit(&self, bucket: Bucket, _key: &str) -> impl Future<Output = Admission> + Send {
        let verdict = if self.deny == Some(bucket) {
            Admission::Denied {
                retry_after_secs: DENY_RETRY_SECS,
            }
        } else {
            Admission::Allowed
        };
        std::future::ready(verdict)
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

/// Body chunk syntax: a string (one chunk), `{"text": s, "repeat": n}`, or
/// `{"bytes": [u8, …]}`.
fn chunk_bytes(chunk: &Value) -> Result<Bytes, String> {
    match chunk {
        Value::String(s) => Ok(Bytes::copy_from_slice(s.as_bytes())),
        Value::Object(map) => {
            if let Some(repeat) = map.get("repeat") {
                let text = map
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "chunk needs \"text\"".to_owned())?;
                let n = repeat
                    .as_u64()
                    .ok_or_else(|| "chunk needs integer \"repeat\"".to_owned())?;
                let n = usize::try_from(n).map_err(|_| "chunk repeat too big".to_owned())?;
                return Ok(Bytes::from(text.repeat(n)));
            }
            let bytes = map
                .get("bytes")
                .and_then(Value::as_array)
                .ok_or_else(|| "chunk needs \"bytes\" or \"text\"+\"repeat\"".to_owned())?;
            let mut out = Vec::with_capacity(bytes.len());
            for b in bytes {
                let v = b
                    .as_u64()
                    .ok_or_else(|| "chunk byte must be an integer".to_owned())?;
                out.push(u8::try_from(v).map_err(|_| "chunk byte out of range".to_owned())?);
            }
            Ok(Bytes::from(out))
        }
        _ => Err("body chunk must be a string or an object".to_owned()),
    }
}

fn hop_body(body: Option<&Value>) -> Result<Vec<Bytes>, String> {
    match body {
        None => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![Bytes::copy_from_slice(s.as_bytes())]),
        Some(Value::Array(chunks)) => chunks.iter().map(chunk_bytes).collect(),
        Some(_) => Err("body must be a string or an array of chunks".to_owned()),
    }
}

/// Stamps a hop's `headers` map onto a canned response; a name maps to one
/// string or to an array for repeated headers.
fn apply_headers(
    res: &mut http::Response<Vec<Bytes>>,
    headers: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    for (name, value) in headers {
        let name: HeaderName = name
            .parse()
            .map_err(|_| format!("bad header name {name:?}"))?;
        let values = match value {
            Value::String(v) => vec![v.clone()],
            Value::Array(vs) => vs
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "header values must be strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(format!("header {name} must be string or array")),
        };
        for v in values {
            let v = HeaderValue::from_str(&v).map_err(|_| "bad header value".to_owned())?;
            res.headers_mut().append(name.clone(), v);
        }
    }
    Ok(())
}

fn hops(upstream: Option<&Value>) -> Result<VecDeque<(String, Option<String>, Hop)>, String> {
    let mut out = VecDeque::new();
    let Some(list) = upstream else {
        return Ok(out);
    };
    let list = list
        .as_array()
        .ok_or_else(|| "upstream must be an array".to_owned())?;
    for hop in list {
        let url = hop
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "hop needs a url".to_owned())?
            .to_owned();
        let accept = hop.get("accept").and_then(Value::as_str).map(str::to_owned);
        let outcome = if let Some(err) = hop.get("error").and_then(Value::as_str) {
            let err = match err {
                "blocked" => FetchError::Blocked,
                "timeout" => FetchError::Timeout,
                "unreachable" => FetchError::Unreachable,
                other => return Err(format!("unknown hop error {other:?}")),
            };
            Hop::Fail(err)
        } else {
            let status = hop
                .get("status")
                .and_then(Value::as_u64)
                .ok_or_else(|| "hop needs status or error".to_owned())?;
            let status = StatusCode::from_u16(
                u16::try_from(status).map_err(|_| "hop status out of range".to_owned())?,
            )
            .map_err(|_| "hop status invalid".to_owned())?;
            let mut res = http::Response::new(hop_body(hop.get("body"))?);
            *res.status_mut() = status;
            if let Some(headers) = hop.get("headers").and_then(Value::as_object) {
                apply_headers(&mut res, headers)?;
            }
            Hop::Respond(res)
        };
        out.push_back((url, accept, outcome));
    }
    Ok(out)
}

/// Each expected header must appear exactly once with exactly the
/// expected value.
fn check_headers(
    actual: &http::HeaderMap,
    expected: &serde_json::Map<String, Value>,
    problems: &mut Vec<String>,
) {
    for (name, want) in expected {
        let all: Vec<_> = actual.get_all(name.as_str()).iter().collect();
        let want = want.as_str().unwrap_or_default();
        match all.as_slice() {
            [got] if got.to_str().ok() == Some(want) => {}
            [got] => problems.push(format!("header {name}: got {got:?}, want {want:?}")),
            _ => problems.push(format!("header {name}: {} values", all.len())),
        }
    }
}

/// Runs one vector file through `service::handle`; `Err` carries the
/// assertion failures joined with `"; "`.
fn run_vector(vector: &Value) -> Result<(), String> {
    let request = vector.get("request").cloned().unwrap_or(Value::Null);
    let method: Method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .parse()
        .map_err(|_| "bad request method".to_owned())?;
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("/v1/preview");
    let uri: Uri = match request.get("query").and_then(Value::as_str) {
        Some(query) if !query.is_empty() => format!("{path}?{query}"),
        _ => path.to_owned(),
    }
    .parse()
    .map_err(|_| "bad request uri".to_owned())?;
    let origin = request
        .get("origin")
        .and_then(Value::as_str)
        .map(HeaderValue::from_str)
        .transpose()
        .map_err(|_| "bad origin header".to_owned())?;

    let deny = vector
        .get("runtime")
        .and_then(|r| r.get("deny"))
        .and_then(Value::as_str)
        .map(|which| match which {
            "client" => Ok(Bucket::Client),
            "host" => Ok(Bucket::Host),
            other => Err(format!("runtime.deny must be client|host, not {other:?}")),
        })
        .transpose()?;

    let rt = FakeRuntime {
        fetch: ScriptedFetch {
            hops: Mutex::new(hops(vector.get("upstream"))?),
            problems: Mutex::new(Vec::new()),
        },
        cache: Mutex::new(HashMap::new()),
        deny,
    };
    let origins = Origins::parse("https://app.example").map_err(|e| e.to_string())?;

    let incoming = Incoming {
        method: &method,
        uri: &uri,
        origin: origin.as_ref(),
        client_ip: None,
    };
    let (res, record) = futures_executor::block_on(service::handle(&rt, &origins, incoming));

    let expect = vector
        .get("expect")
        .ok_or_else(|| "vector needs expect".to_owned())?;
    let mut problems: Vec<String> = Vec::new();

    // `corx-cache` is stamped after the cache lookup: it must be present
    // iff the request reached it (`record.cache`), never earlier, never
    // stored. Vectors do not list it — it is diagnostic, not contract.
    match (
        res.headers()
            .get(corx::http::CACHE_STATUS)
            .and_then(|v| v.to_str().ok()),
        record.cache.map(CacheStatus::as_str),
    ) {
        (None, None) => {}
        (Some(got), Some(want)) if got == want => {}
        (got, want) => problems.push(format!("corx-cache: got {got:?}, want {want:?}")),
    }

    if let Some(status) = expect.get("status").and_then(Value::as_u64)
        && u64::from(res.status().as_u16()) != status
    {
        problems.push(format!(
            "status: got {}, want {status}",
            res.status().as_u16()
        ));
    }
    if let Some(headers) = expect.get("headers").and_then(Value::as_object) {
        check_headers(res.headers(), headers, &mut problems);
    }
    let body = res.body();
    match expect.get("body") {
        Some(want) => {
            let got: Value = serde_json::from_slice(body)
                .map_err(|e| format!("response body is not JSON: {e}"))?;
            if &got != want {
                problems.push(format!("body: got {got}, want {want}"));
            }
        }
        None => {
            if !body.is_empty() {
                problems.push(format!("body must be empty, got {body:?}"));
            }
        }
    }

    let leftover = rt
        .fetch
        .hops
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    if leftover > 0 {
        problems.push(format!("{leftover} scripted hop(s) never fetched"));
    }
    problems.extend(
        rt.fetch
            .problems
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..),
    );
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn vectors_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contract/vectors")
        .canonicalize()
        .expect("contract/vectors must exist")
}

#[test]
fn all_vectors_pass() {
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(vectors_dir())
        .expect("read vectors dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    entries.sort();
    let count = entries.len();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_owned();
        let text = std::fs::read_to_string(&path).expect("read vector");
        let vector: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                failures.push((name, format!("invalid JSON: {e}")));
                continue;
            }
        };
        if let Err(e) = run_vector(&vector) {
            failures.push((name, e));
        }
    }
    assert!(
        failures.is_empty(),
        "{count} vectors, {} failing:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|(n, e)| format!("  {n}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
