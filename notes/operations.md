# Operations

Runtime concerns: probes, signals, hot reload, capacity planning.

## Health probes

| Endpoint  | Status                 | Use case                            |
| --------- | ---------------------- | ----------------------------------- |
| `/livez`  | `200 OK` once started  | Kubernetes `livenessProbe`          |
| `/readyz` | `200 OK` while serving | Kubernetes `readinessProbe` / LB    |
| `/healthz`| Alias of `/readyz`     | Backwards-compat for default probes |

`/readyz` flips to `503 Service Unavailable` the moment a shutdown signal
is received, so load balancers stop routing to the pod before the listener
actually closes. In-flight requests are drained for up to
`server.graceful_shutdown` (default 30 seconds).

## Signals

| Signal      | Effect                                                            |
| ----------- | ----------------------------------------------------------------- |
| `SIGTERM`   | Drain & exit (same as Ctrl+C).                                    |
| `SIGINT`    | Drain & exit.                                                     |
| `SIGHUP`    | Reload configuration. Replaces hot-swappable policies atomically. |

Hot-reload outcomes are visible via `corx_config_reload_total{result}` and
the structured log:

```json
{"timestamp":"...","level":"INFO","target":"corx_server::hot_reload",
 "message":"configuration reloaded","path":"/etc/corx/config.toml"}
```

Attempts to change immutable fields (bind, TLS, body limits, request
timeout, metrics endpoint) are logged at `WARN` and **leave the previous
snapshot active** so the proxy never stops on a bad reload.

## Capacity planning

Three knobs to tune in production:

1. **`limits.max_request_body_bytes`** — the proxy buffers nothing, so
   this is mostly a denial-of-service guard. 10 MiB suits API gateways;
   bump to 100 MiB for upload-heavy workloads.
2. **`limits.inflight_max`** — caps concurrent requests process-wide and
   powers the load-shed layer (metric dimension `inflight`). Set
   2–3× p99 concurrency; `0` disables load-shed.
3. **`limits.max_response_body_bytes`** — streaming response size cap
   (default 50 MiB; `0` = unlimited). Guards bandwidth amplification.
4. **`upstream.pool_max_idle_per_host`** — bigger means fewer TLS
   handshakes per upstream; right-size against the connection cap of the
   backend.
5. **`rate_limit.enabled` / `rate_limit.max_keys`** — GCRA dimensions and
   keyed-map cardinality cap (fail-closed when full).

## Subcommands

```bash
corx serve   # default; run the listener
corx check   # validate config; non-zero exit on failure
corx dump    # print the resolved config (--format toml|json)
corx version # print version + os/arch + active features
```

`corx check` and `corx dump` are safe to run against a production config
file; neither binds sockets nor mutates state.

## Common runbooks

### "Origin not allowed"

Returned as `403 Forbidden` with `{"error":"origin_not_allowed",...}`.
Verify the request's `Origin` header against `security.origin_blacklist`
and `security.origin_whitelist`. CORS reflect mode also constrains
allowed origins via `cors.allowlist` when the policy kind is `reflect`.

### "Rate limited"

Returned as `429 Too Many Requests` with the `Retry-After` header. The
`x-corx-rate-limit-dimension` response header tells you which bucket
(`origin`, `ip`, `target_host`, `global`) actually fired so you know
which knob to turn.

### Reload failed but proxy still running

The previous snapshot is intact. Inspect the warning log message and run
`corx check --config <new>` locally to reproduce. Common causes are typos
in CIDR strings, illegal CORS combinations, and attempts to change
immutable fields.
