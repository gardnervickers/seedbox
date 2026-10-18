//! Private shared application guest-wire codec. Management and boot-console
//! channels are distinct: only lifecycle management TX enters `receive`.
//! Framing belongs here; oracle validation sees completed SBX_RPC1 frames only.
//! Codec state, queued commands and partial frames belong to machine snapshots.

use super::{
    oracle, transport::LifecycleWrite, ApplicationCommand, CommandResult, Request, Response, Stream,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, VecDeque},
    num::NonZeroU32,
};
const MAX_LINE: usize = 16384 + 64;
const MAX_LOG: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Response {
        time: u64,
        response: Response,
    },
    Oracle(oracle::Request),
    /// Terminal supervisor setup/runtime error, separate from application exit.
    GuestError {
        time: u64,
        errno: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Failed,
    Frame,
    Time,
    ReusedId,
    Acknowledgment,
    DuplicateReady,
    DuplicateOutcome,
    Oracle(oracle::Error),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Codec {
    partial: Vec<u8>,
    pending: VecDeque<(NonZeroU32, String)>,
    used_ids: BTreeSet<NonZeroU32>,
    last_time: Option<u64>,
    ready: bool,
    outcome: bool,
    failed: bool,
    oracle: oracle::Decoder,
}

fn action(command: ApplicationCommand) -> &'static str {
    match command {
        ApplicationCommand::Start => "start",
        ApplicationCommand::Stop => "stop",
        ApplicationCommand::Pause => "freeze",
        ApplicationCommand::Resume => "thaw",
    }
}

impl Codec {
    fn fail<T>(&mut self, error: Error) -> Result<T, Error> {
        self.failed = true;
        Err(error)
    }

