//! Native worker and launcher contract integration.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use seedbox::engine::*;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn arm_contract_matches_the_worker() {
    assert_eq!(ARM_WORLD_V1, seedbox_arm64::application::world::CONTRACT);
}
