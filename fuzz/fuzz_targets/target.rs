#![no_main]

//! Fuzz `Target::parse` + `Target::redirect`: normalization must be a
//! fixed point (the parsed form is the cache key), a parsed target has a
//! dotted host and a 12-char fingerprint, and a followed redirect can
//! never downgrade https → http or leave the http(s) schemes.

use corx::Target;
use libfuzzer_sys::arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct Input {
    /// Candidate target URL.
    base: String,
    /// Candidate `Location` header value.
    location: String,
}

fuzz_target!(|input: Input| {
    let Ok(t) = Target::parse(&input.base) else {
        return;
    };
    // Normalization is a fixed point: the parsed form is the cache key.
    assert_eq!(
        Target::parse(t.as_str()).as_ref(),
        Ok(&t),
        "parse(as_str) round-trips to the same Target"
    );
    assert!(t.host().contains('.'), "public host is dotted");
    assert_eq!(t.fingerprint().len(), 12, "fingerprint is 12 hex chars");
    if let Ok(next) = t.redirect(&input.location) {
        let scheme = next.as_url().scheme();
        assert!(
            scheme == "http" || scheme == "https",
            "redirect stays http(s): {scheme}"
        );
        assert!(
            !(t.as_url().scheme() == "https" && scheme == "http"),
            "no https → http downgrade"
        );
        assert_eq!(
            Target::parse(next.as_str()).as_ref(),
            Ok(&next),
            "redirect hop normalizes identically"
        );
    }
});
