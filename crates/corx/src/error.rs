//! The error type: every failure maps to a stable status, slug and
//! `Cache-Control` policy.

use ::http::StatusCode;

/// Every failure a preview request can produce, mapped to a stable HTTP
/// status, machine-readable slug, and `Cache-Control` policy.
#[allow(
    clippy::error_impl_error,
    reason = "the crate's single public error type; the name is the API"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The `url` parameter was missing, malformed, or violates a syntax rule.
    InvalidUrl,
    /// The request `Origin` is not on the allowlist.
    ForbiddenOrigin,
    /// The target (or a redirect hop) resolves to a disallowed host.
    BlockedTarget,
    /// No route matched the request path.
    NotFound,
    /// The page carried no usable link metadata.
    NoMetadata,
    /// The method is not supported by this service.
    MethodNotAllowed,
    /// The upstream body exceeded `MAX_HTML_BYTES` before `</head>`.
    TooLarge,
    /// The upstream `Content-Type` was missing or unparseable.
    UnsupportedContent,
    /// A followed 3xx had a missing, non-ASCII or unparseable `Location`,
    /// or the hop limit was exceeded.
    RedirectFailed,
    /// The upstream returned a non-2xx, non-followed final status.
    UpstreamStatus(StatusCode),
    /// The caller exceeded a rate limit.
    RateLimited,
    /// The target could not be reached.
    UpstreamUnreachable,
    /// The service is shedding load.
    Overloaded,
    /// The upstream exchange exceeded its time budget.
    UpstreamTimeout,
}

impl Error {
    /// HTTP status for this error.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::InvalidUrl => StatusCode::BAD_REQUEST,
            Self::ForbiddenOrigin | Self::BlockedTarget => StatusCode::FORBIDDEN,
            Self::NotFound | Self::NoMetadata => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedContent => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::RedirectFailed | Self::UpstreamStatus(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::UpstreamUnreachable => StatusCode::BAD_GATEWAY,
            Self::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            Self::UpstreamTimeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// Stable machine-readable slug for the JSON error body.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid_url",
            Self::ForbiddenOrigin => "forbidden_origin",
            Self::BlockedTarget => "blocked_target",
            Self::NotFound => "not_found",
            Self::NoMetadata => "no_metadata",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::TooLarge => "too_large",
            Self::UnsupportedContent => "unsupported_content",
            Self::RedirectFailed => "redirect_failed",
            Self::UpstreamStatus(_) => "upstream_status",
            Self::RateLimited => "rate_limited",
            Self::UpstreamUnreachable => "upstream_unreachable",
            Self::Overloaded => "overloaded",
            Self::UpstreamTimeout => "upstream_timeout",
        }
    }

    /// Short English sentence for the JSON error body.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidUrl => "the url parameter is missing, malformed, or unsupported",
            Self::ForbiddenOrigin => "the request origin is not allowed",
            Self::BlockedTarget => "the target host is not allowed",
            Self::NotFound => "not found",
            Self::NoMetadata => "the page contains no usable link metadata",
            Self::MethodNotAllowed => "method not allowed",
            Self::TooLarge => "the response exceeded the size limit",
            Self::UnsupportedContent => "the response content type is missing or unsupported",
            Self::RedirectFailed => {
                "the upstream redirect is missing, malformed, or exceeds the hop limit"
            }
            Self::UpstreamStatus(_) => "the upstream server returned an unexpected status",
            Self::RateLimited => "rate limit exceeded",
            Self::UpstreamUnreachable => "the target could not be reached",
            Self::Overloaded => "the server is overloaded",
            Self::UpstreamTimeout => "the upstream server timed out",
        }
    }

    /// `Cache-Control` value for this error.
    #[must_use]
    pub const fn cache_control(self) -> &'static str {
        if self.is_cacheable() {
            "public, max-age=3600"
        } else {
            "no-store"
        }
    }

    /// The upstream's non-2xx status, when this error carries one.
    #[must_use]
    pub const fn upstream_status(self) -> Option<StatusCode> {
        match self {
            Self::UpstreamStatus(status) => Some(status),
            _ => None,
        }
    }

    /// Whether a negative result is safe to store in a shared cache.
    #[must_use]
    pub const fn is_cacheable(self) -> bool {
        matches!(
            self,
            Self::NoMetadata | Self::RedirectFailed | Self::UpstreamStatus(_)
        )
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

impl core::error::Error for Error {}

#[cfg(test)]
mod tests {
    use http::StatusCode;

    use super::Error;

    #[test]
    fn status_and_slug_table() {
        let cases = [
            (Error::InvalidUrl, 400, "invalid_url", false),
            (Error::ForbiddenOrigin, 403, "forbidden_origin", false),
            (Error::BlockedTarget, 403, "blocked_target", false),
            (Error::NotFound, 404, "not_found", false),
            (Error::NoMetadata, 404, "no_metadata", true),
            (Error::MethodNotAllowed, 405, "method_not_allowed", false),
            (Error::TooLarge, 413, "too_large", false),
            (Error::UnsupportedContent, 415, "unsupported_content", false),
            (Error::RedirectFailed, 422, "redirect_failed", true),
            (
                Error::UpstreamStatus(StatusCode::FORBIDDEN),
                422,
                "upstream_status",
                true,
            ),
            (Error::RateLimited, 429, "rate_limited", false),
            (
                Error::UpstreamUnreachable,
                502,
                "upstream_unreachable",
                false,
            ),
            (Error::Overloaded, 503, "overloaded", false),
            (Error::UpstreamTimeout, 504, "upstream_timeout", false),
        ];
        for (err, status, slug, cacheable) in cases {
            assert_eq!(err.status().as_u16(), status, "{err}");
            assert_eq!(err.slug(), slug, "{err}");
            assert_eq!(err.is_cacheable(), cacheable, "{err}");
            assert!(!err.message().is_empty(), "{err}");
            assert_eq!(
                err.cache_control(),
                if cacheable {
                    "public, max-age=3600"
                } else {
                    "no-store"
                },
                "{err}"
            );
        }
    }
}
