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
crates/corx/          sans-IO core (publishable; builds native + wasm32)
crates/corx-server/   the `corx` binary: axum + reqwest + governor + moka
crates/corx-worker/   Cloudflare Worker adapter (workers-rs, cdylib) and wrangler.jsonc
Dockerfile            multi-stage cargo-chef image (distroless runtime)
.github/              CI, release, docker and dependabot workflows
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
- Native DNS answers are filtered by `ip::is_public` before connecting (no
  TOCTOU), and the HTTP client ignores proxies. The Worker relies on the
  `global_fetch_strictly_public` compatibility flag instead.
- Caches store CORS-free responses; `Access-Control-*` headers are applied
  after the cache lookup, never stored.
- URL-valued metadata fields are never truncated — an oversized URL is
  dropped whole (`clean_url`), while text fields truncate to their cap
  (`clean_text`).
- Logs never contain the full target URL or the client IP — only the target
  host and its `Target::fingerprint` (12 hex chars of SHA-256).
- No `unwrap`/`expect`/panics in non-test code; the workspace lint set runs
  under `-D warnings`.
- `Send`/`Sync` bounds in the core go through `MaybeSend`/`MaybeSync`, which
  are real bounds on native and vacuous on `wasm32`.

## HTTP contract

`GET|HEAD|OPTIONS /v1/preview?url=…`, its JSON fields, error slugs, status
codes and `Cache-Control` values are a **public contract** consumed by
clients (Meowl). Changing any of them is a breaking change. Two tests guard
it: the exact-JSON contract test in `corx::preview` (`json_contract`) and
the status/slug/cacheability table test in `corx::error`
(`status_and_slug_table`).

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
- The Worker is verified with `worker-build --release` plus a `wrangler dev`
  smoke run.

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
