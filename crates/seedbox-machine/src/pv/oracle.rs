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
    Sessions,
}

/// Most oracle connections one guest relay serves at once (`guest_oracle.h`).
pub(crate) const MAX_SESSIONS: usize = 8;

/// One open guest connection. Its calls are sequential: at most one request
/// is outstanding, and public request IDs count from 1 within the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Session {
    pub(crate) id: NonZeroU32,
    public: NonZeroU32,
    pub(crate) pending: Option<Correlation>,
    pub(crate) reply_prepared: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Decoder {
    relay: u32,
    /// Highest session the guest has issued; issued sessions are never reused.
    session: u32,
    failed: bool,
    last_time: Option<u64>,
    /// Open sessions in ascending ID order. A closed session's replies are
    /// discarded; a session that sent nothing yet is unknown here.
    sessions: Vec<Session>,
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

    fn open(&self, session: NonZeroU32) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == session)
    }

    /// True when `session` was issued and has since closed or not yet spoken,
    /// so a reply addressed to it has lost its destination.
    fn issued(&self, session: NonZeroU32) -> bool {
        session.get() <= self.session && self.open(session).is_none()
    }

    fn observe(&mut self, observed: u64) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.last_time.is_some_and(|last| observed < last) {
            return self.fail(Error::Time);
        }
        Ok(())
    }

    /// Validate one completed SBX_RPC1 frame, without its newline. The shared
    /// management codec owns fragmentation/demultiplexing and supplies the
    /// canonical observation time of the final byte. No generic console bytes
    /// belong here. Equal timestamps preserve input order. Requests from
    /// different sessions interleave; each session stays sequential.
    pub(crate) fn receive(&mut self, observed: u64, line: &[u8]) -> Result<Request, Error> {
        self.observe(observed)?;
        if line.len() > MAX_TRANSPORT_FRAME || line.contains(&0) || line.contains(&b'\n') {
            return self.fail(Error::Frame);
        }
        let (key, index) = match self.parse(line) {
            Ok(parsed) => parsed,
            Err(error) => return self.fail(error),
        };
        let payload = line
            .splitn(4, |byte| *byte == b' ')
            .nth(3)
            .ok_or(Error::Frame)?
            .to_vec();
        self.relay = key.relay.get();
        self.last_time = Some(observed);
        let session = Session {
            id: key.session,
            public: key.request,
            pending: Some(key),
            reply_prepared: false,
        };
        match index {
            Some(i) => self.sessions[i] = session,
            None => {
                self.session = key.session.get();
                self.sessions.push(session);
            }
        }
        Ok(Request {
            correlation: key,
            observed,
            payload,
        })
    }

    fn parse(&self, line: &[u8]) -> Result<(Correlation, Option<usize>), Error> {
        let text = std::str::from_utf8(line).map_err(|_| Error::Frame)?;
        let tail = text.strip_prefix("SBX_RPC1 ").ok_or(Error::Frame)?;
        let (relay, tail) = tail.split_once(' ').ok_or(Error::Frame)?;
        let (session, payload) = tail.split_once(' ').ok_or(Error::Frame)?;
        let session = session
            .parse::<NonZeroU32>()
            .map_err(|_| Error::StaleSession)?;
        let index = self.open(session);
        if index.is_none() && session.get() <= self.session {
            return Err(Error::StaleSession);
        }
        if index.is_some_and(|i| self.sessions[i].pending.is_some()) {
            return Err(Error::Outstanding);
        }
        if index.is_none() && self.sessions.len() >= MAX_SESSIONS {
            return Err(Error::Sessions);
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
        let expected = match index {
            Some(i) => self.sessions[i].public.checked_add(1),
            None => NonZeroU32::new(1),
        };
        if Some(header.id) != expected {
            return Err(Error::PublicId);
        }
        Ok((
            Correlation {
                relay,
                session,
                request: header.id,
            },
            index,
        ))
    }

    /// Validate one SBX_CLOSE1 frame: the guest dropped a session that had
    /// sent a request. Its outstanding call, if any, keeps its host effects;
    /// only reply delivery is abandoned. Returns the closed session.
    pub(crate) fn close(&mut self, observed: u64, line: &[u8]) -> Result<NonZeroU32, Error> {
        self.observe(observed)?;
        let session = std::str::from_utf8(line)
            .ok()
            .and_then(|text| text.strip_prefix("SBX_CLOSE1 "))
            .and_then(|id| id.parse::<NonZeroU32>().ok());
        let Some(index) = session.and_then(|session| self.open(session)) else {
            return self.fail(Error::StaleSession);
        };
        self.last_time = Some(observed);
        Ok(self.sessions.remove(index).id)
    }

    /// Resolve a public reply identity without exposing the boot relay cursor.
    /// Closed sessions have lost their destination. Live mismatches remain fatal.
    pub(crate) fn resolve(
        &mut self,
        id: super::SimulationControlId,
    ) -> Result<Option<Correlation>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.issued(id.session) {
            return Ok(None);
        }
        match self.open(id.session).and_then(|i| self.sessions[i].pending) {
            Some(key) if key.request == id.request => Ok(Some(key)),
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
    /// until delivery commits, so another request on that session cannot
    /// overtake queued RX. A closed session returns None without poisoning the
    /// decoder. Bridge write failure is terminal; it must not fabricate delivery.
    pub(crate) fn reply(
        &mut self,
        key: Correlation,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.validate_reply(payload)?;
        // Closure abandons only reply delivery; returned host requests and
        // their accepted effects remain owned by their caller.
        if self.issued(key.session) && key.relay.get() <= self.relay {
            return Ok(None);
        }
        let Some(i) = self.open(key.session) else {
            return self.fail(Error::StaleReply);
        };
        if self.sessions[i].pending != Some(key) || self.sessions[i].reply_prepared {
            return self.fail(Error::StaleReply);
        }
        let mut frame = format!("SBX_REPLY1 {} {} ", key.relay, key.session).into_bytes();
        frame.extend_from_slice(payload);
        frame.push(b'\n');
        self.sessions[i].reply_prepared = true;
        Ok(Some(frame))
    }

    /// Called at committed delivery of the prepared reply to the guest transport.
    /// Correlation survives snapshots and is never inferred from completion order.
    /// False means the session closed, not that host work was cancelled.
    pub(crate) fn delivered(&mut self, key: Correlation) -> Result<bool, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.issued(key.session) && key.relay.get() <= self.relay {
            return Ok(false);
        }
        let Some(i) = self.open(key.session) else {
            return self.fail(Error::StaleReply);
        };
        if self.sessions[i].pending != Some(key) || !self.sessions[i].reply_prepared {
            return self.fail(Error::StaleReply);
        }
        self.sessions[i].pending = None;
        self.sessions[i].reply_prepared = false;
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
impl crate::snapshot::Record for Session {
    const MIN: u64 = <NonZeroU32 as crate::snapshot::Record>::MIN
        + <NonZeroU32 as crate::snapshot::Record>::MIN
        + <Option<Correlation> as crate::snapshot::Record>::MIN
        + <bool as crate::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        let Self {
            id,
            public,
            pending,
            reply_prepared,
        } = self;
        id.encode(w)?;
        public.encode(w)?;
        pending.encode(w)?;
        reply_prepared.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Ok(Self {
            id: crate::snapshot::Record::decode(r)?,
            public: crate::snapshot::Record::decode(r)?,
            pending: crate::snapshot::Record::decode(r)?,
            reply_prepared: crate::snapshot::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::snapshot::Record for Decoder {
    const MIN: u64 = <u32 as crate::snapshot::Record>::MIN
        + <u32 as crate::snapshot::Record>::MIN
        + <bool as crate::snapshot::Record>::MIN
        + <Option<u64> as crate::snapshot::Record>::MIN
        + <Vec<Session> as crate::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        let Self {
            relay,
            session,
            failed,
            last_time,
            sessions,
        } = self;
        relay.encode(w)?;
        session.encode(w)?;
        failed.encode(w)?;
        last_time.encode(w)?;
        sessions.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Ok(Self {
            relay: crate::snapshot::Record::decode(r)?,
            session: crate::snapshot::Record::decode(r)?,
            failed: crate::snapshot::Record::decode(r)?,
            last_time: crate::snapshot::Record::decode(r)?,
            sessions: crate::snapshot::Record::decode(r)?,
        })
    }
}

impl Decoder {
    pub(super) fn snapshot_time(&self) -> Option<u64> {
        self.last_time
    }
    pub(super) fn snapshot_sessions(&self) -> &[Session] {
        &self.sessions
    }
    /// True when a reply for `key` would be discarded as addressed to a
    /// closed session.
    pub(super) fn superseded(&self, key: Correlation) -> bool {
        self.issued(key.session) && key.relay.get() <= self.relay
    }
    pub(super) fn validate_snapshot(&self) -> crate::snapshot::Result<()> {
        use crate::snapshot::CodecError::Invalid;
        let Self {
            relay,
            session,
            failed,
            last_time,
            sessions,
        } = self;
        if *failed {
            return Err(Invalid("poisoned oracle"));
        }
        let zero = *relay == 0;
        if zero != (*session == 0) || zero != last_time.is_none() {
            return Err(Invalid("oracle identity tuple"));
        }
        if sessions.len() > MAX_SESSIONS
            || sessions.windows(2).any(|pair| pair[0].id >= pair[1].id)
            || sessions.iter().any(|s| {
                s.id.get() > *session
                    || (s.reply_prepared && s.pending.is_none())
                    || s.pending.is_some_and(|p| {
                        p.relay.get() > *relay || p.session != s.id || p.request != s.public
                    })
            })
        {
            return Err(Invalid("oracle session state"));
        }
        Ok(())
    }
}
