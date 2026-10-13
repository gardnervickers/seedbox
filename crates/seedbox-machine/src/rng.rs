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
    fn next_byte(&mut self) -> u8 {
        let mut buffer = [0u8; 1];
        self.fill_bytes(&mut buffer);
        buffer[0]
    }

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

    fn next_u128(&mut self) -> u128 {
        let mut buffer = [0u8; 16];
        self.fill_bytes(&mut buffer);
        u128::from_le_bytes(buffer)
    }

    fn next_usize(&mut self) -> usize {
        let mut buffer = [0u8; std::mem::size_of::<usize>()];
        self.fill_bytes(&mut buffer);
        usize::from_le_bytes(buffer)
    }
}

mod seeded;
pub(crate) use seeded::SeededRng;
