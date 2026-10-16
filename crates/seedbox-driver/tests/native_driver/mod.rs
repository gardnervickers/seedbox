//! Native Driver contracts through public APIs.
#![cfg(unix)]
pub use seedbox_driver::{definition, driver, pv};
pub use seedbox_machine::clock;
mod rng {
    pub use seedbox_machine::rng::*;
    pub use seedbox_machine::test_support::SeededRng;
}
mod machine;
