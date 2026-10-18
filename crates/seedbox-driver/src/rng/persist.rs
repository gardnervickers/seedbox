//! Version 1 RNG state fields; no stream algorithm/domain changes.
use super::seeded::Domain;
use super::*;
use crate::persistence::{CodecError, Decoder, Encoder, Result, Wire};
use std::io::{Read, Write};
wire_enum!(Domain;units{0=>Machine,1=>ClockFaults,2=>NetworkLoss,3=>NetworkJitter,4=>NetworkReorder,5=>NetworkDuplicate,6=>NetworkBurst,7=>NetworkSpike,8=>NetworkReset}tuples{}records{});
impl Wire for SeededRng {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        let (domain, seed, block, bytes, position) = self.parts();
        domain.put(e)?;
        seed.put(e)?;
        block.put(e)?;
        bytes.put(e)?;
        position.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Self::from_parts(
            Wire::get(d)?,
            Wire::get(d)?,
            Wire::get(d)?,
            Wire::get(d)?,
            Wire::get(d)?,
        )
        .map_err(CodecError::Invalid)
    }
}
impl SeededRng {
    pub(crate) fn check_admissions_persist(
        &self,
        seeds: impl ExactSizeIterator<Item = u64>,
    ) -> Result<()> {
        self.check_admission_state(seeds)
            .map_err(CodecError::Invalid)
    }

    pub(crate) fn check_persist(&self, domain: Domain, seed: Option<u64>) -> Result<()> {
        self.check_state(domain, seed).map_err(CodecError::Invalid)
    }
}
