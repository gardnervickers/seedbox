use super::*;
use crate::persistence::{require, Decoder, Encoder, Result, Wire};
use std::io::{Read, Write};
wire_enum!(PlaybackRate;units{0=>Paused,2=>Unlimited}tuples{}records{1=>Limited{numerator,denominator}});
impl Wire for PlaybackIntent {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.rate.put(e)?;
        self.step_after.put(e)?;
        self.before_next.put(e)?;
        self.held.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Ok(Self {
            rate: Wire::get(d)?,
            step_after: Wire::get(d)?,
            before_next: Wire::get(d)?,
            held: Wire::get(d)?,
        })
    }
}
impl PlaybackIntent {
    pub(crate) fn check_persist(&self, quanta: u64) -> Result<()> {
        require(
            self.step_after
                .is_none_or(|q| q <= quanta && self.rate == PlaybackRate::Paused),
            "playback step target",
        )
    }
}
