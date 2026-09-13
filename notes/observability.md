# Observability

`corx` ships with three concentric layers of telemetry, all enabled by
default. Every layer can be turned off or filtered without recompiling.

## Logs

Structured logs go to stdout in JSON by default (`observability.log_format
= "json"`). Switch to a colourised single-line layout with `pretty`. The
`observability.log_level` field is a [`tracing-subscriber` `EnvFilter`
directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html);
override at runtime via `RUST_LOG=info,corx=debug`.

### Access log

A `corx::access` event is emitted for every completed request with the
following fields:

| Field          | Description                                                |
| -------------- | ---------------------------------------------------------- |
| `method`       | HTTP method                                                |
| `path`         | Inbound URI path                                           |
| `status`       | Final HTTP status the client saw                           |
| `duration_ms`  | Wall-clock duration including timeouts and load-shed       |
| `client_ip`    | Remote peer (validated against trusted CIDR list)          |
| `origin`       | `Origin` header                                            |
| `request_id`   | UUID v7 stamped by the proxy when missing                  |
| `error_kind`   | `corx-core` `ErrorKind` slug; empty on success             |

## Metrics (Prometheus)

Scraped from `observability.metrics_endpoint` (default `/metrics`).
Counters reset on process restart; histograms use a 1 ms..10 s bucket
layout shared across every duration metric.

| Metric                                | Type      | Labels                          |
| ------------------------------------- | --------- | ------------------------------- |
| `corx_requests_total`                 | counter   | `method`, `status`              |
| `corx_request_duration_seconds`       | histogram | `method`, `status`              |
| `corx_upstream_duration_seconds`      | histogram | `status`                        |
| `corx_upstream_errors_total`          | counter   | `kind`                          |
| `corx_inflight_requests`              | gauge     |                                 |
| `corx_bytes_transferred_total`        | counter   | `direction = request \| response` |
| `corx_rate_limited_total`             | counter   | `dimension` (`origin` \| `ip` \| `target_host` \| `global` \| `inflight`) |
| `corx_circuit_opens_total`            | counter   |                                 |
| `corx_circuit_rejects_total`          | counter   |                                 |
| `corx_ssrf_blocks_total`              | counter   | `cidr`                          |
| `corx_dns_lookups_total`              | counter   | `result = literal \| ok \| error` |
| `corx_redirect_hops`                  | histogram | `target_host`                   |

| `corx_config_reload_total`            | counter   | `result = ok \| rejected \| error` |
| `corx_build_info`                     | gauge     | `version`, `rust_version`, `features` (always reports `1`) |

`corx_build_info` is always `1`; pin a Grafana panel to it to confirm
which binary is running per pod.

## Traces (OpenTelemetry / OTLP)

Compile with `--features otel` and flip `observability.otel.enabled =
true`. The collector endpoint, wire protocol (`grpc` / `http`), service
identity, and sample ratio are all configurable:

```toml
[observability.otel]
enabled              = true
endpoint             = "http://otel-collector:4317"
protocol             = "grpc"
service_name         = "corx"
service_namespace    = "edge"
resource_attributes  = ["deployment.environment=prod"]
sample_ratio         = 0.05
```

The `tracing-opentelemetry` layer is wired into the same registry as the
log subscriber, so every log line carries the active trace / span context.

## Local stack

`docker compose up` boots Prometheus, Grafana, and an OTLP collector
side-by-side with the proxy. See [`docker-compose.yml`](../docker-compose.yml)
and the configs under `deploy/`.
