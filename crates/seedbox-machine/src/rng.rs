//! Provides an seeded random byte stream for consumption within the seedbox execution environment.
//!
//! Provides an `RngStream` trait for generating random bytes within the seedbox execution environment.
//! This trait is intended to be implemented by any random number generator used within the seedbox environment.
//! This allows the seedbox environment to produce random data in a controlled and consistent manner.

/// Trait representing a stream of random bytes.
pub trait RngStream {
    /// Fills the provided buffer with random bytes from the stream.
    fn fill_bytes(&mut self, buffer: &mut [u8]);
}

/// Extension trait for `RngStream` providing additional utility methods.
pub trait RngStreamExt: RngStream {
    fn next_u32(&mut self) -> u32 {
        let mut buffer = [0u8; 4];
        self.fill_bytes(&mut buffer);
        u32::from_le_bytes(buffer)
    }

    fn next_u64(&mut self) -> u64 {
        let mut buffer = [0u8; 8];
        self.fill_bytes(&mut buffer);
        u64::from_le_bytes(buffer)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use crate::test_support::SeededRng;
