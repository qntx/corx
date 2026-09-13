# Security model

`corx` is designed to be safe to expose to the open internet. Every layer
defaults to the strictest setting that still allows the cors-anywhere use
case to function.

## Defence layers

```text
   client
     │
     ▼
+---------+   1. TLS / mTLS (feature `tls` / `mtls`)
| listener|
+---------+
     │
     ▼
+---------+   2. Body / header size limits
| router  |   3. Timeouts
+---------+   4. Load shed (global inflight cap)
     │
     ▼
+---------+   5. Origin allow/deny + required headers
| guards  |   6. Multi-dimensional rate limit
+---------+   7. CORS preflight short-circuit (after guards by default)
     │
     ▼
+---------+   8. URL parser (RFC 3986 + scheme normaliser)
| target  |   9. SSRF guard (DNS-aware, IPv4-mapped canonicalisation)
+---------+   10. Redirect guard (max hops, https-to-http downgrade gate)
     │
     ▼
+---------+   11. Outbound HTTP client (rustls / aws-lc-rs)
| upstream|
+---------+
```

## Preflight gating

By default (`security.preflight.mode = "enforce"`) `OPTIONS` preflights run
the same origin / method / required-header guards as normal requests, and
optionally charge the rate limiter (`security.preflight.rate_limit = true`).
Blacklisted origins therefore cannot harvest `204` responses.

Set `security.preflight.mode = "open"` only when you intentionally want
classic cors-anywhere behaviour (preflight before guards).

## Target admission

`[target]` filters hosts and schemes **before** DNS/SSRF on the first hop,
and again on **every redirect hop** before connect:

- `any_public` (default) — any host; SSRF still applies to resolved IPs
- `allowlist` / `denylist` — exact hosts or DNS suffixes (`.example.com`)
- `https_only` — reject non-HTTPS targets

## Authentication

`[security.auth]` with `mode = "bearer"` requires
`Authorization: Bearer <token>` on non-OPTIONS proxy traffic (ops routes and
preflights are exempt). Use `security.require_client_binding = true` to refuse
startup without origin whitelist, bearer tokens, or mTLS.

## Circuit breaker

`[circuit_breaker]` tracks consecutive upstream failures per target host in
process memory and returns `503` / `circuit_open` while open. Not shared
across replicas.

## SSRF guard

The guard sits **between** the URL parser and hyper's connector, so it is
impossible to bypass by aiming the proxy at an IP literal:

- Default block-list covers RFC 1918, loopback, link-local (incl. cloud
  metadata), and IPv6 reserved ranges.
- IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`) are folded back to IPv4
  before evaluation.
- Operators add carve-outs via `ssrf.extra_allowed_cidrs`; they take
  precedence and are how you intentionally proxy to internal API gateways.
- The same checks run on every redirect hop because the SSRF-aware
  resolver is plugged into the underlying hyper connector.

## CORS policy

- `wildcard` returns `Access-Control-Allow-Origin: *`.
- `reflect` (default) echoes the request `Origin`, optionally constrained
  by `cors.allowlist`.
- `explicit` returns the request `Origin` only if it is on `cors.explicit`.

`allow_credentials = true` automatically downgrades wildcard mode to
reflection because browsers reject `*` together with credentials.

CORS headers are also stamped on **error** responses so browsers can read
the JSON error body.

## Origin allow/deny lists

`security.origin_blacklist` and `origin_whitelist` are exact-match string
lists evaluated before the rate limiter, so blocked origins never burn
through limit budget.

## Rate limit dimensions

Each dimension is an independent GCRA bucket. The first dimension to fail
is the one Prometheus attributes the rejection to via
`corx_rate_limited_total{dimension}`:

- `origin` (per `Origin` header)
- `ip` (per remote address; CIDRs in `trusted_cidrs` are exempted)
- `target_host` (per validated upstream host)
- `global` (process-wide GCRA)
- process **inflight** concurrency lives under `limits.inflight_max`
  (metric dimension `inflight`, independent of GCRA `enabled`)

## TLS / mTLS / FIPS

The optional `tls`, `mtls`, and `fips` Cargo features compile in inbound
TLS termination, mutual-TLS verification, and the aws-lc-rs FIPS-validated
crypto provider respectively. See
[`docs/deployment.md`](deployment.md#tls) for the full operator workflow.

## Header hygiene

By default `corx` strips `cookie` / `cookie2` from inbound requests and
`set-cookie` / `set-cookie2` from upstream responses, eliminating an entire
class of CSRF vectors. Tweak `security.remove_request_headers` and
`security.remove_response_headers` for additional fields.
