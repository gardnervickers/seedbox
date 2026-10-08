#[path = "../build_version.rs"]
mod build_version;

#[test]
fn only_a_clean_matching_release_tag_names_a_release() {
    let revision = "07fc05c4aeade1f8520109365ece71666523928a";
    for tag in [None, Some("v0.0.9"), Some("v0.1.0"), Some("0.1.0")] {
        for dirty in [false, true] {
            let version = build_version::label("0.1.0", Some(revision), dirty, tag);
            if !dirty && matches!(tag, Some("v0.1.0" | "0.1.0")) {
                assert_eq!(version, "0.1.0");
            } else {
                assert_eq!(
                    version,
                    format!(
                        "0.1.0-dev+07fc05c4aead{}",
                        if dirty { ".dirty" } else { "" }
                    )
                );
            }
        }
    }
}

#[test]
fn missing_or_malformed_revision_uses_readable_unknown_version() {
    assert_eq!(
        build_version::label("0.1.0", None, false, Some("v0.1.0")),
        "0.1.0-dev+unknown"
    );
    for revision in [
        "short",
        "07fc05c\nrelease",
        "gggggggggggggggggggggggggggggggggggggggg",
    ] {
        assert_eq!(
            build_version::label("0.1.0", Some(revision), false, None),
            "0.1.0-dev+unknown"
        );
    }
}
