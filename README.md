<!-- markdownlint-disable MD033 MD041 -->

# corx

Link-preview service in Rust, for bare-metal servers and Cloudflare Workers. Give it a URL, get the Open Graph / Twitter Card metadata needed to render a link card.

- `corx` — sans-IO core: URL admission, SSRF rules, streaming metadata extraction and the shared request pipeline (`wasm32-unknown-unknown` clean)
- `corx-server` — the `corx` binary (axum + reqwest)
- `corx-worker` — the Cloudflare Worker (workers-rs)

## Quick start

```bash
cargo run --release -p corx-server -- --allowed-origins https://app.example.com
curl 'http://localhost:8080/v1/preview?url=https%3A%2F%2Fexample.com'
```

`corx --help` lists every flag with its `CORX_*` environment variable. The Worker is configured in `crates/corx-worker/wrangler.jsonc` and deployed with wrangler (`ALLOWED_ORIGINS` must be set).

Changes are tracked in [CHANGELOG.md](CHANGELOG.md); contributor invariants and the local gate live in [AGENTS.md](AGENTS.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

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
