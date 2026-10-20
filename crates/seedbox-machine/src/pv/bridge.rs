//! Machine-owned guest service acceptance and observation. Whole-driver queues
//! remain caller-owned; only accepted pending work belongs in this snapshot.
// TODO: Re-evaluate: separate backends consume this canonical helper.
// Crate-private access cannot cross packages; representation remains private.
#[cfg(test)]
use super::transport::LifecycleWrite;
use super::{
    oracle,
    protocol::{Codec, Event},
    transport::GuestWrite,
    Output,
};
use crate::machine::{RunContext, Timestamp};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

#[derive(Clone, Copy, Serialize, Deserialize)]
struct SessionMap {
    wire: NonZeroU32,
    public: NonZeroU32,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct GuestBridge {
    codec: Codec,
    // Issued public identities belong to the machine lifetime, not a guest boot.
    session_highwater: u32,
    session: Option<SessionMap>,
    prepared_replies: Vec<oracle::Correlation>,
}

impl GuestBridge {
    /// Invalidate only the ended boot's transport. Public identities never
    /// return to the available pool, including across empty/repeated resets.
    pub fn reset(&mut self) {
        self.codec = Codec::default();
        self.session = None;
        self.prepared_replies.clear();
    }

    fn public_id(
        &mut self,
        key: oracle::Correlation,
    ) -> Result<super::SimulationControlId, String> {
        let session = match self.session {
            Some(session) if session.wire == key.session => session,
            _ => {
                let next = self
                    .session_highwater
                    .checked_add(1)
                    .and_then(NonZeroU32::new)
                    .ok_or("simulation-control session IDs exhausted")?;
                let session = SessionMap {
                    wire: key.session,
                    public: next,
                };
                self.session_highwater = next.get();
                self.session = Some(session);
                session
            }
        };
        Ok(super::SimulationControlId {
            session: session.public,
            request: key.request,
        })
    }