    /// Encode once when the machine accepts the command. Returned bytes must
    /// join the ordered lifecycle RX queue and survive snapshots until delivered.
    /// An ID remains reserved for this boot after acknowledgment. Preparing or
    /// delivering bytes does not complete the command: only a matching guest ack
    /// removes it from pending. Replace this codec when invalidating the old boot.
    pub(crate) fn request(&mut self, request: &Request) -> Result<Vec<u8>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        match request {
            Request::Application { id, command } => {
                if !self.used_ids.insert(*id) {
                    return Err(Error::ReusedId);
                }
                let action = action(*command);
                self.pending.push_back((*id, action.into()));
                Ok(format!("{action} {id}\n").into_bytes())
            }
            Request::StepRealtime { id, nanoseconds } => {
                if !self.used_ids.insert(*id) {
                    return Err(Error::ReusedId);
                }
                self.pending.push_back((*id, "step".into()));
                Ok(format!("step {id} {nanoseconds}\n").into_bytes())
            }
            Request::Crash
            | Request::Reboot
            | Request::Shutdown
            | Request::SimulationReply { .. }
            | Request::ReceiveFrame { .. } => Err(Error::Frame),
        }
    }

    /// Decode a lifecycle write in byte order; emit each completed frame at the
    /// canonical timestamp of its final byte. Multiple frames in one write keep
    /// their original order. Guest monotonic acknowledgment fields are validated
    /// but never substituted for the canonical transport timestamp. A terminal
    /// guest error preserves events observed through that error and discards
    /// the remaining bytes of this write; no later response is decoded.
    /// Subsequent writes fail because the boot is no longer usable.
    pub(crate) fn receive(&mut self, write: &LifecycleWrite) -> Result<Vec<Event>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.last_time.is_some_and(|last| write.time < last) {
            return self.fail(Error::Time);
        }
        self.last_time = Some(write.time);
        let mut events = Vec::new();
        for &byte in &write.bytes {
            if byte != b'\n' {
                if byte == 0 || self.partial.len() >= MAX_LINE {
                    return self.fail(Error::Frame);
                }
                self.partial.push(byte);
                continue;
            }
            let line = std::mem::take(&mut self.partial);
            if let Err(error) = self.frame(write.time, &line, &mut events) {
                return self.fail(error);
            }
            if self.failed {
                break;
            }
        }
        Ok(events)
    }

    /// Boot console bytes do not pass through the management parser, even if
    /// they happen to spell a management prefix. This does not mutate framing.
    pub(crate) fn console(time: u64, bytes: Vec<u8>) -> Event {
        Event::Response {
            time,
            response: Response::BootConsole { bytes },
        }
    }

    fn frame(&mut self, time: u64, line: &[u8], events: &mut Vec<Event>) -> Result<(), Error> {
        let response = if line == b"SBX_READY1" {
            if self.ready {
                return Err(Error::DuplicateReady);
            }
            self.ready = true;
            Response::Ready
        } else if let Some(log) = line.strip_prefix(b"SBX_LOG1 ") {
            if log.len() < 4 || log[1] != b' ' {
                return Err(Error::Frame);
            }
            let stream = match log[0] {
                b'1' => Stream::Stdout,
                b'2' => Stream::Stderr,
                _ => return Err(Error::Frame),
            };
            let hex = &log[2..];
            if hex.is_empty() || hex.len() % 2 != 0 || hex.len() / 2 > MAX_LOG {
                return Err(Error::Frame);
            }
            fn digit(byte: u8) -> Result<u8, Error> {
                match byte {
                    b'0'..=b'9' => Ok(byte - b'0'),
                    b'a'..=b'f' => Ok(byte - b'a' + 10),
                    _ => Err(Error::Frame),
                }
            }
            let mut bytes = Vec::with_capacity(hex.len() / 2);
            for pair in hex.as_chunks::<2>().0 {
                bytes.push(digit(pair[0])? * 16 + digit(pair[1])?);
            }
            Response::ApplicationOutput { stream, bytes }
        } else if line.starts_with(b"SBX_RPC1 ") {
            events.push(Event::Oracle(
                self.oracle.receive(time, line).map_err(Error::Oracle)?,
            ));
            return Ok(());
        } else {
            let text = std::str::from_utf8(line).map_err(|_| Error::Frame)?;
            if let Some(status) = text.strip_prefix("SBX_EXIT1 status=") {
                if self.outcome {
                    return Err(Error::DuplicateOutcome);
                }
                let code = decimal(status)?.try_into().map_err(|_| Error::Frame)?;
                self.outcome = true;
                Response::ApplicationOutcome { code }
            } else if let Some(errno) = text.strip_prefix("SBX_ERROR1 errno=") {
                let errno = decimal(errno)?.try_into().map_err(|_| Error::Frame)?;
                let mut bytes = line.to_vec();
                bytes.push(b'\n');
                events.push(Self::console(time, bytes));
                events.push(Event::GuestError { time, errno });
                // Guest errors terminate this boot; pending correlations are not
                // turned into fabricated command acknowledgments.
                self.failed = true;
                return Ok(());
            } else {
                let (result, tail) = if let Some(tail) = text.strip_prefix("SEEDBOX_EFFECT ") {
                    (CommandResult::Completed, tail)
                } else if let Some(tail) = text.strip_prefix("SEEDBOX_FAILURE ") {
                    (CommandResult::Rejected, tail)
                } else {
                    return Err(Error::Frame);
                };
                let mut fields = tail.split(' ');
                let id = NonZeroU32::new(
                    decimal(
                        fields
                            .next()
                            .and_then(|s| s.strip_prefix("id="))
                            .ok_or(Error::Frame)?,
                    )?
                    .try_into()
                    .map_err(|_| Error::Frame)?,
                )
                .ok_or(Error::Frame)?;
                let action = fields
                    .next()
                    .and_then(|s| s.strip_prefix("action="))
                    .ok_or(Error::Frame)?;
                decimal(
                    fields
                        .next()
                        .and_then(|s| s.strip_prefix("guest_ns="))
                        .ok_or(Error::Frame)?,
                )?;
                if fields.next().is_some() {
                    return Err(Error::Frame);
                }
                if self
                    .pending
                    .front()
                    .is_none_or(|(expected_id, expected_action)| {
                        *expected_id != id || expected_action != action
                    })
                {
                    return Err(Error::Acknowledgment);
                }
                self.pending.pop_front();
                // Only confirmed Start begins another application lifetime.
                // Queuing/rejected Start cannot erase duplicate-outcome checks.
                if action == "start" && result == CommandResult::Completed {
                    self.outcome = false;
                }
                Response::CommandAcknowledged { id, result }
            }
        };
        events.push(Event::Response { time, response });
        Ok(())
    }

    pub(crate) fn validate_reply(&mut self, payload: &[u8]) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.oracle.validate_reply(payload).map_err(|error| {
            self.failed = true;
            Error::Oracle(error)
        })
    }

    pub(crate) fn reject_reply(&mut self) -> Error {
        self.failed = true;
        Error::Oracle(oracle::Error::StaleReply)
    }

    pub(crate) fn prepare_reply(
        &mut self,
        id: super::SimulationControlId,
        payload: &[u8],
    ) -> Result<Option<(oracle::Correlation, Vec<u8>)>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        // Even obsolete replies must have valid payload framing.
        self.oracle.validate_reply(payload).map_err(|error| {
            self.failed = true;
            Error::Oracle(error)
        })?;
        let key = self.oracle.resolve(id).map_err(|error| {
            self.failed = true;
            Error::Oracle(error)
        })?;
        match key {
            Some(key) => Ok(self.oracle_reply(key, payload)?.map(|bytes| (key, bytes))),
            None => Ok(None),
        }
    }

    /// Encode oracle reply through the same ordered management RX channel.
    pub(crate) fn oracle_reply(
        &mut self,
        key: oracle::Correlation,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.oracle.reply(key, payload).map_err(|error| {
            self.failed = true;
            Error::Oracle(error)
        })
    }
    /// Mark actual committed delivery of the oracle reply, not mere staging.
    pub(crate) fn oracle_delivered(&mut self, key: oracle::Correlation) -> Result<bool, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.oracle.delivered(key).map_err(|error| {
            self.failed = true;
            Error::Oracle(error)
        })
    }
}

