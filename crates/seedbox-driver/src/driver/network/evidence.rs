//! Logical network authority excludes optional observation retention/draining.
use super::*;
use crate::persistence::{Encoder, Wire};
impl Network {
    pub(in crate::driver) fn policy_evidence<W: std::io::Write>(
        &self,
        e: &mut Encoder<W>,
    ) -> crate::persistence::Result<()> {
        let Self {
            config,
            members,
            pairs,
            in_flight,
            global,
            baselines,
            layers,
            connections,
            queue_counts,
            counts,
            frame_capacity: _,
            frames: _,
            omitted: _,
            cuts,
            partitions,
        } = self;
        config.put(e)?;
        members.put(e)?;
        pairs.put(e)?;
        in_flight.put(e)?;
        global.put(e)?;
        baselines.put(e)?;
        layers.put(e)?;
        connections.export_state().put(e)?;
        queue_counts.put(e)?;
        counts.put(e)?;
        cuts.put(e)?;
        partitions.put(e)
    }
}
