//! Black-box HTTP conformance suite for `corx`.
//!
//! The crate carries no library code: every case lives under `tests/`, is
//! `#[ignore]`d, and runs only via `run.sh`, which boots an edition (native
//! binary or `wrangler dev --local`) and points `CORX_URL` at it.
