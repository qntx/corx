# Changelog

All notable changes to `corx` are documented in this file.

The format is loosely based on [Keep a Changelog]; the project follows
[Semantic Versioning] for the public API exposed by `corx-core` and the
binary CLI surface.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html

## [Unreleased]

### Added

- `[security.preflight]` — `mode = "enforce"|"open"` (default `enforce`) and
  `rate_limit` (default `true`) so preflights participate in origin/rate
  guards by default.
- `[security.auth]` — optional `bearer` shared-secret authentication.
- `security.require_client_binding` — hard-fail startup without origin
  whitelist, bearer auth, or mTLS.
- `[target]` — host/scheme admission (`any_public` | `allowlist` | `denylist`),
  enforced on **every redirect hop**.
- `[circuit_breaker]` — process-local per-host circuit breaker (`max_hosts`
  soft cap, default 8192).
- `limits.redirect_policy` — `follow` | `block` | `rewrite` (rewrite stamps
  proxy path-prefix `Location`).
- `limits.inflight_max` — process concurrency load-shed (moved from
  `rate_limit.global`).
- `limits.max_response_body_bytes` — streaming response size cap (default
  50 MiB; `0` = unlimited).
- `rate_limit.max_keys` — fail-closed cardinality cap for keyed GCRA maps.
- `cors.origins` + `cors.allow_any_origin` (unified origin list).
- CatchPanic layer; load-shed inflight RAII permit; hot-reload retains
  circuit / rate-limit maps when those config sections are unchanged.

### Changed (BREAKING)

- `rate_limit.global.inflight_max` **removed** — use `limits.inflight_max`.
- `rate_limit.enabled` default is now **`true`** (GCRA on by default).
- Client error JSON `message` is kind-stable (no internal DNS/connect detail).
- `CircuitDecision` public enum removed; `CircuitBreaker::check` returns
  `Result<(), ProxyError>`.
- CORS middleware sits outside auth/load-shed/header-limit so 401/503/431
  include CORS headers; success path no longer double-stamps CORS in the
  handler.

- Cleartext listeners honour `server.graceful_shutdown` with a force-abort
  deadline after SIGTERM/Ctrl+C (previously only the TLS path used the
  duration).
- Default Helm chart `config` aligned with `corx-core` serde shapes.
- CORS: removed `allowlist` / `explicit` fields; use `origins` +
  `allow_any_origin` (default `false` — fail-closed reflection).
- `corx-core` depends on `tower-service` only (not full `tower`).
- **Workspace layout:** `corx` is now the **umbrella library**; the binary
  lives in package `corx-cli` (binary name still `corx`). Embedders should
  depend on `corx`. Run with `cargo run -p corx-cli`.
- Dependency floor raised across the workspace (hyper 1.11, tokio 1.53,
  governor 0.10, metrics-exporter-prometheus 0.18, OpenTelemetry 0.30
  family, axum-server 0.8, rustls-platform-verifier 0.6, …). See
  `Cargo.toml` `[workspace.dependencies]`.
- `Upstream::new` is now fallible (platform TLS verifier load).

### Fixed

- `limits.max_request_header_bytes` is enforced (`431`).
- Preflight no longer bypasses origin blacklist / whitelist when
  `security.preflight.mode = "enforce"`.

### Removed

- Placeholder WebSocket metrics and related documentation (capability was
  never implemented).
- CORS config fields `allowlist` and `explicit`.

## [0.2.0] -- enterprise upgrade

### Added

- **Workspace split** -- engine moved to `corx-core` (no `axum` deps),
  axum / tower glue lives in `corx-server`, the binary is `corx`. Library
  consumers can embed the engine under any HTTP framework.
- **SSRF v2** -- DNS-aware guard sits between the URL parser and hyper's
  connector; IPv4-mapped IPv6 is folded back to IPv4; happy-eyeballs
  works because every admissible address from a single lookup is
  returned.
