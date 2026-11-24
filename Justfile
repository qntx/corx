# corx — link-preview service

default:
    @just --list

# Format all code (Rust + TOML).
fmt:
    cargo fmt --all
    npx --yes @taplo/cli@0.7.0 fmt

# Run the full local gate (same checks as CI).
lint:
    npx --yes @taplo/cli@0.7.0 fmt --check
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo clippy -p corx -p corx-worker --target wasm32-unknown-unknown -- -D warnings
    cargo test --workspace --all-features
    RUSTDOCFLAGS="-D warnings" cargo doc -p corx --no-deps
    cargo deny check

# Strict clippy on the native workspace.
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Strict clippy for the wasm crates.
clippy-wasm:
    rustup target add wasm32-unknown-unknown
    cargo clippy -p corx -p corx-worker --target wasm32-unknown-unknown --all-features -- -D warnings

# Run the test suite.
test:
    cargo test --workspace --all-features

# Run the black-box conformance suite (edition: native|worker|remote).
conformance edition:
    crates/corx-conformance/run.sh {{ edition }}

# Run the URL corpus against a deployment (e.g. `just corpus http://127.0.0.1:8787`).
corpus base-url:
    cargo run --release -p corx-corpus -- run {{ base-url }}

# Sweep upstream User-Agents through the Worker (see corpus/ua-sweep.sh).
corpus-ua *uas:
    corpus/ua-sweep.sh {{ uas }}

# Run the Worker locally (installs wrangler + worker-build via npm/cargo).
worker-dev:
    cd crates/corx-worker && npx wrangler@4.164.0 dev --var ALLOWED_ORIGINS:http://localhost:5173

# Deploy the Worker to Cloudflare.
worker-deploy:
    cd crates/corx-worker && npx wrangler@4.164.0 deploy
