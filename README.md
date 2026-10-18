<!-- markdownlint-disable MD033 MD041 -->

# corx

Link-preview service: one pure Rust core plus two thin runtimes — a native
binary and a Cloudflare Worker. Give it a URL, get back the Open Graph /
Twitter Card / standard metadata needed to render a link card.

## Layout

```text
crates/corx          pure core (no I/O; compiles native + wasm32)
crates/corx-server   native binary (`corx`)
crates/corx-worker   Cloudflare Worker (workers-rs, wasm32)
```

The core owns URL validation, SSRF rules, redirect policy, HTML metadata
extraction, response shaping and CORS rules. The adapters only do I/O.

## API

`GET /v1/preview?url=<absolute http(s) URL>` → JSON:

```json
{
  "url": "https://a.com",
  "finalUrl": "https://a.com/",
  "title": "…", "description": "…", "siteName": "…",
  "image": { "url": "…", "width": 1200, "height": 630, "alt": "…" },
  "favicon": "…", "card": "…", "type": "…", "themeColor": "…", "lang": "…",
  "fetchedAt": 0
}
```

Errors are `{"error": "<slug>", "message": "…"}` with stable statuses:

| Status | Slug                  | Cache-Control                    |
|--------|-----------------------|----------------------------------|
| 400    | `invalid_url`         | `no-store`                       |
| 403    | `forbidden_origin`    | `no-store`                       |
| 403    | `blocked_target`      | `no-store`                       |
| 404    | `not_found`           | `no-store`                       |
| 404    | `no_metadata`         | `public, max-age=3600`           |
| 405    | `method_not_allowed`  | `no-store`                       |
| 413    | `too_large`           | `no-store`                       |
| 415    | `not_html`            | `no-store`                       |
| 422    | `upstream_status`     | `public, max-age=3600`           |
| 429    | `rate_limited`        | `no-store`                       |
| 502    | `upstream_unreachable`| `no-store`                       |
| 503    | `overloaded`          | `no-store`                       |
| 504    | `upstream_timeout`    | `no-store`                       |

Success: `Cache-Control: public, max-age=86400, stale-while-revalidate=604800`.
`OPTIONS /v1/preview` answers CORS preflights; `GET /healthz` returns `ok`.
405 responses carry `Allow: GET, HEAD, OPTIONS`.

## Security model

- Targets must be absolute `http(s)` URLs with no credentials, no explicit
  port, a multi-label hostname, and no private/reserved hostnames.
- IP literals and every redirect hop are validated against public-range
  tables (0/8, 10/8, 127/8, 169.254/16, 172.16/12, 192.168/16, CGNAT,
  documentation ranges, multicast, …; IPv6 restricted to 2000::/3 minus
  reserved sub-ranges).
- Native: DNS answers are filtered through the same public-range table
  before reqwest connects (no TOCTOU).
- Worker: `global_fetch_strictly_public` routes upstream fetches via the
  public internet only.
- Redirects are followed manually (max 5), validated per hop; `https → http`
  downgrades are refused.
- Responses are capped at 1 MiB and 4 s total; non-HTML content types are
  rejected.
- Rate limiting: per client IP (60/min sustained, 120 burst) and per target
  host (120/min). Clients whose IP cannot be extracted share one bucket.
- Logs are one JSON line per request containing method, route, status,
  latency, cache result, target host and a 12-char SHA-256 fingerprint —
  never the full URL.

## Running the server

```bash
cargo run -p corx-server -- --allowed-origins https://app.example.com
```

Environment variables (each flag has a `CORX_*` equivalent):

| Flag / env                          | Default                                  |
|-------------------------------------|------------------------------------------|
| `--bind` / `CORX_BIND`              | `0.0.0.0:8080`                           |
| `--allowed-origins` / `CORX_ALLOWED_ORIGINS` | **required** (comma list, `*`, or empty = native clients only) |
| `--user-agent` / `CORX_USER_AGENT`  | `corx/<version> (+https://github.com/qntx/corx)` |
| `--client-ip-source` / `CORX_CLIENT_IP_SOURCE` | `ConnectInfo`                   |
| `--ip-rate-per-minute` / `CORX_IP_RATE_PER_MINUTE` | `60`                        |
| `--ip-burst` / `CORX_IP_BURST`      | `120`                                    |
| `--host-rate-per-minute` / `CORX_HOST_RATE_PER_MINUTE` | `120`              |
| `--max-concurrency` / `CORX_MAX_CONCURRENCY` | `512`                          |
| `--cache-capacity` / `CORX_CACHE_CAPACITY` | `100000`                          |
| `--metrics-bind` / `CORX_METRICS_BIND` | unset — Prometheus exporter address   |
| `--log-format` / `CORX_LOG_FORMAT`  | `json` (`pretty` also available)         |
| `RUST_LOG`                          | `info`                                   |

Docker: `docker build -t corx . && docker run -e CORX_ALLOWED_ORIGINS='*' -p 8080:8080 corx`

## Deploying the Worker

```bash
just worker-dev      # local dev server
just worker-deploy   # deploy via wrangler
```

Set `ALLOWED_ORIGINS` (and optionally `USER_AGENT`) in
`crates/corx-worker/wrangler.jsonc` or at deploy time:

```bash
npx wrangler@4.149.0 deploy --var ALLOWED_ORIGINS:https://app.example.com
```

An empty list means no browser `Origin` is allowed (native clients only).
The Cache API only works on a custom domain/route — it is a no-op on
`*.workers.dev` and in `wrangler dev`.

## Development

```bash
just fmt clippy test   # fmt + strict clippy + tests
just clippy-wasm       # wasm32 clippy for core + worker
just check-wasm        # wasm32 typecheck
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be dual-licensed as above, without any additional terms or conditions.

---

<div align="center">

A **[QuantX](https://qntx.org)** open-source project.

<a href="https://qntx.org"><img alt="QuantX" width="369" src="https://raw.githubusercontent.com/qntx/.github/main/profile/qntx.svg" /></a>

Code is law. We write both.

</div>
