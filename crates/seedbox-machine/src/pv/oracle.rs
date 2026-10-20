//! Private host oracle envelope decoder for the shared guest bridge.
//!
//! The bridge supplies oracle bytes demultiplexed from lifecycle TX, stages the
//! decoded call at its committed barrier, and delivers an encoded reply through
//! lifecycle RX. The guest owns sockets, reconnects and session allocation.
//! Operations/replies remain opaque JSON; this module defines no public schema.
//! One decoder belongs to one boot and must be captured with machine-owned
//! pending work. The native guest bridge decodes requests; public host dispatch
//! is not connected yet.

use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
const MAX_FRAME: usize = 16384;
const MAX_TRANSPORT_FRAME: usize = MAX_FRAME + 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Correlation {
    pub(crate) relay: NonZeroU32,
    pub(crate) session: NonZeroU32,
    pub(crate) request: NonZeroU32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub(crate) correlation: Correlation,
    /// Canonical observation time of the terminating byte, not guest time.
    pub(crate) observed: u64,
    /// Exact JSON payload; operation validation belongs to the oracle handler.
    pub(crate) payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Failed,
    Frame,
    Identity,
    StaleSession,
    RelayId,
    PublicId,
    Outstanding,
    StaleReply,
    Time,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Decoder {
    relay: u32,
    session: u32,
    public: u32,
    failed: bool,
    last_time: Option<u64>,
    pending: Option<Correlation>,
    reply_prepared: bool,
}

#[derive(Deserialize)]
struct Header {
    version: u32,
    id: NonZeroU32,
}

impl Decoder {
    fn fail<T>(&mut self, error: Error) -> Result<T, Error> {
        self.failed = true;
        Err(error)
    }

    /// Validate one completed SBX_RPC1 frame, without its newline. The shared
    /// management codec owns fragmentation/demultiplexing and supplies the
    /// canonical observation time of the final byte. No generic console bytes
    /// belong here. Equal timestamps preserve input order.
    pub(crate) fn receive(&mut self, observed: u64, line: &[u8]) -> Result<Request, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.last_time.is_some_and(|last| observed < last) {
            return self.fail(Error::Time);
        }
        if line.len() > MAX_TRANSPORT_FRAME || line.contains(&0) || line.contains(&b'\n') {
            return self.fail(Error::Frame);
        }
        let key = match self.parse(line) {
            Ok(key) => key,
            Err(error) => return self.fail(error),
        };
        if self.pending.is_some() && key.session.get() == self.session {
            return self.fail(Error::Outstanding);
        }
        let payload = line
            .splitn(4, |byte| *byte == b' ')
            .nth(3)
            .ok_or(Error::Frame)?
            .to_vec();
        self.relay = key.relay.get();
        self.session = key.session.get();
        self.public = key.request.get();
        self.last_time = Some(observed);
        self.pending = Some(key);
        self.reply_prepared = false;
        Ok(Request {
            correlation: key,
            observed,
            payload,
        })
    }

    fn parse(&self, line: &[u8]) -> Result<Correlation, Error> {
        let text = std::str::from_utf8(line).map_err(|_| Error::Frame)?;
        let tail = text.strip_prefix("SBX_RPC1 ").ok_or(Error::Frame)?;
        let (relay, tail) = tail.split_once(' ').ok_or(Error::Frame)?;
        let (session, payload) = tail.split_once(' ').ok_or(Error::Frame)?;
        let session = session
            .parse::<NonZeroU32>()
            .map_err(|_| Error::StaleSession)?;
        if session.get() < self.session {
            return Err(Error::StaleSession);
        }
        if payload.len() + 1 > MAX_FRAME {
            return Err(Error::Frame);
        }
        let relay = relay.parse::<NonZeroU32>().map_err(|_| Error::RelayId)?;
        if Some(relay.get()) != self.relay.checked_add(1) {
            return Err(Error::RelayId);
        }
        let header: Header = serde_json::from_str(payload).map_err(|_| Error::Identity)?;
        if header.version != 1 {
            return Err(Error::Identity);
        }
        let expected = if session.get() > self.session {
            Some(1)
        } else {
            self.public.checked_add(1)
        };
        if Some(header.id.get()) != expected {
            return Err(Error::PublicId);
        }
        Ok(Correlation {
            relay,
            session,
            request: header.id,
        })
    }

    /// Resolve a public reply identity without exposing the boot relay cursor.
    /// Older sessions have lost their destination. Live mismatches remain fatal.
    pub(crate) fn resolve(
        &mut self,
        id: super::SimulationControlId,
    ) -> Result<Option<Correlation>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if id.session.get() < self.session {
            return Ok(None);
        }
        match self.pending {
            Some(key) if key.session == id.session && key.request == id.request => Ok(Some(key)),
            _ => self.fail(Error::StaleReply),
        }
    }

    pub(crate) fn validate_reply(&mut self, payload: &[u8]) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if payload.is_empty()
            || payload.len() + 1 > MAX_FRAME
            || payload.contains(&0)
            || payload.contains(&b'\n')
            || serde_json::from_slice::<serde_json::Value>(payload).is_err()
        {
            return self.fail(Error::Frame);
        }
        Ok(())
    }

    /// Encode exactly the outstanding call's reply. Keep the call outstanding
    /// until delivery commits, so another request cannot overtake queued RX.
    /// A superseded session returns None without poisoning the decoder.
    /// Bridge write failure is terminal; it must not fabricate delivery.
    pub(crate) fn reply(
        &mut self,
        key: Correlation,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.validate_reply(payload)?;
        // Supersession abandons only reply delivery; returned host requests
        // and their accepted effects remain owned by their caller.
        if key.session.get() < self.session && key.relay.get() < self.relay {
            return Ok(None);
        }
        if self.pending != Some(key) || self.reply_prepared {
            return self.fail(Error::StaleReply);
        }
        let mut frame = format!("SBX_REPLY1 {} {} ", key.relay, key.session).into_bytes();
        frame.extend_from_slice(payload);
        frame.push(b'\n');
        self.reply_prepared = true;
        Ok(Some(frame))
    }

    /// Called at committed delivery of the prepared reply to the guest transport.
    /// Correlation survives snapshots and is never inferred from completion order.
    /// False means the reply destination was superseded, not host work cancelled.
    pub(crate) fn delivered(&mut self, key: Correlation) -> Result<bool, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if key.session.get() < self.session && key.relay.get() < self.relay {
            return Ok(false);
        }
        if self.pending != Some(key) || !self.reply_prepared {
            return self.fail(Error::StaleReply);
        }
        self.pending = None;
        self.reply_prepared = false;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::snapshot::Record for Correlation {
    const MIN: u64 = <NonZeroU32 as crate::snapshot::Record>::MIN
        + <NonZeroU32 as crate::snapshot::Record>::MIN
        + <NonZeroU32 as crate::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        let Self {
            relay,
            session,
            request,
        } = self;
        relay.encode(w)?;
        session.encode(w)?;
        request.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Ok(Self {
            relay: crate::snapshot::Record::decode(r)?,
            session: crate::snapshot::Record::decode(r)?,
            request: crate::snapshot::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::snapshot::Record for Decoder {
    const MIN: u64 = <u32 as crate::snapshot::Record>::MIN
        + <u32 as crate::snapshot::Record>::MIN
        + <u32 as crate::snapshot::Record>::MIN
        + <bool as crate::snapshot::Record>::MIN
        + <Option<u64> as crate::snapshot::Record>::MIN
        + <Option<Correlation> as crate::snapshot::Record>::MIN
        + <bool as crate::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        let Self {
            relay,
            session,
            public,
            failed,
            last_time,
            pending,
            reply_prepared,
        } = self;
        relay.encode(w)?;
        session.encode(w)?;
        public.encode(w)?;
        failed.encode(w)?;
        last_time.encode(w)?;
        pending.encode(w)?;
        reply_prepared.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Ok(Self {
            relay: crate::snapshot::Record::decode(r)?,
            session: crate::snapshot::Record::decode(r)?,
            public: crate::snapshot::Record::decode(r)?,
            failed: crate::snapshot::Record::decode(r)?,
            last_time: crate::snapshot::Record::decode(r)?,
            pending: crate::snapshot::Record::decode(r)?,
            reply_prepared: crate::snapshot::Record::decode(r)?,
        })
    }
}

impl Decoder {
    pub(super) fn snapshot_state(&self) -> (u32, u32, Option<u64>, Option<Correlation>, bool) {
        (
            self.relay,
            self.session,
            self.last_time,
            self.pending,
            self.reply_prepared,
        )
    }
    pub(super) fn validate_snapshot(&self) -> crate::snapshot::Result<()> {
        use crate::snapshot::CodecError::Invalid;
        let Self {
            relay,
            session,
            public,
            failed,
            last_time,
            pending,
            reply_prepared,
        } = self;
        if *failed {
            return Err(Invalid("poisoned oracle"));
        }
        let zero = *relay == 0;
        if zero != (*session == 0) || zero != (*public == 0) || zero != last_time.is_none() {
            return Err(Invalid("oracle identity tuple"));
        }
        if pending.is_some_and(|p| {
            p.relay.get() != *relay || p.session.get() != *session || p.request.get() != *public
        }) || (*reply_prepared && pending.is_none())
        {
            return Err(Invalid("oracle pending correlation"));
        }
        Ok(())
    }
}
