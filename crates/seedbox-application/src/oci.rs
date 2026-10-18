//! Validate and retain original OCI image descriptors and launch configuration.
use crate::{
    config::Service,
    execution::PreparedImage,
    image::{resolve_launch, ImageDefaults},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
/// Linux filesystems stay on Linux; a container transport returns an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootFilesystem {
    /// An application-root directory, without a transport prefix.
    Directory(PathBuf),
    /// Transport tar with rootfs/ on entries and hard-link targets. The legacy
    /// boot assembler consumes it directly; SDK builders need the logical root.
    Archive(PathBuf),
}

pub trait ImageTransport {
    fn import(
        &self,
        service: &Service,
        directory: &Path,
        store: &ContentStore,
        context: Option<(&crate::preparation::PreparationContext, &str)>,
        architecture: crate::guest::Architecture,
    ) -> Result<ImportedImage>;
}
#[derive(Clone)]
pub struct ImportedImage {
    pub image: PreparedImage,
    pub rootfs: RootFilesystem,
    pub auxiliary: BTreeMap<String, ContentDigest>,
    pub provenance: BTreeMap<String, String>,
}

impl ImportedImage {
    /// Reuse verified image bytes, resolving this service's launch independently.
    pub(crate) fn for_service(&self, service: &Service, store: &ContentStore) -> Result<Self> {
        let config: ImageConfig = serde_json::from_slice(&store.read(&self.image.config)?)?;
        let defaults: ImageDefaults = serde_json::from_value(config.config)?;
        let mut imported = self.clone();
        imported.image.launch = resolve_launch(service, &defaults)?;
        Ok(imported)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ImageConfig {
    os: String,
    architecture: String,
    config: serde_json::Value,
}

#[derive(Deserialize)]
struct ImageTimes {
    created: Option<String>,
    #[serde(default)]
    history: Vec<HistoryTime>,
}

#[derive(Deserialize)]
struct HistoryTime {
    created: Option<String>,
}

/// The latest `created` time in an image config and its history, in Unix seconds.
/// A builder stamps these after each step runs, so anything a build step minted
/// (such as a certificate) is no later than this. None when no time is recorded.
pub fn image_created_epoch(config: &[u8]) -> Result<Option<i64>> {
    let times: ImageTimes = serde_json::from_slice(config)?;
    let mut latest = None;
    for created in times.created.iter().chain(
        times
            .history
            .iter()
            .filter_map(|entry| entry.created.as_ref()),
    ) {
        let seconds = rfc3339_seconds(created).ok_or_else(|| {
            Error::Integrity(format!(
                "image config created time {created:?} is not RFC 3339"
            ))
        })?;
        latest = latest.max(Some(seconds));
    }
    Ok(latest)
}

/// Whole Unix seconds of an RFC 3339 timestamp; fractional seconds are dropped.
fn rfc3339_seconds(text: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = text.get(range)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse().ok())?
    };
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && rest.as_bytes()[3] == b':' => {
            let hours = rest.get(1..3)?.parse::<i64>().ok()?;
            let minutes = rest.get(4..6)?.parse::<i64>().ok()?;
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    // Days from the civil date (proleptic Gregorian), after Howard Hinnant.
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146097 + day_of_era - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second - offset)
}

/// Verify a single-platform manifest and its complete compressed input closure.
/// Kept separate from transport/extraction for testability and OCI producers.
pub fn import_metadata(
    raw: &Path,
    service: &Service,
    store: &ContentStore,
) -> Result<PreparedImage> {
    import_metadata_for_arch(raw, service, store, crate::guest::Architecture::Amd64)
}

