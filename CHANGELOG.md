# Changelog

All notable changes to `corx` are documented in this file.

The format is loosely based on [Keep a Changelog]; the project follows
[Semantic Versioning] for the public API exposed by the `corx` crate and
the HTTP contract of the preview endpoint.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html

## [0.4.0] - Unreleased

### Changed (BREAKING)

- HTTP contract reshaped: success bodies are a union tagged on `kind`
  (`page` | `media`). Pages gain `source` (`html` | `oembed`) and need a
  title or an image. A non-HTML final hop becomes `media` (`mediaType`,
  `size`). `unsupported_content` replaces `not_html` and only covers a
  missing or unparseable Content-Type (#61).
- `redirect_failed` replaces `upstream_status` for redirect problems and
  `upstream_status` bodies now carry `upstreamStatus` (the final hop's
  status); the total request budget is 6 s (#62).
- `Fetch::get` takes the `Accept` value as a parameter (#63).
- Rust 1.100 toolchain; worker 0.9.0, worker-build 0.9.0, wrangler
  4.164.0 and `compatibility_date` 2026-11-17; version bump to 0.4.0.

### Added

- `contract/` directory: prose contract, JSON schema and 71 golden
  vectors validating the HTTP shape (#61).
- Per-request diagnostics: `upstream_status`, `hops` and `oembed_status`
  on the `Record`/structured logs of both adapters (#62, #63).
- oEmbed fallback for reddit.com and tiktok.com when HTML extraction
  fails or yields too little (#63).
- Black-box conformance suite `corx-conformance` running the same cases
  against both editions, wired into CI (#60).
- `corx-corpus`: corpus runner/compare tool, the URL corpus,
  `corpus/ua-sweep.sh` and the weekly advisory corpus workflow (#64).
- cargo-fuzz targets (`extract`, `target`) in an isolated `fuzz/`
  workspace plus a PR job and a daily scheduled workflow (#65).

### Fixed

- The Worker sent `content-type: application/octet-stream, application/json`
  on every response (three values on cache hits), and cache hits leaked
  `Age`, `CF-Cache-Status` and a frozen `Date` (#60).
- The extractor panicked when the lol_html rewriter was poisoned (e.g. by
  a `<title/>` inside SVG), which aborts the process in release builds: a
  crafted page took down the native server (#65).
- Title extraction depended on body chunk boundaries when `done` fired
  mid-title (#65).

### Security

- Lockfile updates resolving previously open RustSec advisories (#59).
- Still open: RUSTSEC-2026-0592 (`url`; the fix, 2.5.9, is younger than
  the 7-day rule until 2026-11-28) and RUSTSEC-2026-0543 (`chrono` is
  unmaintained and a non-optional dependency of every `worker` release).

## [0.3.0] - 2026-10-18

### Changed (BREAKING)

- Complete rewrite: corx is now a **link-preview service** (`GET
  /v1/preview?url=…` → JSON metadata) instead of a CORS forwarding proxy.
- One pure core crate `crates/corx` (no I/O, compiles on native and
  `wasm32-unknown-unknown`) with two thin adapters: `corx-server` (native
  Axum binary) and `corx-worker` (Cloudflare Worker via workers-rs).
- The full request pipeline — routing, CORS, rate admission, shared cache,
  timed preview fetch and rendering — lives in `corx::service`; both
  adapters only implement the `service::Runtime` trait, so behaviour is
  identical across runtimes.
- Public docs removed in favour of `CONTRIBUTING.md`; `docs/` is local-only.
- TOML is formatted by taplo (`.taplo.toml`).
- All previous configuration (TOML file, CLI subcommands) replaced by a
  small set of `CORX_*` env vars / flags.
- Helm chart, Compose files and the old proxy code have been removed.

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
