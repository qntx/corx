//! `corx-worker` — Cloudflare Worker adapter for the corx link-preview core.
//!
//! Compiles to a `cdylib` for `wasm32-unknown-unknown` only; native builds
//! see an empty, documented crate.

#[cfg(target_arch = "wasm32")]
mod imp;
