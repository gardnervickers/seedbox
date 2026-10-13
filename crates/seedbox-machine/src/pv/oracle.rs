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
mod tests {
    use super::*;
    fn call(d: &mut Decoder, time: u64, relay: u32, session: u32, id: u32) -> Request {
        d.receive(
            time,
            format!("SBX_RPC1 {relay} {session} {{\"version\":1,\"id\":{id}}}").as_bytes(),
        )
        .unwrap()
    }
    fn complete(d: &mut Decoder, r: &Request) {
        assert_eq!(
            d.reply(r.correlation, b"{}"),
            Ok(Some(
                format!(
                    "SBX_REPLY1 {} {} {{}}\n",
                    r.correlation.relay, r.correlation.session
                )
                .into_bytes()
            ))
        );
        d.delivered(r.correlation).unwrap();
    }
    #[test]
    fn operation_errors_remain_correlated_and_pending_survives_snapshot() {
        let mut d = Decoder::default();
        let r = d
            .receive(9, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":false}")
            .unwrap();
        assert_eq!(r.payload, b"{\"version\":1,\"id\":1,\"op\":false}");
        d.reply(r.correlation, b"{\"error\":\"invalid op\"}")
            .unwrap();
        let mut d: Decoder = serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
        d.delivered(r.correlation).unwrap();
        call(&mut d, 9, 2, 1, 2);
    }
    #[test]
    fn same_time_order_reconnect_and_public_exhaustion() {
        let mut d = Decoder::default();
        let a = call(&mut d, 5, 1, 1, 1);
        complete(&mut d, &a);
        let b = call(&mut d, 5, 2, 1, 2);
        complete(&mut d, &b);
        let c = call(&mut d, 5, 3, 2, 1);
        complete(&mut d, &c);
        assert_eq!(
            d.receive(5, b"SBX_RPC1 4 1 {\"version\":1,\"id\":3}"),
            Err(Error::StaleSession)
        );
        let mut d = Decoder {
            session: 1,
            public: u32::MAX,
            ..Decoder::default()
        };
        assert_eq!(
            d.receive(0, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}"),
            Err(Error::PublicId)
        );
        let mut d = Decoder {
            session: 1,
            public: u32::MAX,
            ..Decoder::default()
        };
        call(&mut d, 0, 1, 2, 1);
    }
    #[test]
    fn all_reply_identity_fields_and_undelivered_gate() {
        for field in 0..3 {
            let mut d = Decoder::default();
            let r = call(&mut d, 0, 1, 1, 1);
            let mut key = r.correlation;
            match field {
                0 => key.session = NonZeroU32::new(2).unwrap(),
                1 => key.relay = NonZeroU32::new(2).unwrap(),
                _ => key.request = NonZeroU32::new(2).unwrap(),
            }
            assert_eq!(d.reply(key, b"{}"), Err(Error::StaleReply));
        }
        let mut d = Decoder::default();
        let a = call(&mut d, 0, 1, 1, 1);
        d.reply(a.correlation, b"{}").unwrap();
        assert_eq!(
            d.receive(0, b"SBX_RPC1 2 1 {\"version\":1,\"id\":2}"),
            Err(Error::Outstanding)
        );
    }
    #[test]
    fn malformed_identity_envelope_exhaustion_and_time() {
        for payload in [
            "{}",
            "{\"version\":1,\"id\":0}",
            "{\"version\":2,\"id\":1}",
            "{\"version\":1,\"id\":1,\"id\":1}",
        ] {
            let mut d = Decoder::default();
            assert_eq!(
                d.receive(0, format!("SBX_RPC1 1 1 {payload}").as_bytes()),
                Err(Error::Identity)
            );
        }
        let mut d = Decoder {
            relay: u32::MAX,
            ..Decoder::default()
        };
        assert_eq!(
            d.receive(0, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}"),
            Err(Error::RelayId)
        );
        let mut d = Decoder::default();
        assert_eq!(d.receive(0, b"SBX_RPC1 1 1 {}\n"), Err(Error::Frame));
        let mut d = Decoder::default();
        let a = call(&mut d, 2, 1, 1, 1);
        complete(&mut d, &a);
        assert_eq!(
            d.receive(1, b"SBX_RPC1 2 1 {\"version\":1,\"id\":2}"),
            Err(Error::Time)
        );
    }
    #[test]
    fn replacement_session_preserves_accepted_work_and_discards_old_destination() {
        for prepared in [false, true] {
            let mut decoder = Decoder::default();
            let old = call(&mut decoder, 1, 1, 1, 1);
            if prepared {
                assert!(decoder.reply(old.correlation, b"{}").unwrap().is_some());
            }
            let new = call(&mut decoder, 2, 2, 3, 1);
            // The caller's accepted old operation remains available; replacing
            // transport pending state neither rewrites nor cancels its payload.
            assert_eq!(old.payload, b"{\"version\":1,\"id\":1}");
            let mut decoder: Decoder =
                serde_json::from_slice(&serde_json::to_vec(&decoder).unwrap()).unwrap();
            assert_eq!(decoder.reply(old.correlation, b"{}"), Ok(None));
            assert_eq!(decoder.delivered(old.correlation), Ok(false));
            complete(&mut decoder, &new);
            let later = call(&mut decoder, 3, 3, 3, 2);
            complete(&mut decoder, &later);
        }
    }

    #[test]
    fn replacement_validation_remains_fail_closed() {
        for (frame, expected) in [
            (
                b"SBX_RPC1 2 2 {\"version\":1,\"id\":2}".as_slice(),
                Error::PublicId,
            ),
            (
                b"SBX_RPC1 3 2 {\"version\":1,\"id\":1}".as_slice(),
                Error::RelayId,
            ),
            (b"SBX_RPC1 2 2 {}".as_slice(), Error::Identity),
        ] {
            let mut decoder = Decoder::default();
            call(&mut decoder, 1, 1, 1, 1);
            assert_eq!(decoder.receive(2, frame), Err(expected));
        }
        let mut decoder = Decoder::default();
        let old = call(&mut decoder, 1, 1, 1, 1);
        call(&mut decoder, 2, 2, 2, 1);
        assert_eq!(
            decoder.reply(old.correlation, b"invalid JSON"),
            Err(Error::Frame)
        );
    }
}
