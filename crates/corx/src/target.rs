//! Validated target URLs: the unit the preview pipeline operates on.

use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::{Error, MAX_URL_LEN};

/// Reserved domain suffixes that can never be previewed.
const BLOCKED_SUFFIXES: &[&str] = &[".localhost", ".local", ".internal", ".arpa", ".onion"];

/// A URL that passed every admission rule: http(s) only, no credentials,
/// no explicit port, a dotted public domain (never an IP literal), no
/// reserved suffixes, fragment stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target(Url);

impl Target {
    /// Parses and validates `raw` as a preview target.
    ///
    /// # Errors
    /// `Error::InvalidUrl` for malformed or syntactically disallowed input,
    /// `Error::BlockedTarget` for well-formed URLs pointing at private or
    /// reserved destinations.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        if raw.len() > MAX_URL_LEN {
            return Err(Error::InvalidUrl);
        }
        let mut url = Url::parse(raw).map_err(|_| Error::InvalidUrl)?;
        url.set_fragment(None);
        Self::validate(url)
    }

    /// Resolves a redirect `Location` value against this target and applies
    /// the full validation to the result. HTTPS-to-HTTP downgrades are
    /// rejected.
    ///
    /// # Errors
    /// `Error::UpstreamStatus` when `location` is unparseable; otherwise the
    /// same errors as [`Target::parse`].
    pub fn redirect(&self, location: &str) -> Result<Self, Error> {
        let mut joined = self.0.join(location).map_err(|_| Error::UpstreamStatus)?;
        if self.0.scheme() == "https" && joined.scheme() == "http" {
            return Err(Error::BlockedTarget);
        }
        joined.set_fragment(None);
        Self::validate(joined)
    }

    fn validate(url: Url) -> Result<Self, Error> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::InvalidUrl);
        }
        if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
            return Err(Error::InvalidUrl);
        }
        let host = match url.host() {
            Some(Host::Domain(domain)) => domain.strip_suffix('.').unwrap_or(domain),
            Some(Host::Ipv4(_) | Host::Ipv6(_)) => return Err(Error::BlockedTarget),
            None => return Err(Error::InvalidUrl),
        };
        if host == "localhost"
            || !host.contains('.')
            || BLOCKED_SUFFIXES.iter().any(|s| host.ends_with(s))
        {
            return Err(Error::BlockedTarget);
        }
        Ok(Self(url))
    }

    /// The normalized URL.
    #[must_use]
    pub const fn as_url(&self) -> &Url {
        &self.0
    }

    /// The normalized URL string — also the cache key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The registered host (lowercased, one trailing dot stripped).
    #[must_use]
    pub fn host(&self) -> &str {
        let host = self.0.host_str().unwrap_or_default();
        host.strip_suffix('.').unwrap_or(host)
    }

    /// First 6 bytes of the SHA-256 of the normalized URL as lowercase hex —
    /// a privacy-preserving identifier for logs.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.as_str().as_bytes());
        digest
            .iter()
            .take(6)
            .flat_map(|b| [b >> 4, b & 0x0f])
            .map(|n| char::from_digit(u32::from(n), 16).unwrap_or('0'))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Target, Error> {
        Target::parse(raw)
    }

    #[test]
    fn accepts_normal_urls() {
        let t = parse("https://example.com/path?a=b#frag").expect("valid");
        assert_eq!(t.as_str(), "https://example.com/path?a=b");
        assert_eq!(t.host(), "example.com");
    }

    #[test]
    fn rejects_too_long() {
        // "https://example.com/" is 20 bytes: pad to exactly 2048 and 2049.
        let raw = format!("https://example.com/{}", "a".repeat(2028));
        assert!(parse(&raw).is_ok(), "2048-byte URL is accepted");
        let raw2 = format!("https://example.com/{}", "a".repeat(2029));
        assert_eq!(
            parse(&raw2),
            Err(Error::InvalidUrl),
            "2049-byte URL rejected"
        );
    }

    #[test]
    fn rejects_bad_scheme_and_malformed() {
        for raw in [
            "ftp://example.com",
            "file:///etc/passwd",
            "not a url",
            "http://",
            "://x",
        ] {
            assert_eq!(parse(raw), Err(Error::InvalidUrl), "{raw}");
        }
    }

    #[test]
    fn rejects_userinfo_and_ports() {
        for raw in [
            "http://user@example.com/",
            "http://user:pw@example.com/",
            "http://:pw@example.com/",
            "http://example.com:8080/",
            "https://example.com:4443/",
        ] {
            assert_eq!(parse(raw), Err(Error::InvalidUrl), "{raw}");
        }
        // Explicit default ports normalize away and are accepted.
        assert_eq!(
            parse("http://example.com:80/").expect("ok").as_str(),
            "http://example.com/"
        );
        assert_eq!(
            parse("https://example.com:443/").expect("ok").as_str(),
            "https://example.com/"
        );
    }

    #[test]
    fn rejects_ip_literals_in_all_forms() {
        for raw in [
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://8.8.8.8/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[2606:4700:4700::1111]/",
        ] {
            assert_eq!(parse(raw), Err(Error::BlockedTarget), "{raw}");
        }
    }

    #[test]
    fn rejects_private_domains() {
        for raw in [
            "http://localhost/",
            "http://foo.localhost/",
            "http://printer.local/",
            "http://svc.internal/",
            "http://1.0.0.127.in-addr.arpa/",
            "http://example.onion/",
            "http://com/",
            "http://intranet/",
        ] {
            assert_eq!(parse(raw), Err(Error::BlockedTarget), "{raw}");
        }
    }

    #[test]
    fn trailing_dot_and_case() {
        let t = parse("https://EXAMPLE.COM./x").expect("valid");
        assert_eq!(t.as_str(), "https://example.com./x");
        assert_eq!(t.host(), "example.com");
        assert_eq!(parse("http://localhost./"), Err(Error::BlockedTarget));
        assert_eq!(parse("http://foo.localhost./"), Err(Error::BlockedTarget));
    }

    #[test]
    fn redirect_rules() {
        let base = parse("https://a.example/page").expect("base");
        // Relative join.
        assert_eq!(
            base.redirect("../next").expect("ok").as_str(),
            "https://a.example/next"
        );
        // Absolute join to another public host.
        assert_eq!(
            base.redirect("https://b.example/x").expect("ok").as_str(),
            "https://b.example/x"
        );
        // Downgrade blocked, upgrade allowed.
        assert_eq!(
            base.redirect("http://a.example/x"),
            Err(Error::BlockedTarget)
        );
        let http = parse("http://a.example/").expect("base");
        assert_eq!(
            http.redirect("https://a.example/").expect("ok").as_str(),
            "https://a.example/"
        );
        // Redirect to IP literal or private suffix still blocked.
        assert_eq!(
            http.redirect("http://127.0.0.1/"),
            Err(Error::BlockedTarget)
        );
        assert_eq!(http.redirect("//localhost/x"), Err(Error::BlockedTarget));
        // Unparseable Location is an upstream problem.
        assert_eq!(http.redirect("http://[bad"), Err(Error::UpstreamStatus));
        // Redirect to a non-http scheme is just an invalid target.
        assert_eq!(http.redirect("ftp://a.example/x"), Err(Error::InvalidUrl));
        // Fragment on the location is stripped.
        assert_eq!(
            http.redirect("/x#y").expect("ok").as_str(),
            "http://a.example/x"
        );
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        let a = parse("https://example.com/").expect("ok");
        let b = parse("https://example.com/#x").expect("ok");
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint().len(), 12);
        assert!(a.fingerprint().chars().all(|c| c.is_ascii_hexdigit()));
    }
}
