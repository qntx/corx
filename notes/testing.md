# Testing & benchmarking

`corx` ships with three tiers of automated checks. The CI pipeline runs
every tier on `pull_request` and `push` events.

## Unit tests

```bash
cargo test --workspace --lib
cargo test --workspace --lib --all-features
```

Both invocations are required. `--all-features` flips on `tls`, `mtls`,
`fips`, and `otel`; the default build keeps the dependency graph small.

Notable suites:

- `corx_core::config::validate::tests` — rejects dangerous combinations.
- `corx_core::proxy::ssrf::tests` — covers strict / permissive modes,
  IPv4-mapped canonicalisation, and link-local denial.
- `corx_core::proxy::cors::tests` — wildcard / reflect / explicit
  policies, credentials-aware downgrade, allowlist enforcement.
- `corx_server::middleware::rate_limit::tests` — first-failing-dimension
  attribution across the four GCRA buckets.
- `corx_server::hot_reload::tests` — assert-immutable acceptance and
  rejection paths, plus field-wise TLS comparison.

## Integration tests

```bash
cargo test --workspace --all-features
```

Located in `crates/corx-server/tests/integration_proxy.rs`. The suite
stands up a fully-assembled router and a `wiremock` upstream, then drives
requests through `tower::ServiceExt::oneshot`. Coverage:

1. `/livez` and `/readyz` follow the readiness contract.
2. CORS preflight short-circuit returns `204` with reflect-mode headers.
3. Simple proxy forwards body and reflects upstream response.
4. SSRF guard blocks `localhost` (DNS-resolved) when not allow-listed.
5. Origin allow-list rejects blacklisted origins with `403`.
6. Forwarded / X-Forwarded-* / X-Request-Id headers are stamped.

## Benchmarks (criterion)

```bash
cargo bench --package corx-core
```

Two benches cover the hot path:

- `benches/url_parser.rs` exercises `extract_target` over five canonical
  URL shapes (scheme-less, full URL, query-string form, IDN punycode,
  deep path with query parameters).
- `benches/ssrf_guard.rs` exercises `SsrfGuard::check_ip` under both
  `strict` and `permissive` modes with public, loopback, link-local, and
  IPv6 addresses.

Targets are intentionally small so a regression shows up as a clear
percentage delta in the criterion output, not a nebulous absolute number.

## Linting

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check
```

`deny.toml` codifies the supply-chain policy (license allow-list,
yanked-deny, registry pinning).
