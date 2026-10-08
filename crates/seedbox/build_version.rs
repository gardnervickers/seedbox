//! Pure build provenance formatting, shared with the build-script tests.

pub fn label(
    package: &str,
    revision: Option<&str>,
    dirty: bool,
    exact_tag: Option<&str>,
) -> String {
    if let Some(revision) = revision {
        if !matches!(revision.len(), 40 | 64)
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return format!("{package}-dev+unknown");
        }
        if !dirty && exact_tag.is_some_and(|tag| tag == package || tag == format!("v{package}")) {
            return package.into();
        }
        return format!(
            "{package}-dev+{}{}",
            revision[..12].to_ascii_lowercase(),
            if dirty { ".dirty" } else { "" }
        );
    }
    // A source archive without provenance can never name a release build.
    format!("{package}-dev+unknown")
}
