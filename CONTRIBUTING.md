# Contributing

Thanks for taking the time to improve corx.

## Prerequisites

- **Rust stable** via `rust-toolchain.toml`; the minimum supported version is
  `[workspace.package].rust-version` in `Cargo.toml`.
- **`wasm32-unknown-unknown` target** — the core and the Worker must build for
  it:

  ```bash
  rustup target add wasm32-unknown-unknown
  ```

- **cargo-deny** for the supply-chain check (`cargo install cargo-deny`).
- **Node.js** — `npx` runs wrangler and taplo; no other JS tooling is required.
- **Optional**: `just` (recipe shortcuts in `Justfile`), Docker (image build).

## Repository layout

```text
crates/corx/              sans-IO core (publishable; builds native + wasm32)
crates/corx-conformance/  black-box conformance suite for both editions
crates/corx-corpus/       the `corx-corpus` tool (corpus run/compare)
crates/corx-server/       the `corx` binary: axum + reqwest + governor + moka
crates/corx-worker/       Cloudflare Worker adapter (workers-rs, cdylib) and wrangler.jsonc
contract/                 the HTTP contract: prose, JSON schema, golden vectors
corpus/                   the URL corpus list + the UA-sweep script
Dockerfile                multi-stage cargo-chef image (distroless runtime)
.github/                  CI, release, docker and dependabot workflows
```

## Architecture invariants

These are the rules reviewers enforce:

- `corx` performs **no I/O** and must build for `wasm32-unknown-unknown`.
- The entire request pipeline lives only in `corx::service`: routing, origin
  checks, rate admission, cache lookup, the timed fetch and rendering. Both
  adapters implement `service::Runtime` (fetch, clock, timer, admission,
  cache) and convert types — nothing else.
- Every redirect hop goes through `Target::redirect` — never follow a
  `Location` blindly.
- The oEmbed fallback request is built with `Target::parse` and sent
  through the same `Fetch` port as any other upstream hop — same
  validation, same headers discipline.
- Native DNS answers are filtered by `ip::is_public` before connecting (no
  TOCTOU), and the HTTP client ignores proxies. The Worker relies on the
  `global_fetch_strictly_public` compatibility flag instead.
- Caches store CORS-free responses; `Access-Control-*` headers are applied
  after the cache lookup, never stored.
- URL-valued metadata fields are never truncated — an oversized URL is
  dropped whole (`clean_url`), while text fields truncate to their cap
  (`clean_text`).
- Logs never contain the full target URL or the client IP — only the target
  host and its `Target::fingerprint` (12 hex chars of SHA-256). Per-request
  diagnostics are limited to `upstream_status` (the last upstream status
  seen on a miss) and `hops` (redirects followed), both `None` for cache
  hits, rejects and preflights.
- No `unwrap`/`expect`/panics in non-test code; the workspace lint set runs
  under `-D warnings`.
- `Send`/`Sync` bounds in the core go through `MaybeSend`/`MaybeSync`, which
  are real bounds on native and vacuous on `wasm32`.

## HTTP contract

`GET|HEAD|OPTIONS /v1/preview?url=…`, its JSON fields, error slugs, status
codes and `Cache-Control` values are a **public contract** consumed by
clients (Meowl) — see `contract/` for the prose spec
(`contract/README.md`), the JSON schema (`contract/preview.schema.json`)
and the golden vectors (`contract/vectors/`). Changing any of them is a
breaking change. The `contract` and `vectors` integration tests in
`crates/corx` guard it: every rendered shape validates against the schema
and every vector replays through the pipeline.

## Local gate

Run before opening a pull request:

```bash
npx --yes @taplo/cli@0.7.0 fmt --check
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p corx -p corx-worker --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc -p corx --no-deps
cargo deny check
```

CI runs taplo, fmt, clippy, tests, rustdoc and cargo-deny on the native
workspace, and builds `corx` and `corx-worker` for `wasm32-unknown-unknown`.

## Running locally

Server:

```bash
cargo run -p corx-server -- --allowed-origins https://app.example.com
```

Every flag has a `CORX_*` environment variable; `corx --help` lists them.

Worker:

```bash
just worker-dev   # wrangler 4.149.0 (pinned), ALLOWED_ORIGINS=http://localhost:5173
```

or `npx wrangler@4.149.0 dev --var ALLOWED_ORIGINS:…` from
`crates/corx-worker`. The Cache API works in `wrangler dev` and on custom
domains/routes, but not on `*.workers.dev` hostnames.

Docker:

```bash
docker build -t corx . && docker run -e CORX_ALLOWED_ORIGINS='*' -p 8080:8080 corx
```

On OrbStack add `--dns 1.1.1.1`: its embedded resolver answers every name
with `198.18.0.0/15` addresses, which corx correctly rejects as reserved.

## Testing

- Pipeline behaviour is tested in `corx` with fake `Fetch`/`Runtime`
  implementations (scripted admissions, an in-memory cache, a controllable
  timer).
- `corx-server` tests cover only adapter-specific behaviour: miss
  coalescing, load shedding, client-IP sources, timeouts under
  `tokio::time::pause`.
- `NativeFetch` tests hit a local axum server via IP-literal URLs — the core
  rejects IP literals in `Target::parse`, never inside `get`.
- The black-box conformance suite in `crates/corx-conformance` runs the same
  HTTP assertions against both editions: `run.sh native` builds and boots
  `corx`, `run.sh worker` serves `crates/corx-worker` under
  `wrangler dev --local`. Each case asserts the exact response header set —
  no case needs the public web.
- The corpus tool (`just corpus <base-url>`) runs `corpus/urls.txt` against
  a live deployment and scores each URL against its expectation — it
  reports, never gates (the web changes). `corpus/ua-sweep.sh` does the
  same per upstream `USER_AGENT` to pick a deployment UA. The weekly
  advisory workflow (`corpus.yml`, plus `workflow_dispatch`) runs it on CI;
  on Macs whose resolver answers 198.18.0.0/15 (fake-IP proxies) the
  native server rejects every target — run `wrangler dev --local` and point
  the tool at the Worker for Worker-code numbers (measuring this machine's
  egress, not Cloudflare's), or use the workflow for native numbers.

## Dependencies

Only depend on versions published at least 7 days ago (the Dependabot
cooldown matches). Keep `cargo deny check` green. `worker-build` and
wrangler versions are pinned (0.8.7 and 4.149.0) — bump them deliberately.

## Versioning and release

Bump `[workspace.package].version`, update `CHANGELOG.md`, merge, then tag
`v*.*.*` on main. The tag publishes `corx` to crates.io, a multi-arch image
to GHCR, and a GitHub Release. Worker deployments run `wrangler deploy` from
the tagged checkout.

## Commits

English Conventional Commits: `type(scope): subject` (`feat`, `fix`, `docs`,
`style`, `refactor`, `perf`, `test`, `chore`, `ci`, `build`, `revert`),
subject in imperative mood, no period, ≤ 50 chars; `BREAKING CHANGE:` footer
for breaking changes.

## Documentation policy

`docs/` is local-only and gitignored; never commit it. Public behaviour is
documented in this file, `corx --help`, and `wrangler.jsonc` comments.

## License

Contributions are dual-licensed under MIT OR Apache-2.0.
