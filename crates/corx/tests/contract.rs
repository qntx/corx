//! `contract/preview.schema.json` ⇄ types agreement: every rendered
//! response shape validates against the schema, and the schema's property
//! and slug sets equal what the types produce — drift in either direction
//! fails here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    reason = "a schema that fails to compile or a malformed fixture aborts the test"
)]

use std::collections::BTreeSet;
use std::sync::LazyLock;

use corx::http;
use corx::{Error, Image, Media, Page, Preview, Source};
use jsonschema::{Draft, Validator};
use serde_json::Value;

fn schema_json() -> &'static Value {
    static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
        serde_json::from_str(include_str!("../../../contract/preview.schema.json"))
            .expect("preview.schema.json is JSON")
    });
    &SCHEMA
}

fn validator() -> &'static Validator {
    static VALIDATOR: LazyLock<Validator> = LazyLock::new(|| {
        jsonschema::options()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(true)
            .build(schema_json())
            .expect("preview.schema.json compiles")
    });
    &VALIDATOR
}

fn assert_valid(instance: &Value) {
    let errors: Vec<String> = validator()
        .iter_errors(instance)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{instance} fails the schema: {errors:?}");
}

/// Every `Error` variant; `index`'s `match` has no wildcard, so a new
/// variant fails to compile until this list — and the schema's `error`
/// enum — is updated.
fn all_errors() -> [Error; 13] {
    const fn index(error: Error) -> usize {
        match error {
            Error::InvalidUrl => 0,
            Error::ForbiddenOrigin => 1,
            Error::BlockedTarget => 2,
            Error::NotFound => 3,
            Error::NoMetadata => 4,
            Error::MethodNotAllowed => 5,
            Error::TooLarge => 6,
            Error::UnsupportedContent => 7,
            Error::UpstreamStatus => 8,
            Error::RateLimited => 9,
            Error::UpstreamUnreachable => 10,
            Error::Overloaded => 11,
            Error::UpstreamTimeout => 12,
        }
    }
    let all = [
        Error::InvalidUrl,
        Error::ForbiddenOrigin,
        Error::BlockedTarget,
        Error::NotFound,
        Error::NoMetadata,
        Error::MethodNotAllowed,
        Error::TooLarge,
        Error::UnsupportedContent,
        Error::UpstreamStatus,
        Error::RateLimited,
        Error::UpstreamUnreachable,
        Error::Overloaded,
        Error::UpstreamTimeout,
    ];
    for (i, e) in all.iter().enumerate() {
        assert_eq!(index(*e), i, "index table must match the list order");
    }
    all
}

fn maximal_image() -> Image {
    Image {
        url: "https://example.com/i.png".to_owned(),
        width: Some(1200),
        height: Some(630),
        alt: Some("cover art".to_owned()),
    }
}

fn maximal_page() -> Preview {
    Preview::Page(Page {
        url: "https://example.com/".to_owned(),
        final_url: "https://example.com/".to_owned(),
        title: Some("Example".to_owned()),
        description: Some("An example page.".to_owned()),
        site_name: Some("Example".to_owned()),
        image: Some(maximal_image()),
        favicon: Some("https://example.com/favicon.ico".to_owned()),
        card: Some("summary_large_image".to_owned()),
        kind: Some("website".to_owned()),
        theme_color: Some("#ffffff".to_owned()),
        lang: Some("en".to_owned()),
        source: Source::Html,
        fetched_at: 1_700_000_000,
    })
}

fn minimal_page() -> Preview {
    Preview::Page(Page {
        url: "https://example.com/".to_owned(),
        final_url: "https://example.com/".to_owned(),
        title: Some("Example".to_owned()),
        description: None,
        site_name: None,
        image: None,
        favicon: None,
        card: None,
        kind: None,
        theme_color: None,
        lang: None,
        source: Source::Oembed,
        fetched_at: 1_700_000_000,
    })
}

fn maximal_media() -> Preview {
    Preview::Media(Media {
        url: "https://example.com/a.png".to_owned(),
        final_url: "https://example.com/a.png".to_owned(),
        media_type: "image/png".to_owned(),
        size: Some(4096),
        fetched_at: 1_700_000_000,
    })
}

fn minimal_media() -> Preview {
    Preview::Media(Media {
        url: "https://example.com/a.png".to_owned(),
        final_url: "https://example.com/a.png".to_owned(),
        media_type: "image/png".to_owned(),
        size: None,
        fetched_at: 1_700_000_000,
    })
}

/// Keys of a `$defs.<def>.properties` map.
fn property_keys(def: &str) -> BTreeSet<String> {
    schema_json()["$defs"][def]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("$defs.{def}.properties must exist"))
        .keys()
        .cloned()
        .collect()
}

/// Keys of a serialized object.
fn object_keys(instance: &Value) -> BTreeSet<String> {
    instance
        .as_object()
        .expect("instance is an object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn rendered_errors_validate() {
    for error in all_errors() {
        let res = http::render(&Err(error));
        let body: Value = serde_json::from_slice(res.body()).expect("rendered error body is JSON");
        assert_eq!(body["error"], error.slug(), "slug must render");
        assert_eq!(body["message"], error.message(), "message must render");
        assert_valid(&body);
    }
}

#[test]
fn slug_enum_matches() {
    let declared: BTreeSet<String> = schema_json()["$defs"]["error"]["properties"]["error"]["enum"]
        .as_array()
        .expect("error.enum is an array")
        .iter()
        .map(|v| v.as_str().expect("slugs are strings").to_owned())
        .collect();
    let actual: BTreeSet<String> = all_errors().iter().map(|e| e.slug().to_owned()).collect();
    assert_eq!(actual, declared, "schema error enum must equal Error slugs");
}

#[test]
fn preview_shapes_validate() {
    for preview in [
        maximal_page(),
        minimal_page(),
        maximal_media(),
        minimal_media(),
    ] {
        let instance = serde_json::to_value(&preview).expect("preview serializes");
        assert_valid(&instance);
    }
}

/// The schema's declared `properties` must equal the field set the types
/// serialize — a schema field the types never emit, or a type field the
/// schema forgot, both fail here.
#[test]
fn field_sets_match() {
    let page = serde_json::to_value(maximal_page()).expect("serializes");
    assert_eq!(object_keys(&page), property_keys("page"), "page fields");

    let image = serde_json::to_value(maximal_image()).expect("serializes");
    assert_eq!(object_keys(&image), property_keys("image"), "image fields");

    let media = serde_json::to_value(maximal_media()).expect("serializes");
    assert_eq!(object_keys(&media), property_keys("media"), "media fields");

    let res = http::render(&Err(Error::InvalidUrl));
    let error: Value = serde_json::from_slice(res.body()).expect("error is JSON");
    assert_eq!(object_keys(&error), property_keys("error"), "error fields");
}

/// Every vector's `expect.body` must itself satisfy the schema — a vector
/// that promises a non-contract body fails here, not at runtime.
#[test]
fn vector_expect_bodies_validate() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contract/vectors")
        .canonicalize()
        .expect("contract/vectors must exist");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(dir).expect("read vectors dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read vector");
        let vector: Value = serde_json::from_str(&text).expect("vector is JSON");
        if let Some(body) = vector.get("expect").and_then(|e| e.get("body")) {
            assert_valid(body);
            checked += 1;
        }
    }
    assert!(checked > 0, "no vector bodies were validated");
}
