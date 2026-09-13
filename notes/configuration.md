# Configuration

`corx` reads a single TOML file at startup. Discovery order:

1. `--config <path>` (CLI flag)
2. `$CORX_CONFIG` (environment variable)
3. `./corx.toml`
4. `/etc/corx/config.toml`

A fully-commented sample lives at [`corx.example.toml`](../corx.example.toml).
Run `corx dump --format toml` to print the fully-defaulted configuration
that the binary is actually using.

## Sections at a glance

| Section                  | What it controls                                   |
| ------------------------ | -------------------------------------------------- |
| `[server]`               | Listener address, HTTP/2 toggle, graceful shutdown |
| `[server.tls]`           | Optional inbound TLS / mTLS material               |
| `[limits]`               | Body/header/response size, inflight, timeouts, redirects |
| `[cors]`                 | Allow-origin policy, methods, headers, credentials |
| `[security]`             | Required headers, origin allow/block, auth         |
| `[target]`               | Host/scheme admission (every hop including redirects) |
| `[circuit_breaker]`      | Per-host circuit breaker                           |
| `[ssrf]`                 | DNS-aware SSRF guard                               |
| `[forwarded]`            | RFC 7239 / X-Forwarded-* / X-Request-Id            |
| `[rate_limit.*]`         | Multi-dimensional GCRA rate limit                  |
| `[upstream]`             | HTTP client tuning                                 |
| `[observability]`        | Logging, metrics, OTLP traces                      |

## Hot-swappable vs immutable

A `SIGHUP` triggers a configuration reload. Fields fall into two camps:

- **Hot-swappable** (replaced atomically via `arc-swap`): `cors`,
  `security`, `target`, `ssrf`, `forwarded`, `rate_limit`,
  `circuit_breaker`, `upstream`, and related policy. When
  `rate_limit` / `circuit_breaker` sections are **unchanged**, in-memory
  GCRA buckets and open circuits are **retained** across reload. The
  upstream connection pool is retained unless ssrf/target/upstream/
  connect/redirect knobs change.
- **Immutable** (require a process restart): `server.bind`, `server.http2`,
  `server.tls`, `limits.max_request_body_bytes`,
  `limits.max_request_header_bytes`, `limits.max_response_body_bytes`,
  `limits.inflight_max`, `limits.request_timeout`,
  `observability.metrics_endpoint`.

Reload outcomes are reported via `corx_config_reload_total{result}` with
labels `ok`, `rejected` (immutable diff), and `error` (I/O / validation).

## Validation

`corx check` validates the configuration without binding any sockets.
Combine it with `--config` for CI gates:

```bash
corx check --config ./config.toml || exit 1
```

The validator rejects dangerous combinations such as
`cors.allow_credentials = true` paired with a wildcard origin policy.