fn decimal(text: &str) -> Result<u64, Error> {
    if text.is_empty()
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err(Error::Frame);
    }
    text.parse().map_err(|_| Error::Frame)
}

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::snapshot::record::Record for Codec {
    const MIN: u64 = <Vec<u8> as crate::snapshot::record::Record>::MIN
        + <VecDeque<(NonZeroU32, String)> as crate::snapshot::record::Record>::MIN
        + <BTreeSet<NonZeroU32> as crate::snapshot::record::Record>::MIN
        + <Option<u64> as crate::snapshot::record::Record>::MIN
        + <bool as crate::snapshot::record::Record>::MIN
        + <bool as crate::snapshot::record::Record>::MIN
        + <bool as crate::snapshot::record::Record>::MIN
        + <oracle::Decoder as crate::snapshot::record::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        let Self {
            partial,
            pending,
            used_ids,
            last_time,
            ready,
            outcome,
            failed,
            oracle,
        } = self;
        partial.encode(w)?;
        pending.encode(w)?;
        used_ids.encode(w)?;
        last_time.encode(w)?;
        ready.encode(w)?;
        outcome.encode(w)?;
        failed.encode(w)?;
        oracle.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        let partial = crate::snapshot::record::Record::decode(r)?;
        let n = r.count::<(NonZeroU32, String)>(12)?;
        // Reserve the validation index's full quota before reading any pending
        // element or reserving its retained Vec/VecDeque storage.
        crate::snapshot::record::charge_index::<NonZeroU32, _>(r, n as u64)?;
        let mut pending = Vec::new();
        crate::snapshot::reserve(&mut pending, n)?;
        for _ in 0..n {
            pending.push(crate::snapshot::record::Record::decode(r)?);
        }
        Ok(Self {
            partial,
            pending: pending.into(),
            used_ids: crate::snapshot::record::Record::decode(r)?,
            last_time: crate::snapshot::record::Record::decode(r)?,
            ready: crate::snapshot::record::Record::decode(r)?,
            outcome: crate::snapshot::record::Record::decode(r)?,
            failed: crate::snapshot::record::Record::decode(r)?,
            oracle: crate::snapshot::record::Record::decode(r)?,
        })
    }
}

impl Codec {
    pub(super) fn snapshot_oracle(&self) -> &oracle::Decoder {
        &self.oracle
    }
    pub(super) fn validate_snapshot(&self, reached: u64) -> crate::snapshot::Result<()> {
        use crate::snapshot::CodecError::Invalid;
        let Self {
            partial,
            pending,
            used_ids,
            last_time,
            ready: _,
            outcome: _,
            failed,
            oracle,
        } = self;
        if *failed || partial.len() > MAX_LINE || partial.contains(&0) || partial.contains(&b'\n') {
            return Err(Invalid("poisoned or malformed lifecycle codec"));
        }
        // Input order is admission order, not numeric identity order. Sort a
        // separate fallibly allocated identity index, never the pending queue.
        // The decoder charged this copy before decoding pending elements;
        // trusted in-memory validation has no decode budget and may allocate.
        let mut identities = Vec::new();
        crate::snapshot::reserve(&mut identities, pending.len())?;
        identities.extend(pending.iter().map(|(id, _)| *id));
        identities.sort_unstable();
        if identities.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(Invalid("pending command identity or action"));
        }
        for (id, action) in pending {
            if !used_ids.contains(id)
                || !matches!(
                    action.as_str(),
                    "start" | "stop" | "freeze" | "thaw" | "step"
                )
            {
                return Err(Invalid("pending command identity or action"));
            }
        }
        oracle.validate_snapshot()?;
        let (_, _, oracle_time, _, _) = oracle.snapshot_state();
        if last_time.is_some_and(|t| t > reached)
            || oracle_time.is_some_and(|t| last_time.is_none_or(|last| t > last))
        {
            return Err(Invalid("bridge time ordering"));
        }
        Ok(())
    }
}