pub fn import_metadata_for_arch(
    raw: &Path,
    service: &Service,
    store: &ContentStore,
    architecture: crate::guest::Architecture,
) -> Result<PreparedImage> {
    let manifest_bytes = fs::read(raw.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    if manifest.schema_version != 2 {
        return Err(Error::Integrity(
            "expected OCI/Docker schema 2 manifest".into(),
        ));
    }
    if ![
        "application/vnd.oci.image.config.v1+json",
        "application/vnd.docker.container.image.v1+json",
    ]
    .contains(&manifest.config.media_type.as_str())
    {
        return Err(Error::Integrity(
            "unsupported image configuration media type".into(),
        ));
    }
    let config_digest = retain_descriptor(raw, &manifest.config, store)?;
    let config: ImageConfig = serde_json::from_slice(&store.read(&config_digest)?)?;
    if config.os != "linux" || config.architecture != architecture.oci_name() {
        return Err(Error::Integrity(format!(
            "image must be {}",
            architecture.platform()
        )));
    }
    for field in ["OnBuild", "Healthcheck"] {
        if let Some(value) = config.config.get(field) {
            if !value.is_null() && value.as_array().is_none_or(|v| !v.is_empty()) {
                return Err(Error::Integrity(format!(
                    "image config {field} is unsupported"
                )));
            }
        }
    }
    // ArgsEscaped only affects Windows command-line quoting; Linux argv is unchanged.
    let defaults: ImageDefaults = serde_json::from_value(config.config)?;
    let launch = resolve_launch(service, &defaults)?;
    let mut layers = Vec::new();
    for layer in &manifest.layers {
        if ![
            "application/vnd.oci.image.layer.v1.tar",
            "application/vnd.oci.image.layer.v1.tar+gzip",
            "application/vnd.oci.image.layer.v1.tar+zstd",
            "application/vnd.docker.image.rootfs.diff.tar.gzip",
        ]
        .contains(&layer.media_type.as_str())
        {
            return Err(Error::Integrity(format!(
                "unsupported layer media type {}",
                layer.media_type
            )));
        }
        layers.push(retain_descriptor(raw, layer, store)?);
    }
    Ok(PreparedImage {
        manifest: store.put(&manifest_bytes)?,
        config: config_digest,
        layers,
        launch,
    })
}

fn retain_descriptor(
    raw: &Path,
    descriptor: &Descriptor,
    store: &ContentStore,
) -> Result<ContentDigest> {
    let hex = descriptor
        .digest
        .strip_prefix("sha256:")
        .ok_or_else(|| Error::Integrity("only sha256 OCI descriptors are supported".into()))?;
    let expected = ContentDigest::try_from(hex.to_owned())?;
    let path = raw.join(expected.hex());
    if !fs::symlink_metadata(&path)?.file_type().is_file()
        || fs::metadata(&path)?.len() != descriptor.size
    {
        return Err(Error::Integrity(format!(
            "descriptor size/type mismatch: {}",
            descriptor.digest
        )));
    }
    let actual = store.put_file(&path)?;
    if actual != expected {
        return Err(Error::Integrity(format!(
            "descriptor hash mismatch: {}",
            descriptor.digest
        )));
    }
    Ok(actual)
}

#[cfg(test)]
mod created_time_tests {
    use super::*;

    #[test]
    fn rfc3339_times_become_unix_seconds() {
        for (text, seconds) in [
            ("1970-01-01T00:00:00Z", 0),
            ("1970-01-02T00:00:00Z", 86400),
            ("2026-10-09T04:05:33Z", 1_791_518_733),
            ("2026-10-09T04:05:33.987654321Z", 1_791_518_733),
            ("2026-10-09T06:05:33+02:00", 1_791_518_733),
            ("2026-10-08T23:05:33-05:00", 1_791_518_733),
            ("2000-02-29T00:00:00Z", 951_782_400),
            ("0001-01-01T00:00:00Z", -62_135_596_800),
        ] {
            assert_eq!(rfc3339_seconds(text), Some(seconds), "{text}");
        }
        for text in [
            "",
            "2026-10-09",
            "2026-10-09T04:05:33",
            "2026-13-09T04:05:33Z",
            "2026-10-09T04:05:33.Z",
            "2026-10-09T04:05:33+0200",
            "２026-10-09T04:05:33Z",
        ] {
            assert_eq!(rfc3339_seconds(text), None, "{text}");
        }
    }

    #[test]
    fn image_epoch_is_the_latest_config_or_history_time() {
        let config = br#"{"created":"2026-10-09T04:00:00Z","history":[
            {"created":"2025-01-01T00:00:00Z"},{"created":"2026-10-09T04:05:33.5Z"},{}]}"#;
        assert_eq!(image_created_epoch(config).unwrap(), Some(1_791_518_733));
        assert_eq!(image_created_epoch(b"{}").unwrap(), None);
        assert!(matches!(
            image_created_epoch(br#"{"created":"yesterday"}"#),
            Err(Error::Integrity(_))
        ));
    }
}