- **CORS v2** -- `allowed_methods` / `allowed_headers` /
  `exposed_headers` / `allow_private_network` / `max_age`; CORS headers
  are stamped on error responses too.
- **URL parser v2 + redirect v2** -- IDN punycode, scheme normalisation,
  per-redirect-hop SSRF re-validation, https-to-http downgrade gate.
- **Multi-dimensional rate limiting** -- per-Origin, per-IP,
  per-target-host, and global GCRA buckets backed by `governor`. The
  first failing dimension wins so rejections are attributed via
  `corx_rate_limited_total{dimension}`.
- **Load shed** -- atomic in-flight counter behind
  `rate_limit.global.inflight_max` short-circuits over-capacity requests
  with `503 Service Unavailable`.
- **Inbound TLS / mTLS / FIPS** -- new `corx-server::tls` module compiles
  a `rustls::ServerConfig` from operator-supplied PEM material; mTLS is
  feature-gated; the `fips` feature swaps in `aws-lc-rs`.
- **Health probes** -- dedicated `/livez` and `/readyz` endpoints;
  `/healthz` is kept as an alias.
- **Graceful shutdown v2** -- the readiness flag flips to `false` the
  moment a shutdown signal arrives so load balancers stop routing while
  in-flight requests drain.
- **Structured access log** -- `corx::access` event per completed request
  with method, path, status, duration, client IP, origin, request ID,
  and error kind, layered at the outermost router position.
- **Observability** -- comprehensive metric set including streaming byte
  counters via `CountingBody`; `build_info` gauge with version /
  rust_version / features labels; OpenTelemetry / OTLP traces under
  feature `otel`.
- **Hot reload** -- `arc-swap`-based `LivePolicies` snapshot atomically
  swapped on `SIGHUP`; immutable fields rejected with explicit log
  messages.
- **CLI subcommands** -- `corx serve` (default), `corx check`, `corx
  dump --format toml|json`, `corx version`. Bare `corx` keeps invoking
  `serve` for backwards compatibility.
- **Release engineering** -- multi-stage cargo-chef Dockerfile producing
  multi-arch images; `docker-compose.yml` with Prometheus / Grafana /
  OTLP collector; Helm chart at `charts/corx`; GitHub Actions workflows
  for image build + cosign signing + CycloneDX SBOM + provenance
  attestation, and Helm chart lint / kubeconform / OCI publish.
- **Documentation** -- nine-doc operator and contributor guide under
  `docs/`, plus this changelog.
- **Tests** -- 73 unit tests plus 7 integration tests under
  `tower::ServiceExt::oneshot` against a `wiremock` upstream.
- **Benchmarks** -- two criterion benches covering `extract_target` and
  `SsrfGuard::check_ip`.

### Changed (BREAKING)

- `RateLimitConfig` -- flat single-bucket schema replaced with four
  nested sub-configs (`origin`, `ip`, `target_host`, `global`).
- `TlsConfig` -- gains `client_ca_path` and `alpn_protocols`.
- `SsrfConfig` -- `enabled: bool` replaced by `mode: SsrfMode { Strict |
  Permissive { allow_private } }`.
- `ServerBuild` -- `cors`, `request_filter`, `response_filter`, `guard`,
  `upstream`, `config` fields removed; access via
  `ServerBuild::policies()`. Listener-level metadata moves to
  `immutable_*` fields.
- `shutdown::run` -- now takes `Arc<AtomicBool>` ready flag.
- `Cli` -- gains a `Command` enum (`Serve` / `Check` / `Dump` /
  `Version`); `--config` is now a global flag.
- Default `cors.policy.kind` flipped from `wildcard` to `reflect` to
  match the production-safe default.
- Workspace name surface -- imports change from `corx::*` to
  `corx_core::*` / `corx_server::*` (see [notes/migration.md]).

[notes/migration.md]: notes/migration.md

## [0.1.0] -- MVP

Initial cors-anywhere-compatible release. Single-crate, single-bucket
rate limit, basic SSRF guard, no hot reload, no TLS termination.
