//! Backend implementation over the common machine contract.
#[cfg(unix)]
pub mod tcg;

#[cfg(test)]
mod test_environment;

#[cfg(test)]
pub(crate) use seedbox_machine::test_support::TEST_EPOCH_SECONDS;

#[cfg(test)]
mod guest_service_snapshot_tests;
#[cfg(test)]
mod prepared_application_tests;
