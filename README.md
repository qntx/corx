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

The core owns the whole request pipeline (`corx::service`): routing, CORS,
rate admission, caching, SSRF-safe fetching, HTML metadata extraction and
response rendering. The adapters only supply platform primitives — fetch,
timers, rate limiters and a cache — through `service::Runtime`, so both
runtimes behave identically.

## Quick start

Native server:

```bash
cargo run -p corx-server -- --allowed-origins https://app.example.com
curl 'http://localhost:8080/v1/preview?url=https%3A%2F%2Fexample.com'
```

Docker: `docker build -t corx . && docker run -e CORX_ALLOWED_ORIGINS='*' -p 8080:8080 corx`

Cloudflare Worker:

```bash
just worker-dev      # local dev server (wrangler 4.149.0, pinned)
just worker-deploy   # deploy; set ALLOWED_ORIGINS via --var or wrangler.jsonc
```

## Docs

- [API](docs/api.mdx) — endpoint, JSON contract, error slugs, caching and CORS
- [Security](docs/security.mdx) — SSRF model, resource limits, privacy
- [Deployment](docs/deployment.mdx) — configuration, Docker, Worker, metrics

## Development

```bash
just fmt clippy test   # fmt + strict clippy + tests
just clippy-wasm       # wasm32 clippy for core + worker
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
