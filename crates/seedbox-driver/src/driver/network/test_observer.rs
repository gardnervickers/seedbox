//! Router unit fixture with the same authority/observer separation as Driver.
use super::*;
use crate::persistence::{Decoder, Encoder, Result, Wire};
use std::{
    io::{Read, Write},
    ops::{Deref, DerefMut},
};
#[derive(Clone)]
pub(super) struct ObservedNetwork {
    network: Network,
    pub(super) observations: FrameObservations,
}
impl ObservedNetwork {
    pub(super) fn from_setup(setup: NetworkSetup) -> Self {
        let observations = FrameObservations::from_setup(&setup);
        Self {
            network: Network::from_setup(setup),
            observations,
        }
    }
    pub(super) fn route(
        &mut self,
        from: MachineId,
        sent: u64,
        bytes: &[u8],
    ) -> std::result::Result<(), RouteError> {
        self.network.route_at(
            Position {
                at: Timestamp::from_nanos(sent),
                boundary_index: 0,
                completed_quanta: 0,
            },
            from,
            sent,
            bytes,
            &mut self.observations,
        )
    }
    pub(super) fn drain_frames(&mut self) -> FrameFacts {
        self.observations.drain()
    }
}
impl Deref for ObservedNetwork {
    type Target = Network;
    fn deref(&self) -> &Network {
        &self.network
    }
}
impl DerefMut for ObservedNetwork {
    fn deref_mut(&mut self) -> &mut Network {
        &mut self.network
    }
}
impl Wire for ObservedNetwork {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.network.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Ok(Self {
            network: Network::get(d)?,
            observations: FrameObservations::default(),
        })
    }
}
