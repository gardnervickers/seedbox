//! Backend contracts exercised through public APIs.
#![cfg(unix)]
use seedbox_machine_tcg::machine::tcg;
const TEST_EPOCH_SECONDS: u32 = 946_684_800;
mod kernel_terminal_tests;
mod network_device_tests;
mod network_tests;
