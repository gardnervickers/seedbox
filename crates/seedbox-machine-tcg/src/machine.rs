//! Backend implementation over the common machine contract.
#[cfg(unix)]
pub mod tcg;

#[cfg(test)]
mod test_environment;

#[cfg(test)]
pub(crate) const TEST_EPOCH_SECONDS: u32 = 946_684_800;

#[cfg(test)]
mod guest_service_snapshot_tests;
#[cfg(test)]
mod prepared_application_tests;
