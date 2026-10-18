//! Image launch semantics shared by registry, Dockerfile, and OCI-layout imports.
//! This module does not fetch images or unpack layers.

use crate::{
    config::{guest_path, Service},
    invalid, Result,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The launch-relevant portion of a resolved OCI image configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ImageDefaults {
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    #[serde(deserialize_with = "null_default")]
    pub env: Vec<String>,
    pub working_dir: String,
    pub user: String,
    #[serde(deserialize_with = "null_default")]
    pub volumes: BTreeMap<String, serde_json::Value>,
}

fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

pub use seedbox_artifacts::image::Launch;

pub fn resolve_launch(service: &Service, image: &ImageDefaults) -> Result<Launch> {
    let mut argv = service
        .entrypoint
        .as_ref()
        .or(image.entrypoint.as_ref())
        .cloned()
        .unwrap_or_default();
    // An explicit entrypoint overrides the image CMD,
    // including an explicitly empty override. A service command still applies.
    let command = service.command.as_ref().or_else(|| {
        if service.entrypoint.is_none() {
            image.cmd.as_ref()
        } else {
            None
        }
    });
    argv.extend(command.cloned().unwrap_or_default());
    if argv.first().is_none_or(String::is_empty) || argv.iter().any(|s| s.contains('\0')) {
        return invalid(
            "image.launch.argv",
            "no executable, or argument contains NUL",
        );
    }
    let mut environment = BTreeMap::new();
    for assignment in &image.env {
        let (key, value) = assignment
            .split_once('=')
            .ok_or_else(|| crate::Error::Validation {
                path: "image.config.Env".into(),
                message: "expected NAME=value".into(),
            })?;
        if key.is_empty() || assignment.contains('\0') {
            return invalid("image.config.Env", "empty environment name or NUL");
        }
        environment.insert(key.into(), value.into());
    }
    for (key, value) in &service.environment {
        match value {
            Some(value) => {
                environment.insert(key.clone(), value.clone());
            }
            None => {
                environment.remove(key);
            }
        }
    }
    let working_dir = service.working_dir.clone().unwrap_or_else(|| {
        if image.working_dir.is_empty() {
            "/".into()
        } else {
            image.working_dir.clone()
        }
    });
    guest_path(&working_dir, "image.launch.working_dir")?;
    let user = service.user.clone().unwrap_or_else(|| image.user.clone());
    if user.contains('\0') {
        return invalid("image.launch.user", "NUL is not allowed");
    }
    let volumes: Vec<_> = image.volumes.keys().cloned().collect();
    for path in &volumes {
        guest_path(path, "image.config.Volumes")?;
    }
    Ok(Launch {
        argv,
        environment,
        working_dir,
        user,
        volumes,
        disk_volumes: service.volumes.clone(),
    })
}
