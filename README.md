# corx

[![Crates.io][crates-badge]][crates-url]
[![Docs.rs][docs-badge]][docs-url]
[![CI][ci-badge]][ci-url]
[![License][license-badge]][license-url]
[![Rust][rust-badge]][rust-url]

[crates-badge]: https://img.shields.io/crates/v/corx.svg
[crates-url]: https://crates.io/crates/corx
[docs-badge]: https://img.shields.io/docsrs/corx.svg
[docs-url]: https://docs.rs/corx
[ci-badge]: https://github.com/qntx/corx/actions/workflows/ci.yml/badge.svg
[ci-url]: https://github.com/qntx/corx/actions/workflows/ci.yml
[license-badge]: https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg
[license-url]: LICENSE-MIT
[rust-badge]: https://img.shields.io/badge/rust-edition%202024-orange.svg
[rust-url]: https://doc.rust-lang.org/edition-guide/

**High-performance CORS forwarding proxy in Rust — stream any HTTP(S) target through a single binary, synthesise browser CORS, fail-closed SSRF by construction.**

`corx` sits between browsers and upstream APIs that omit CORS. Path-prefix URL semantics match the classic [cors-anywhere](https://github.com/Rob--W/cors-anywhere) pattern (`/https://api.example.com/...`), while the hot path is zero-copy streaming on hyper 1.x + axum 0.8 + tokio: request and response bodies are forwarded chunk-by-chunk, connections are pooled, outbound TLS and DNS are pure Rust, and every resolved address is vetted by an SSRF guard *before* the TCP connect.

> **Embed** via the [`corx`](https://docs.rs/corx) umbrella crate (`corx-core` engine + `corx-server` binding). **Run** the CLI package `corx-cli` (binary name remains `corx`).

## Quick Start

### Install the CLI

Via Cargo:

```bash
cargo install corx-cli
# or from a checkout:
cargo install --path crates/corx-cli
```

Optional features: `--features tls`, `mtls`, `otel`, or `full`.

### Run

```bash
# From this repo
cargo run --release -p corx-cli -- serve --config corx.example.toml

# Or after install (defaults + env / config file)
corx serve --config corx.example.toml
```

Proxy a request (path-prefix target URL):

```bash
curl -H 'Origin: http://localhost' \
     'http://localhost:8080/https://api.github.com/repos/qntx/corx'
```

### Container

```bash
docker build -t corx:dev .
docker run --rm -p 8080:8080 corx:dev

# Full local stack (corx + Prometheus + Grafana + OTLP collector)
docker compose up -d
```

Multi-arch GHCR images are published **on version tags** only (`v*`); see [Deployment](docs/deployment.md).

### CLI Usage

```bash
corx serve              # default; bind and proxy
corx check              # validate config, exit non-zero on failure
corx dump               # print resolved config (--format toml|json)
corx version            # version + os/arch + active features
```

Config sources, increasing precedence:

1. Built-in defaults  
2. `$CORX_CONFIG`, or `./corx.toml`, or `/etc/corx/config.toml`  
3. Environment variables `CORX_*` (nested keys use `__`, e.g. `CORX_SERVER__BIND=0.0.0.0:9000`)  
4. CLI `--config`

Full knob reference: [`corx.example.toml`](corx.example.toml) · [Configuration](docs/configuration.md).

### Library Usage

```toml
# Umbrella facade (recommended for embedders)
corx = { version = "0.2", features = ["tls", "otel"] }
# Narrower graphs: corx-core (engine only) or corx-server (axum binding)
```

```rust
use corx::{AppState, Config, ServerBuild, build_router, run};
use corx::server::config_loader;
use corx::server::observability::{init_metrics, init_tracing};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config_loader::load(None)?;
    init_tracing(&config.observability)?;
    let metrics = init_metrics()?;
    let build = ServerBuild::from_config(config.clone(), metrics)?;
    let ready = std::sync::Arc::clone(&build.ready);
    let router = build_router(AppState::new(build));
    run(&config.server, router, ready).await
}
```

Feature flags on `corx` / `corx-cli`: `tls`, `mtls`, `fips`, `otel`, `full`.

## Highlights

| Capability | What it does |
| ---------- | ------------ |
| **Streaming proxy** | End-to-end body streaming; no full-buffer on the hot path |
| **SSRF-safe DNS** | `GuardedResolver` + CIDR policy (RFC 1918, metadata, loopback, …); IPv4-mapped IPv6 folded; re-check every redirect hop |
| **CORS policies** | `wildcard` / `reflect` / `explicit` via `origins` + `allow_any_origin`; preflights short-circuit; CORS on error bodies |
| **Preflight guards** | Default `security.preflight.mode = enforce` — OPTIONS run origin (and optional rate) guards |
| **Target admission** | `any_public` / `allowlist` / `denylist`, schemes, `https_only` |
| **Rate limit + shed** | GCRA per Origin / IP / target host / global; inflight load-shed → 503 |
| **Circuit breaker** | Process-local per-host open/half-open; optional 5xx counting |
| **Auth** | Optional bearer tokens; mTLS via features; `require_client_binding` |
| **Observability** | JSON/`pretty` access logs, Prometheus, OTLP traces (`otel`) |
| **Ops** | `/livez`, `/readyz` (503 while draining), `/healthz`, `/iscorsneeded`; SIGHUP hot reload via `arc-swap` |
| **Limits** | Body + **header** ceilings, request timeout, redirect `follow` / `block` / `rewrite` |

## Design

- **cors-anywhere semantics** — path-prefix absolute URLs, cookie stripping by default, Origin allow/deny lists, optional require-header  
- **Fail-closed defaults** — strict SSRF mode, CORS reflect requires `allow_any_origin` or non-empty `origins`, CONNECT/TRACE blocked  
- **Enterprise self-host** — single static binary, Helm chart, distroless image, cargo-deny, no Redis required for the core path  
- **Layered crates** — `corx-cli` → `corx` → (`corx-server` → `corx-core`); engine free of axum, only `tower-service` for hyper's DNS `Service`  
- **Strict workspace lints** — Clippy pedantic/nursery/correctness, `forbid(unsafe_code)`, `rust_2018_idioms` deny  

## Crates

| Crate | Role |
| ----- | ---- |
| [`corx`](crates/corx) | Umbrella library — re-exports engine + server (embed here) |
| [`corx-core`](crates/corx-core) | Framework-agnostic proxy engine, config, policy, SSRF |
| [`corx-server`](crates/corx-server) | axum/tower middleware, TLS, metrics, lifecycle |
| [`corx-cli`](crates/corx-cli) | Binary package (`corx` command) |

See [Architecture](docs/architecture.md) for the request pipeline and hot-reload model.

## Documentation

| Doc | Audience |
| --- | -------- |
| [Getting started](docs/getting-started.md) | New users |
| [Configuration](docs/configuration.md) | Operators |
| [Security model](docs/security.md) | Operators / sec-eng |
| [Observability](docs/observability.md) | SRE / platform |
| [Operations](docs/operations.md) | SRE / on-call |
| [Deployment](docs/deployment.md) | Platform |
| [Architecture](docs/architecture.md) | Contributors |
| [Migration 0.1 → 0.2](docs/migration.md) | Upgrade owners |
| [Testing & benchmarks](docs/testing.md) | Contributors |
| [Changelog](CHANGELOG.md) | Everyone |

## Security

`corx` is intended to be safe to expose when configured for production. It has **not** been independently audited. Use at your own risk and review [docs/security.md](docs/security.md).

- SSRF: DNS results filtered before connect; Strict mode blocks private/metadata CIDRs by default  
- Preflight and proxy share origin guards unless `security.preflight.mode = "open"`  
- Default strip of cookie / set-cookie; configurable header denylists  
- Optional bearer auth and mTLS; prefer `security.require_client_binding` on public deploys  
- `cargo deny` licenses/advisories/sources in CI  

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be dual-licensed as above, without any additional terms or conditions.

---

<div align="center">

A **[QuantX](https://qntx.fun)** open-source project.

<a href="https://qntx.fun"><img alt="QuantX" width="369" src="https://raw.githubusercontent.com/qntx/.github/main/profile/qntx.svg" /></a>

Code is law. We write both.

</div>
