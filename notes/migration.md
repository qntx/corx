# Migration

This guide tracks breaking changes between the MVP (`0.1.x`) and the
enterprise build (`0.2.x`). Per the project conventions, backwards
compatibility is **not** maintained between these lines; the goal is a
clean, opinionated configuration surface rather than a sprawling one.

## Workspace split

The single `corx` crate became a workspace of three crates:

| Old import path                | New import path                              |
| ------------------------------ | -------------------------------------------- |
| `corx::config::Config`         | `corx_core::config::Config`                  |
| `corx::error::ProxyError`      | `corx_core::error::ProxyError`               |
| `corx::proxy::*`               | `corx_core::proxy::*`                        |
| `corx::AppState`               | `corx_server::AppState`                      |
| `corx::ServerBuild`            | `corx_server::ServerBuild`                   |

Embed `corx_core` if you want the engine without `axum`; embed
`corx_server` if you want the assembled router. The `corx` crate itself
is now binary-only.

## Configuration

### Top-level

`[forwarded]` is a new section with sane defaults; existing configs that
omit it pick up `inject = true`, `inject_request_id = true`,
`trust_inbound_xff = false`.

### Rate limit and concurrency

The flat single-bucket schema was replaced with multi-dimensional GCRA
plus a separate process inflight cap:

```toml
# OLD (0.1)
[rate_limit]
rps = 10
burst = 20

# NEW (0.2+)
[limits]
inflight_max = 1024                 # load-shed (was rate_limit.global.inflight_max)
max_response_body_bytes = 52428800  # streaming response cap; 0 = unlimited

[rate_limit]
enabled = true                      # default true
max_keys = 16384                    # fail-closed cardinality for keyed dims

[rate_limit.origin]
rps = 50
burst = 100
unlimited_patterns = []

[rate_limit.ip]
rps = 20
burst = 40
trusted_cidrs = []

[rate_limit.target_host]
rps = 100
burst = 200

[rate_limit.global]
rps = 5000
burst = 10000
# inflight_max removed — use limits.inflight_max
```

Set any sub-section's `rps` to `0` to disable that GCRA dimension while
leaving the others active. Set `[rate_limit].enabled = false` to disable
every GCRA dimension at once (inflight load-shed is independent).

### Target admission on redirects

`[target]` allowlist / denylist / `https_only` apply on the **first hop
and every redirect hop**. Configs that relied on following 3xx to hosts
outside the allowlist will now get `403` / `target_not_allowed`.

### CORS

`cors.allowed_methods`, `allowed_headers`, `exposed_headers`,
`allow_private_network` and `max_age` are new. `cors.allow_credentials =
true` paired with `cors.policy.kind = "wildcard"` is now rejected at
startup; corx degrades wildcard to reflection automatically when
credentials are on, so this combination is unambiguously documented.

### SSRF

`ssrf.mode` replaces the old `enabled` boolean. Valid values:

- `strict` (default) — reject every IP in a blocked CIDR.
- `permissive` with `allow_private = true|false` — allow public IPs but
  optionally also private IPs. Useful in heavily firewalled
  environments where the proxy must reach internal services.

The SSRF guard is consulted on every hop, including redirects, because
`Upstream::execute` re-runs the resolver on each follow-up.

### TLS

`server.tls.client_ca_path` (mTLS) and `server.tls.alpn_protocols` are
new. ALPN defaults to `["h2", "http/1.1"]`; supply a one-element vector
to negotiate strictly HTTP/1.1 if the upstream cannot speak HTTP/2.

### Observability

`observability.otel.*` sub-table is new and inert by default. Compile
with `--features otel` to actually export.

## API changes

`ServerBuild`'s public field surface was reshaped:

- `cors`, `request_filter`, `response_filter`, `guard`, `upstream`, and
  `config` are no longer direct fields.
- Fetch a snapshot via `ServerBuild::policies()` (an `ArcSwap` guard).
- Listener-level metadata moved to `immutable_server` /
  `immutable_limits` / `immutable_metrics_endpoint`.

`shutdown::run` now takes `Arc<AtomicBool>` for the readiness flag so
`/readyz` can flip to `503` the instant a shutdown signal lands.

## CLI

The bare `corx --config foo.toml` invocation still works (it dispatches
to `serve`), but the canonical surface is now subcommands:

```bash
corx serve [--config PATH]
corx check [--config PATH]
corx dump  [--config PATH] [--format toml|json]
corx version
```
