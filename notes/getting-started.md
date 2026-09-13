# Getting started

`corx` is a high-performance CORS forwarding proxy. Drop it in front of any
HTTP API to add browser-friendly CORS headers, multi-dimensional rate
limiting, and SSRF protection without touching the upstream.

## Install

### From source

```bash
cargo install --path crates/corx
```

### Docker

```bash
docker pull ghcr.io/qntx/corx:latest
docker run --rm -p 8080:8080 ghcr.io/qntx/corx:latest
```

The default image runs as the distroless `nonroot` user and exposes port
`8080`.

### Helm

```bash
helm install corx oci://ghcr.io/qntx/charts/corx \
  --version 0.1.0 \
  --namespace edge --create-namespace
```

## Verify

```bash
curl -i http://localhost:8080/livez
# HTTP/1.1 200 OK
# live

curl -s "http://localhost:8080/https://api.github.com/zen"
# Mind your words, they are important.
```

The path after the leading `/` is the upstream URL. Both fully-qualified
URLs (`/https://target/path`) and bare hosts (`/target.example.com/path`)
work; the latter defaults to HTTPS when the port is `443`.

## Common operator commands

```bash
corx version                # -> "corx 0.2.0 (linux/x86_64) features=tls,otel"
corx check                  # validate the active configuration
corx dump --format toml     # print the fully-resolved configuration
corx serve --config /etc/corx/config.toml   # explicit serve
```

`corx` (no subcommand) is identical to `corx serve` for backwards
compatibility with `cargo install` workflows.

## Next steps

- Tune the proxy via [`docs/configuration.md`](configuration.md).
- Lock down SSRF and CORS via [`docs/security.md`](security.md).
- Wire metrics and traces via [`docs/observability.md`](observability.md).
