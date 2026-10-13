//! Private shared application guest-wire codec. Management and boot-console
//! channels are distinct: only lifecycle management TX enters `receive`.
//! Framing belongs here; oracle validation sees completed SBX_RPC1 frames only.
//! Codec state, queued commands and partial frames belong to machine snapshots.

use super::{
    ApplicationCommand, CommandResult, Request, Response, Stream, oracle, transport::LifecycleWrite,
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
            Request::Crash
            | Request::Reboot
            | Request::Shutdown
            | Request::SimulationReply { .. } => Err(Error::Frame),
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
            for pair in hex.chunks_exact(2) {
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
mod tests {
    use super::*;
    fn id(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }
    fn write(time: u64, bytes: &[u8]) -> LifecycleWrite {
        LifecycleWrite {
            time,
            bytes: bytes.to_vec(),
        }
    }
    fn command(c: &mut Codec, n: u32, a: ApplicationCommand) -> Vec<u8> {
        c.request(&Request::Application {
            id: id(n),
            command: a,
        })
        .unwrap()
    }
    fn response(time: u64, response: Response) -> Event {
        Event::Response { time, response }
    }
    #[test]
    fn commands_acknowledgments_and_reserved_ids() {
        let mut c = Codec::default();
        for (n, a, wire) in [
            (1, ApplicationCommand::Start, "start"),
            (2, ApplicationCommand::Stop, "stop"),
            (3, ApplicationCommand::Pause, "freeze"),
            (4, ApplicationCommand::Resume, "thaw"),
        ] {
            assert_eq!(command(&mut c, n, a), format!("{wire} {n}\n").into_bytes());
            assert_eq!(
                c.receive(&write(
                    7,
                    format!("SEEDBOX_EFFECT id={n} action={wire} guest_ns=999\n").as_bytes()
                ))
                .unwrap(),
                [response(
                    7,
                    Response::CommandAcknowledged {
                        id: id(n),
                        result: CommandResult::Completed
                    }
                )]
            );
        }
        assert_eq!(
            c.request(&Request::Application {
                id: id(1),
                command: ApplicationCommand::Start
            }),
            Err(Error::ReusedId)
        );
    }
    #[test]
    fn combined_fragmented_frames_keep_time_order_and_binary_bytes() {
        let mut c = Codec::default();
        command(&mut c, 1, ApplicationCommand::Pause);
        let first = c
            .receive(&write(5, b"SBX_READY1\nSBX_LOG1 1 00ff"))
            .unwrap();
        assert_eq!(first, [response(5, Response::Ready)]);
        let events=c.receive(&write(8,b"0a\nSBX_LOG1 2 7a00\nSEEDBOX_FAILURE id=1 action=freeze guest_ns=42\nSBX_EXIT1 status=23\n")).unwrap();
        assert_eq!(
            events,
            [
                response(
                    8,
                    Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: vec![0, 255, 10]
                    }
                ),
                response(
                    8,
                    Response::ApplicationOutput {
                        stream: Stream::Stderr,
                        bytes: vec![b'z', 0]
                    }
                ),
                response(
                    8,
                    Response::CommandAcknowledged {
                        id: id(1),
                        result: CommandResult::Rejected
                    }
                ),
                response(8, Response::ApplicationOutcome { code: 23 })
            ]
        );
    }
    #[test]
    fn snapshot_retains_partial_and_command_until_matching_ack() {
        let mut c = Codec::default();
        command(&mut c, 3, ApplicationCommand::Stop);
        assert!(
            c.receive(&write(10, b"SEEDBOX_EFFECT id=3 action=st"))
                .unwrap()
                .is_empty()
        );
        let mut c: Codec = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(c.pending.len(), 1);
        assert_eq!(
            c.receive(&write(11, b"op guest_ns=4\n")).unwrap(),
            [response(
                11,
                Response::CommandAcknowledged {
                    id: id(3),
                    result: CommandResult::Completed
                }
            )]
        );
        assert!(c.pending.is_empty());
        assert!(c.used_ids.contains(&id(3)));
    }
    #[test]
    fn mismatched_reordered_and_stale_acknowledgments_fail_closed() {
        for line in [
            b"SEEDBOX_EFFECT id=2 action=stop guest_ns=1\n".as_slice(),
            b"SEEDBOX_EFFECT id=1 action=start guest_ns=1\n",
        ] {
            let mut c = Codec::default();
            command(&mut c, 1, ApplicationCommand::Stop);
            command(&mut c, 2, ApplicationCommand::Stop);
            assert_eq!(c.receive(&write(0, line)), Err(Error::Acknowledgment));
            assert_eq!(c.pending.len(), 2);
            assert_eq!(c.receive(&write(0, b"SBX_READY1\n")), Err(Error::Failed));
        }
        let mut c = Codec::default();
        assert_eq!(
            c.receive(&write(0, b"SEEDBOX_EFFECT id=1 action=start guest_ns=1\n")),
            Err(Error::Acknowledgment)
        );
    }
    #[test]
    fn oracle_has_one_framing_owner_and_exact_reply_bytes() {
        let mut c = Codec::default();
        assert!(
            c.receive(&write(1, b"SBX_RPC1 1 1 {\"version\":1,"))
                .unwrap()
                .is_empty()
        );
        let events = c
            .receive(&write(2, b"\"id\":1,\"op\":false}\nSBX_LOG1 1 61\n"))
            .unwrap();
        let Event::Oracle(r) = &events[0] else {
            panic!()
        };
        assert_eq!(r.observed, 2);
        assert_eq!(
            events[1],
            response(
                2,
                Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes: vec![b'a']
                }
            )
        );
        assert_eq!(
            c.oracle_reply(r.correlation, b"{\"error\":\"invalid op\"}")
                .unwrap(),
            Some(b"SBX_REPLY1 1 1 {\"error\":\"invalid op\"}\n".to_vec())
        );
        c.oracle_delivered(r.correlation).unwrap();
    }
    #[test]
    fn confirmed_start_allows_next_outcome_but_rejected_start_does_not() {
        let mut c = Codec::default();
        c.receive(&write(1, b"SBX_EXIT1 status=23\n")).unwrap();
        command(&mut c, 1, ApplicationCommand::Start);
        c.receive(&write(2, b"SEEDBOX_FAILURE id=1 action=start guest_ns=2\n"))
            .unwrap();
        let mut rejected = c.clone();
        assert_eq!(
            rejected.receive(&write(3, b"SBX_EXIT1 status=23\n")),
            Err(Error::DuplicateOutcome)
        );
        command(&mut c, 2, ApplicationCommand::Start);
        c.receive(&write(
            3,
            b"SEEDBOX_EFFECT id=2 action=start guest_ns=3\nSBX_EXIT1 status=0\n",
        ))
        .unwrap();
        assert_eq!(
            c.receive(&write(4, b"SBX_EXIT1 status=0\n")),
            Err(Error::DuplicateOutcome)
        );
    }

    #[test]
    fn initial_error_is_diagnostic_not_natural_outcome_and_console_is_separate() {
        let mut c = Codec::default();
        let events = c.receive(&write(4, b"SBX_ERROR1 errno=2\n")).unwrap();
        assert_eq!(
            events,
            [
                response(
                    4,
                    Response::BootConsole {
                        bytes: b"SBX_ERROR1 errno=2\n".to_vec()
                    }
                ),
                Event::GuestError { time: 4, errno: 2 }
            ]
        );
        assert_eq!(
            Codec::console(5, b"SBX_READY1\n".to_vec()),
            response(
                5,
                Response::BootConsole {
                    bytes: b"SBX_READY1\n".to_vec()
                }
            )
        );
        assert!(!c.ready);
        assert!(!c.outcome);
    }
    #[test]
    fn terminal_error_preserves_diagnostic_and_suppresses_same_write_suffix() {
        let mut c = Codec::default();
        command(&mut c, 1, ApplicationCommand::Start);
        let events = c.receive(&write(4, b"SBX_LOG1 1 61\nSBX_ERROR1 errno=2\nSBX_READY1\nSEEDBOX_EFFECT id=1 action=start guest_ns=4\nSBX_EXIT1 status=0\nSBX_RPC1 1 1 {\"version\":1,\"id\":1}\n")).unwrap();
        assert_eq!(
            events,
            [
                response(
                    4,
                    Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: vec![b'a']
                    }
                ),
                response(
                    4,
                    Response::BootConsole {
                        bytes: b"SBX_ERROR1 errno=2\n".to_vec()
                    }
                ),
                Event::GuestError { time: 4, errno: 2 },
            ]
        );
        assert!(!c.ready);
        assert!(!c.outcome);
        assert_eq!(c.pending.len(), 1);
        assert!(c.partial.is_empty());
        assert_eq!(c.receive(&write(5, b"SBX_READY1\n")), Err(Error::Failed));
    }

    #[test]
    fn malformed_log_and_management_do_not_become_console() {
        for line in [
            b"SBX_LOG1 3 00\n".as_slice(),
            b"SBX_LOG1 1 0A\n",
            b"SBX_LOG1 1 f\n",
            b"SBX_EXIT1 status=256\n",
            b"kernel text\n",
            b"\0",
        ] {
            assert_eq!(Codec::default().receive(&write(0, line)), Err(Error::Frame));
        }
        let mut c = Codec::default();
        c.receive(&write(2, b"SBX_")).unwrap();
        assert_eq!(c.receive(&write(1, b"READY1\n")), Err(Error::Time));
    }
    #[test]
    fn superseded_reply_is_nonfatal_through_codec_and_snapshot() {
        let mut codec = Codec::default();
        let events = codec
            .receive(&write(1, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}\n"))
            .unwrap();
        let Event::Oracle(old) = &events[0] else {
            panic!()
        };
        assert!(
            codec
                .oracle_reply(old.correlation, b"{}")
                .unwrap()
                .is_some()
        );
        let events = codec
            .receive(&write(2, b"SBX_RPC1 2 2 {\"version\":1,\"id\":1}\n"))
            .unwrap();
        let Event::Oracle(new) = &events[0] else {
            panic!()
        };
        let mut restored: Codec =
            serde_json::from_slice(&serde_json::to_vec(&codec).unwrap()).unwrap();
        assert_eq!(restored.oracle_reply(old.correlation, b"{}"), Ok(None));
        assert_eq!(restored.oracle_delivered(old.correlation), Ok(false));
        assert!(
            restored
                .oracle_reply(new.correlation, b"{}")
                .unwrap()
                .is_some()
        );
        assert_eq!(restored.oracle_delivered(new.correlation), Ok(true));
        assert!(
            restored
                .receive(&write(3, b"SBX_RPC1 3 2 {\"version\":1,\"id\":2}\n"))
                .is_ok()
        );
    }
    #[test]
    fn public_reply_identity_payload_and_duplicate_acceptance_fail_closed() {
        let prepare = || {
            let mut codec = Codec::default();
            codec
                .receive(&write(1, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}\n"))
                .unwrap();
            codec
        };
        let id = |session, request| super::super::SimulationControlId {
            session: NonZeroU32::new(session).unwrap(),
            request: NonZeroU32::new(request).unwrap(),
        };
        for bad in [id(1, 2), id(2, 1)] {
            assert!(prepare().prepare_reply(bad, b"{}").is_err());
        }
        for payload in [b"".as_slice(), b"{}\n", b"invalid", b"\0"] {
            assert!(prepare().prepare_reply(id(1, 1), payload).is_err());
        }
        let mut codec = prepare();
        assert!(codec.prepare_reply(id(1, 1), b"{}").unwrap().is_some());
        assert!(codec.prepare_reply(id(1, 1), b"{}").is_err());
        let mut codec = prepare();
        codec
            .receive(&write(2, b"SBX_RPC1 2 2 {\"version\":1,\"id\":1}\n"))
            .unwrap();
        assert!(codec.prepare_reply(id(1, 1), b"{}").unwrap().is_none());
        assert!(codec.prepare_reply(id(1, 1), b"invalid").is_err());
        let mut maximum = vec![b'a'; 16383];
        maximum[0] = b'"';
        maximum[16382] = b'"';
        assert!(prepare().prepare_reply(id(1, 1), &maximum).is_ok());
        maximum.insert(1, b'a');
        assert!(prepare().prepare_reply(id(1, 1), &maximum).is_err());
    }
}
