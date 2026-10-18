//! KVM device access checks; execution inherits the operating system scheduler.
use crate::Error;
#[cfg(target_os = "linux")]
use crate::Result;
#[cfg(target_os = "linux")]
use std::{fs::OpenOptions, path::Path};

/// KVM is a device permission, not a reason to run the entire CLI as root.
/// Called before image preparation and again by execution admission.
#[cfg(target_os = "linux")]
pub(crate) fn check_kvm_access() -> Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map(|_| ())
        .map_err(|error| kvm_access_error(error, Path::new("/etc/NIXOS").exists()))
}
fn kvm_access_error(error: std::io::Error, nixos: bool) -> Error {
    let remedy = match error.kind() {
        std::io::ErrorKind::PermissionDenied if nixos =>
            "add your user to the kvm group in your NixOS configuration (users.users.<your-user>.extraGroups = [ \"kvm\" ];), rebuild with nixos-rebuild switch, then log out and back in",
        std::io::ErrorKind::PermissionDenied =>
            "grant your user read/write access through your distribution's kvm group (usually sudo usermod -aG kvm \"$USER\"), then log out and back in; verify ls -l /dev/kvm and id",
        std::io::ErrorKind::NotFound =>
            "enable hardware virtualization and load kvm_amd or kvm_intel; in a VM, enable nested virtualization on its host",
        _ => "check /dev/kvm and your host's KVM configuration",
    };
    Error::Setup(format!("cannot open /dev/kvm: {error}; {remedy}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kvm_permission_errors_offer_platform_specific_fixes() {
        for (nixos, expected) in [(true, "extraGroups"), (false, "usermod -aG kvm")] {
            let error =
                kvm_access_error(std::io::ErrorKind::PermissionDenied.into(), nixos).to_string();
            assert!(error.contains("/dev/kvm"));
            assert!(error.contains(expected));
            assert!(error.contains("log out and back in"));
        }
        let missing = kvm_access_error(std::io::ErrorKind::NotFound.into(), false).to_string();
        assert!(missing.contains("nested virtualization"));
        assert!(!missing.contains("usermod"));
    }
}
