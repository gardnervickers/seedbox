//! Pure closed records and structural checks for the worker-owned image.
use super::*;
use crate::machine::pv::codec::{json_len, json_write};
use seedbox_machine::snapshot::Record;
use seedbox_machine::snapshot::{CodecError, Reader, Writer};

pub(in crate::machine::pv) struct Header(Capture);
impl Header {
    pub(in crate::machine::pv) fn validate_witness(
        &self,
        expected: &protocol::BootWitness,
    ) -> std::result::Result<(), CodecError> {
        if self.0.state.witness_boot != expected.boot
            || self.0.state.witness_origin != expected.initial
        {
            return Err(CodecError::Invalid("native execution witness owner/origin"));
        }
        Ok(())
    }

    pub(in crate::machine::pv) fn validate_policy(
        &self,
        expected: Option<&protocol::BootPolicy>,
    ) -> std::result::Result<(), CodecError> {
        if self.0.state.policy.as_ref().map(|p| &p.setup) != expected {
            return Err(CodecError::Invalid(
                "native policy differs from physical owner",
            ));
        }
        if let Some(p) = &self.0.state.policy {
            p.validate()
                .map_err(|_| CodecError::Invalid("native timer cursor authority"))?;
        }
        Ok(())
    }
    pub(in crate::machine::pv) fn from_private(
        input: &mut &[u8],
    ) -> std::result::Result<Self, CodecError> {
        protocol::receive(input)
            .map_err(|_| CodecError::Invalid("private native header"))?
            .map(Self)
            .ok_or(CodecError::Invalid("missing private native header"))
    }
    pub(in crate::machine::pv) fn read<R: Read>(
        r: &mut Reader<'_, R>,
    ) -> std::result::Result<Self, CodecError> {
        Ok(Self(Capture::decode(r)?))
    }
    pub(in crate::machine::pv) fn write<W: Write>(
        &self,
        w: &mut Writer<'_, W>,
    ) -> std::result::Result<(), CodecError> {
        self.0.encode(w)
    }
    pub(in crate::machine::pv) fn validate(
        &self,
        memory: u64,
        disk: bool,
        network: Option<[u8; 6]>,
        reached: u64,
    ) -> std::result::Result<(), CodecError> {
        self.0
            .validate_snapshot(memory, disk, network, Some(reached))
            .map(|_| ())
            .map_err(|_| CodecError::Invalid("native header layout, architecture or devices"))
    }
    pub(in crate::machine::pv) fn json_len(&self) -> std::result::Result<usize, CodecError> {
        json_len(&self.0)
    }
    pub(in crate::machine::pv) fn json_write(
        &self,
        out: &mut Vec<u8>,
    ) -> std::result::Result<(), CodecError> {
        json_write(&self.0, out)
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::fixture_architecture;
#[cfg(all(
    test,
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", target_os = "macos")
    )
))]
pub(in crate::machine::pv) use tests::{backlog_without_driver_ok_fixture, fixture};
