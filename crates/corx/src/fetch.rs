//! The fetch port: one GET hop, implemented by each runtime adapter.

use bytes::Bytes;
use futures_core::Stream;
use url::Url;

use crate::{Error, MaybeSend, MaybeSync};

/// Why a single upstream GET failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The destination is not reachable as a public address (DNS filtering).
    #[error("target address is blocked")]
    Blocked,
    /// The hop exceeded its deadline or was aborted.
    #[error("upstream timed out")]
    Timeout,
    /// Any other transport failure.
    #[error("upstream is unreachable")]
    Unreachable,
}

impl From<FetchError> for Error {
    fn from(err: FetchError) -> Self {
        match err {
            FetchError::Blocked => Self::BlockedTarget,
            FetchError::Timeout => Self::UpstreamTimeout,
            FetchError::Unreachable => Self::UpstreamUnreachable,
        }
    }
}

/// One `GET` against an already-validated URL.
///
/// Implementations must never follow redirects, must send `User-Agent` and
/// `Accept: ACCEPT`, must not send cookies, and must transparently decompress.
pub trait Fetch: MaybeSync {
    /// Body stream for the response.
    type Body: Stream<Item = Result<Bytes, FetchError>> + Unpin + MaybeSend;

    /// Performs the request; returns response head + body stream.
    fn get(
        &self,
        url: &Url,
    ) -> impl Future<Output = Result<::http::Response<Self::Body>, FetchError>> + MaybeSend;
}
