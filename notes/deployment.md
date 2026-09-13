# Deployment

`corx` is a single statically-linked binary. The recommended deployment
shapes, in increasing order of operator overhead, are:

1. Bare binary supervised by `systemd` / `supervisor`.
2. Docker / Podman.
3. Kubernetes via the bundled Helm chart.

## Docker

The shipped [`Dockerfile`](../Dockerfile) is a four-stage cargo-chef build
that yields a `~25 MiB` distroless image running as the `nonroot` user.

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --build-arg FEATURES="tls otel" \
  -t ghcr.io/qntx/corx:edge \
  --push .
```

`FEATURES` is space-separated; omit it for the lean HTTP-only build.

## docker-compose (local)

```bash
docker compose up -d   # corx + Prometheus + Grafana + OTLP collector
```

See [`docker-compose.yml`](../docker-compose.yml) and the corresponding
configs under `deploy/`.

## Kubernetes (Helm)

```bash
helm install corx oci://ghcr.io/qntx/charts/corx \
  --version 0.1.0 \
  --namespace edge --create-namespace \
  --set replicaCount=3 \
  --set ingress.enabled=true \
  --set 'ingress.hosts[0].host=corx.example.com' \
  --set autoscaling.enabled=true
```

The chart ships:

- ConfigMap-mounted TOML config with checksum-driven rollout
- `livenessProbe` -> `/livez`, `readinessProbe` -> `/readyz`
- HorizontalPodAutoscaler, PodDisruptionBudget, ServiceMonitor templates
- `securityContext` aligned with PodSecurityStandards `restricted`
- Hot-reload runbook in `helm install`'s NOTES.txt

## TLS

### Inbound TLS

Build with `--features tls`. Provide a PEM-encoded certificate chain and
PKCS#8 / RSA private key:

```toml
[server.tls]
cert_path = "/etc/corx/certs/tls.crt"
key_path  = "/etc/corx/certs/tls.key"
alpn_protocols = ["h2", "http/1.1"]
```

### mTLS

Build with `--features mtls`. Set `client_ca_path` to a bundle of trust
anchors used to verify client certificates. Without `client_ca_path`,
mTLS verification is disabled even when the feature is compiled in.

### FIPS 140-3

Build with `--features fips` to swap rustls's crypto provider from `ring`
to `aws-lc-rs`. Combine with `tls` / `mtls` as needed; certificates and
keys must be RSA-2048 or ECDSA P-256/P-384.

## CI integration

GitHub Actions workflows ship under `.github/workflows`:

- `ci.yml` invokes the shared `qntx/workflows/ci-rust.yml` runner for
  fmt / clippy / unit + integration tests / cargo-deny.
- `docker.yml` builds multi-arch images **only on `v*` tags** (and manual
  `workflow_dispatch`): push to GHCR, cosign sign, CycloneDX SBOM, and
  provenance attestation. PRs and ordinary `main` pushes skip Docker to
  avoid long QEMU/buildx jobs; use `docker build` locally if you need an
  image smoke test before release.
- `helm.yml` lints, renders and `kubeconform`-validates the chart when
  chart paths change; tags publish the chart to the GHCR OCI registry.

## Release engineering checklist

- [ ] Run `cargo deny check` locally (`deny.toml` codifies the policy).
- [ ] Bump `[workspace.package].version` and `appVersion` in
      `charts/corx/Chart.yaml`.
- [ ] Update [`CHANGELOG.md`](../CHANGELOG.md) under the new version.
- [ ] `git tag -a vX.Y.Z -m 'corx X.Y.Z'`; the tag triggers the Docker
      and Helm release workflows.
