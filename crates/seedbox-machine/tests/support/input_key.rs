//! Distinct host input keys for tests that do not inspect them.
use seedbox_machine::driver::InputKey;
use std::sync::atomic::{AtomicU64, Ordering};

/// A key unique within this test process, so it is never live twice.
#[allow(dead_code)]
pub fn auto_key() -> InputKey {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    InputKey::new(format!("auto-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap()
}

/// A fixed key for tests that correlate events with it.
#[allow(dead_code)]
pub fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}
