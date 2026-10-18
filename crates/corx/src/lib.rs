//! `corx` — link-preview core: validated targets, SSRF guard, a sans-IO
//! streaming HTML metadata extractor, and the request/response shaping shared
//! by the native server and the Cloudflare Worker adapters.

use core::time::Duration;

pub mod error;
mod extract;
mod fetch;
pub mod http;
pub mod ip;
mod maybe_send;
mod preview;
mod target;

pub use error::Error;
pub use fetch::{Fetch, FetchError};
pub use maybe_send::{MaybeSend, MaybeSync};
pub use preview::{Image, Preview, preview};
pub use target::Target;

/// Maximum accepted length of a target URL, in bytes.
pub const MAX_URL_LEN: usize = 2048;

/// Maximum number of HTML body bytes fed to the extractor.
pub const MAX_HTML_BYTES: usize = 1 << 20;

/// Maximum number of redirects followed per preview.
pub const MAX_REDIRECTS: usize = 5;

/// Timeout for a single upstream connection attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Wall-clock budget for the whole preview (all redirect hops).
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(4);

/// `Accept` header sent upstream.
pub const ACCEPT: &str = "text/html,application/xhtml+xml";

/// `Cache-Control` emitted for a successful preview.
pub const CACHE_CONTROL_SUCCESS: &str = "public, max-age=86400, stale-while-revalidate=604800";
