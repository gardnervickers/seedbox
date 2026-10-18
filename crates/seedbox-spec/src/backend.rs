//! CPU execution selection and retained runtime metadata.

use serde::{Deserialize, Serialize};

/// CPU selection supplied by the execution client, never a project/YAML knob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuBackend {
    #[default]
    Pv,
    Tcg,
}
impl CpuBackend {
    /// Decode the single recorded backend name. Non-executing test profiles have no CPU backend.
    pub fn from_recorded_name(backend: &str) -> Result<Option<Self>, String> {
        match backend {
            "driver-pv" => Ok(Some(Self::Pv)),
            "driver-tcg" => Ok(Some(Self::Tcg)),
            name if name.starts_with("driver-") => Err(format!("unknown Driver backend {name:?}")),
            _ => Ok(None),
        }
    }

    pub fn is_pv(&self) -> bool {
        *self == Self::Pv
    }
}

#[cfg(test)]
mod backend_authority_tests {
    use super::CpuBackend;
    #[test]
    fn backend_authority_decodes_one_recorded_name() {
        assert_eq!(
            CpuBackend::from_recorded_name("driver-pv").unwrap(),
            Some(CpuBackend::Pv)
        );
        assert_eq!(
            CpuBackend::from_recorded_name("driver-tcg").unwrap(),
            Some(CpuBackend::Tcg)
        );
        assert!(CpuBackend::from_recorded_name("driver-unknown")
            .unwrap_err()
            .contains("unknown Driver backend"));
        assert_eq!(CpuBackend::from_recorded_name("fixture").unwrap(), None);
    }
}