    pub fn request(&mut self, request: &super::Request) -> Result<Vec<u8>, String> {
        if let super::Request::SimulationReply { id, payload } = request {
            self.codec
                .validate_reply(payload)
                .map_err(|error| format!("guest reply protocol: {error:?}"))?;
            let session = match self.session {
                Some(session) if session.public == id.session => session,
                _ if id.session.get() <= self.session_highwater => return Ok(Vec::new()),
                _ => {
                    return Err(format!(
                        "guest reply protocol: {:?}",
                        self.codec.reject_reply()
                    ));
                }
            };
            let wire_id = super::SimulationControlId {
                session: session.wire,
                request: id.request,
            };
            return match self
                .codec
                .prepare_reply(wire_id, payload)
                .map_err(|error| format!("guest reply protocol: {error:?}"))?
            {
                Some((key, bytes)) => {
                    self.prepared_replies.push(key);
                    Ok(bytes)
                }
                None => Ok(Vec::new()),
            };
        }
        self.codec
            .request(request)
            .map_err(|error| format!("guest input protocol: {error:?}"))
    }
    /// Called once after the complete host RX batch transfers to the native
    /// device FIFO. Its retained cursor now owns delivery, even on a zero run.
    pub fn input_delivered(&mut self) -> Result<(), String> {
        for key in self.prepared_replies.drain(..) {
            self.codec
                .oracle_delivered(key)
                .map_err(|error| format!("guest reply delivery protocol: {error:?}"))?;
        }
        Ok(())
    }
    #[cfg(test)]
    fn observe(
        &mut self,
        context: &mut RunContext<'_, ()>,
        writes: Vec<LifecycleWrite>,
        time: u64,
        console: Vec<u8>,
    ) -> Result<(), String> {
        let mut ordered = Vec::new();
        if !console.is_empty() {
            ordered.push(GuestWrite::Console(LifecycleWrite {
                time,
                bytes: console,
            }));
        }
        ordered.extend(writes.into_iter().map(GuestWrite::Lifecycle));
        self.observe_ordered(context, ordered)
    }
    /// Process the actual cross-channel emission sequence, including ties.
    pub fn observe_ordered(
        &mut self,
        context: &mut RunContext<'_, ()>,
        writes: Vec<GuestWrite>,
    ) -> Result<(), String> {
        for write in writes {
            match write {
                GuestWrite::Console(write) => {
                    self.stage(context, Codec::console(write.time, write.bytes))?
                }
                // A NIC emits only in-bounds frames; anything else is a backend fault.
                GuestWrite::Frame(write) => {
                    if !super::frame_in_bounds(&write.bytes) {
                        return Err("guest frame outside frame bounds".into());
                    }
                    self.stage(
                        context,
                        Event::Response {
                            time: write.time,
                            response: super::Response::TransmitFrame { bytes: write.bytes },
                        },
                    )?
                }
                GuestWrite::Lifecycle(write) => {
                    let events = self
                        .codec
                        .receive(&write)
                        .map_err(|error| format!("guest output protocol: {error:?}"))?;
                    for event in events {
                        self.stage(context, event)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn stage(&mut self, context: &mut RunContext<'_, ()>, event: Event) -> Result<(), String> {
        match event {
            Event::Response { time, response } => context
                .outbox
                .emit(Output {
                    at: Timestamp::from_nanos(time),
                    payload: response,
                })
                .map_err(|_| "guest observation outside execution interval".to_owned()),
            Event::GuestError { time, errno } => {
                Err(format!("guest supervisor failed at {time}: errno {errno}"))
            }
            Event::Oracle(request) => {
                let id = self.public_id(request.correlation)?;
                context
                    .outbox
                    .emit(Output {
                        at: Timestamp::from_nanos(request.observed),
                        payload: super::Response::SimulationControl {
                            id,
                            payload: request.payload,
                        },
                    })
                    .map_err(|_| "guest observation outside execution interval".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::snapshot::Record for SessionMap {
    const MIN: u64 =
        <NonZeroU32 as crate::snapshot::Record>::MIN + <NonZeroU32 as crate::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        let Self { wire, public } = self;
        wire.encode(w)?;
        public.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Ok(Self {
            wire: crate::snapshot::Record::decode(r)?,
            public: crate::snapshot::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::snapshot::Record for GuestBridge {
    const MIN: u64 = <Codec as crate::snapshot::Record>::MIN
        + <u32 as crate::snapshot::Record>::MIN
        + <Option<SessionMap> as crate::snapshot::Record>::MIN
        + <Vec<oracle::Correlation> as crate::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        let Self {
            codec,
            session_highwater,
            session,
            prepared_replies,
        } = self;
        codec.encode(w)?;
        session_highwater.encode(w)?;
        session.encode(w)?;
        prepared_replies.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Self::read_record(r, false)
    }
}

impl GuestBridge {
    /// Encode the complete closed bridge record without consuming delivery or
    /// changing protocol cursors. The backend supplies framing and quotas.
    // TODO: Re-evaluate: concrete persistence must reach private bridge state
    // without serde collection parsing or publishing protocol fields.
    pub fn write_snapshot<W: std::io::Write>(
        &self,
        output: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        crate::snapshot::Record::encode(self, output)
    }
    /// Decode one closed bridge record under the enclosing body's quotas and
    /// validate identities, poison, correlations and time without performing I/O
    /// effects or calling request/receive/reply. With `capture_boundary`, reject
    /// a nonzero prepared-reply count before reserving or reading its elements;
    /// otherwise superseded prepared correlations are preserved. Identity-index
    /// scratch consumes the same enclosing item/allocation quotas before any
    /// allocation. Pending order is retained; validation sorts only copied IDs.
    // TODO: Re-evaluate: the backend supplies its stopped-boundary proof while
    // owner-local decoding/validation prevents exposing bridge fields.
    pub fn read_snapshot<R: std::io::Read>(
        input: &mut crate::snapshot::Reader<'_, R>,
        reached: u64,
        capture_boundary: bool,
    ) -> crate::snapshot::Result<Self> {
        let bridge = Self::read_record(input, capture_boundary)?;
        bridge.validate_snapshot(reached, capture_boundary)?;
        Ok(bridge)
    }
    /// Nonmutating owner-local structural validation. With `capture_boundary`,
    /// additionally require that all prepared replies have transferred to RX.
    /// Sorts fallibly allocated, temporary identity copies in O(n log n) without
    /// changing pending/prepared order. This trusted in-memory operation has no
    /// decode quota; read_snapshot precharges every such copy before calling it.
    /// Capture-boundary prepared ownership is checked before general validation.
    /// This is not CPU-time isolation or certification of usability after run Err.
    // TODO: Re-evaluate: the backend knows the reached time and boundary; the
    // bridge alone owns the private protocol relations that must be checked.
    pub fn validate_snapshot(
        &self,
        reached: u64,
        capture_boundary: bool,
    ) -> crate::snapshot::Result<()> {
        use crate::snapshot::CodecError::Invalid;
        let (relay, session, _, pending, prepared) = self.codec.snapshot_oracle().snapshot_state();
        if capture_boundary && (!self.prepared_replies.is_empty() || prepared) {
            return Err(Invalid("prepared reply delivery ownership"));
        }
        self.codec.validate_snapshot(reached)?;
        if self.session.is_some() != (session != 0)
            || self.session.is_some_and(|m| {
                m.wire.get() != session || m.public.get() != self.session_highwater
            })
        {
            return Err(Invalid("bridge session mapping"));
        }
        let mut identities = Vec::new();
        crate::snapshot::reserve(&mut identities, self.prepared_replies.len())?;
        identities.extend_from_slice(&self.prepared_replies);
        identities.sort_unstable_by_key(|key| (key.relay, key.session, key.request));
        if identities.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(Invalid("duplicate prepared reply"));
        }
        let mut current = false;
        for key in &self.prepared_replies {
            if key.session.get() < session && key.relay.get() < relay {
                continue;
            }
            if current || pending != Some(*key) || !prepared {
                return Err(Invalid("prepared reply correlation"));
            }
            current = true;
        }
        if current != prepared {
            return Err(Invalid("prepared reply delivery ownership"));
        }
        Ok(())
    }
}

impl GuestBridge {
    fn read_record<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
        capture_boundary: bool,
    ) -> crate::snapshot::Result<Self> {
        use crate::snapshot::{charge_index, CodecError, Record};
        let codec = Record::decode(r)?;
        let session_highwater = Record::decode(r)?;
        let session = Record::decode(r)?;
        let prepared_replies = if capture_boundary {
            if r.u64()? != 0 {
                return Err(CodecError::Invalid("prepared reply delivery ownership"));
            }
            Vec::new()
        } else {
            let n = r.count::<oracle::Correlation>(12)?;
            charge_index::<oracle::Correlation, _>(r, n as u64)?;
            let mut keys = Vec::new();
            crate::snapshot::reserve(&mut keys, n)?;
            for _ in 0..n {
                keys.push(oracle::Correlation::decode(r)?);
            }
            keys
        };
        Ok(Self {
            codec,
            session_highwater,
            session,
            prepared_replies,
        })
    }
}
