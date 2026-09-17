//! Regression checks for the checked-in package/OCI fixture corpus.
//!
//! Cryptographic content-addressing and native archive parsing are performed
//! by `python3 fixtures/verify.py`; these tests keep the corpus connected to
//! the Rust test suite and assert the route-map trust-boundary invariants.

use std::collections::BTreeSet;

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/corpus/");

#[test]
fn corpus_has_all_approved_package_formats() {
    let packages = [
        (
            "packages/capturepkg-1.0.0-py3-none-any.whl",
            b"PK".as_slice(),
        ),
        ("packages/capturepkg-1.0.0.tar.gz", b"\x1f\x8b".as_slice()),
        (
            "packages/capturepkg-1.0.0-1.x86_64.rpm",
            b"\xed\xab\xee\xdb".as_slice(),
        ),
        (
            "packages/capturepkg_1.0.0-1_amd64.deb",
            b"!<arch>\n".as_slice(),
        ),
        (
            "packages/capturepkg-1.0.0-1-x86_64.pkg.tar.zst",
            b"\x28\xb5\x2f\xfd".as_slice(),
        ),
    ];

    for (relative_path, magic) in packages {
        let bytes = read(relative_path);
        assert!(bytes.starts_with(magic), "{relative_path} has wrong format");
        assert!(!bytes.is_empty(), "{relative_path} is empty");
    }
}

#[test]
fn route_map_declares_each_frontend_and_only_content_addressed_blobs() {
    let map = read_to_string("route-map.json");
    assert!(map.starts_with("{\"version\":1,\"routes\":["));
    for protocol in ["pypi", "rpm", "apt", "pacman"] {
        assert!(map.contains(&format!("\"protocol\":\"{protocol}\"")));
    }
    assert_eq!(map.matches("\"protocol\":").count(), 12);
    assert_eq!(map.matches("\"descriptor\":").count(), 12);
    assert!(!map.contains("http://") && !map.contains("https://"));
    assert!(!map.contains("repository") && !map.contains("foreign"));

    let digests = map
        .split("\"digest\":\"sha256:")
        .skip(1)
        .filter_map(|part| part.get(..64))
        .filter(|digest| digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .collect::<BTreeSet<_>>();
    assert_eq!(digests.len(), 12, "route descriptors must be distinct");
    for digest in digests {
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let path = format!("blobs/sha256/{digest}");
        assert!(!read(&path).is_empty(), "missing route blob {digest}");
    }
}

#[test]
fn manifest_is_single_layer_and_uses_fixed_publication_reference() {
    let manifest = read_to_string("manifest.json");
    assert!(manifest.contains("\"schemaVersion\":2"));
    assert!(manifest.contains("application/vnd.oci.image.manifest.v1+json"));
    assert!(manifest.contains("application/vnd.github.oras-packages.layout.v1+json"));
    assert!(manifest.contains("application/vnd.github.oras-packages.route-map.v1+json"));
    assert_eq!(manifest.matches("\"layers\":[").count(), 1);
    assert!(
        !manifest.contains("oras-packages.v1"),
        "tag belongs to index, not manifest"
    );
    assert!(read_to_string("index.json").contains("oras-packages.v1"));
}

#[test]
fn malformed_corpus_covers_required_rejection_classes() {
    for case in [
        "malformed/invalid-json.json",
        "malformed/route-map-duplicate.json",
        "malformed/route-map-foreign-descriptor.json",
        "malformed/route-map-unsupported-version.json",
        "malformed/manifest-extra-layer.json",
        "malformed/manifest-missing-config.json",
        "malformed/blob-mismatch.json",
        "malformed/bad-descriptor.json",
        "malformed/metadata-external-path.json",
        "malformed/metadata-checksum-mismatch.json",
    ] {
        assert!(!read(case).is_empty(), "negative fixture missing: {case}");
    }
}

fn read(relative_path: &str) -> Vec<u8> {
    std::fs::read(format!("{CORPUS}{relative_path}"))
        .unwrap_or_else(|error| panic!("cannot read fixture {relative_path}: {error}"))
}

fn read_to_string(relative_path: &str) -> String {
    String::from_utf8(read(relative_path)).expect("fixture JSON must be UTF-8")
}
