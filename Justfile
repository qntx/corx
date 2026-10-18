# corx — link-preview service

default:
    @just --list

# Format all code.
fmt:
    cargo fmt --all

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

# wasm32 check for the core and worker crates.
check-wasm:
    rustup target add wasm32-unknown-unknown
    cargo check -p corx -p corx-worker --target wasm32-unknown-unknown

# Run the Worker locally (installs wrangler + worker-build via npm/cargo).
worker-dev:
    cd crates/corx-worker && npx wrangler@4.149.0 dev --var ALLOWED_ORIGINS:http://localhost:5173

# Deploy the Worker to Cloudflare.
worker-deploy:
    cd crates/corx-worker && npx wrangler@4.149.0 deploy
