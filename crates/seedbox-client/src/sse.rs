//! Incremental, bounded decoding of SSE data fields. Delivery chunk boundaries
//! have no relationship to event, line or UTF-8 boundaries.

use crate::{Error, RESPONSE_LIMIT};
mod activity;

use activity::{Clock, TokioClock};
use futures_util::{Stream, StreamExt};
use seedbox_api::{EventDelivery, Phase, Simulation};
use serde::de::DeserializeOwned;
use std::{collections::VecDeque, pin::Pin};

pub type CheckpointExportStream =
    Pin<Box<dyn Stream<Item = Result<seedbox_api::CheckpointExport, Error>> + Send>>;
pub type SimulationStream = Pin<Box<dyn Stream<Item = Result<Simulation, Error>> + Send>>;
pub type EventStream = Pin<Box<dyn Stream<Item = Result<EventDelivery, Error>> + Send>>;
pub type FrameStream =
    Pin<Box<dyn Stream<Item = Result<seedbox_api::FrameDelivery, Error>> + Send>>;
pub type LogStream = Pin<Box<dyn Stream<Item = Result<seedbox_api::LogDelivery, Error>> + Send>>;

pub fn logs(response: reqwest::Response) -> LogStream {
    decode(response, |delivery: &seedbox_api::LogDelivery| {
        matches!(delivery, seedbox_api::LogDelivery::Closed)
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Data(Vec<u8>),
    Keepalive,
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
    has_data: bool,
}

impl Decoder {
    fn push(&mut self, byte: u8) -> Result<Option<Frame>, Error> {
        if self.after_cr && byte == b'\n' {
            self.after_cr = false;
            return Ok(None);
        }
        self.after_cr = byte == b'\r';
        if matches!(byte, b'\r' | b'\n') {
            let line = std::mem::take(&mut self.line);
            if line.is_empty() && self.has_data {
                self.has_data = false;
                self.data.pop(); // SSE joins data fields with LF; omit the final LF.
                return Ok(Some(Frame::Data(std::mem::take(&mut self.data))));
            }
            if line.starts_with(b":") {
                return Ok(Some(Frame::Keepalive));
            }
            if line == b"data" || line.starts_with(b"data:") {
                let value = line.get(5..).unwrap_or_default();
                let value = value.strip_prefix(b" ").unwrap_or(value);
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
                self.has_data = true;
            }
        } else {
            self.line.push(byte);
        }
        if self.line.len().saturating_add(self.data.len()) > RESPONSE_LIMIT {
            return Err(Error::ResponseTooLarge);
        }
        Ok(None)
    }
}

pub fn simulations(response: reqwest::Response) -> SimulationStream {
    decode(response, |simulation: &Simulation| {
        simulation.status.phase == Phase::Completed
    })
}

pub fn frames(response: reqwest::Response) -> FrameStream {
    decode(response, |delivery: &seedbox_api::FrameDelivery| {
        matches!(delivery, seedbox_api::FrameDelivery::Closed)
    })
}

pub(crate) fn checkpoint_exports(response: reqwest::Response) -> CheckpointExportStream {
    decode_chunks(
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::from)),
        |update: &seedbox_api::CheckpointExport| {
            !matches!(update, seedbox_api::CheckpointExport::Progress { .. })
        },
        Some(activity::LIMIT),
        TokioClock::default(),
    )
}

pub fn events(response: reqwest::Response) -> EventStream {
    decode(response, |delivery: &EventDelivery| {
        matches!(delivery, EventDelivery::Closed)
    })
}

fn decode<T: DeserializeOwned + Send + 'static>(
    response: reqwest::Response,
    terminal: fn(&T) -> bool,
) -> Pin<Box<dyn Stream<Item = Result<T, Error>> + Send>> {
    decode_chunks(
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::from)),
        terminal,
        None,
        TokioClock::default(),
    )
}

fn decode_chunks<T, S, B, C>(
    chunks: S,
    terminal: fn(&T) -> bool,
    idle: Option<std::time::Duration>,
    clock: C,
) -> Pin<Box<dyn Stream<Item = Result<T, Error>> + Send>>
where
    T: DeserializeOwned + Send + 'static,
    S: Stream<Item = Result<B, Error>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    C: Clock,
{
    let state = Some((Box::pin(chunks), Decoder::default(), VecDeque::new()));
    Box::pin(futures_util::stream::unfold(state, move |state| {
        let clock = clock.clone();
        async move {
            let (mut chunks, mut decoder, mut pending) = state?;
            // Only charge time while a caller is waiting for the next event.
            // After yielding data, consumer work must not consume this budget.
            // Within this pull, partial bytes never renew it; complete frames do.
            let mut deadline = idle.map(|limit| clock.now().saturating_add(limit));
            loop {
                // Decode bytes already received before consulting the read timer;
                // caller backpressure must not invalidate an already buffered event.
                while let Some(byte) = pending.pop_front() {
                    let frame = match decoder.push(byte) {
                        Ok(Some(frame)) => frame,
                        Ok(None) => continue,
                        Err(error) => return Some((Err(error), None)),
                    };
                    deadline = idle.map(|limit| clock.now().saturating_add(limit));
                    let Frame::Data(data) = frame else {
                        continue;
                    };
                    let value: T = match serde_json::from_slice(&data) {
                        Ok(value) => value,
                        Err(error) => return Some((Err(error.into()), None)),
                    };
                    let done = terminal(&value);
                    return Some((
                        Ok(value),
                        if done {
                            None
                        } else {
                            Some((chunks, decoder, pending))
                        },
                    ));
                }
                let chunk = if let Some(deadline) = deadline {
                    tokio::select! {
                        biased;
                        _ = clock.sleep_until(deadline) => return Some((Err(Error::Stream(activity::STALLED.into())), None)),
                        chunk = chunks.next() => chunk,
                    }
                } else {
                    chunks.next().await
                };
                match chunk {
                    Some(Ok(chunk)) if chunk.as_ref().len() <= RESPONSE_LIMIT => pending.extend(chunk.as_ref().iter().copied()),
                    Some(Ok(_)) => return Some((Err(Error::ResponseTooLarge), None)),
                    Some(Err(error)) => return Some((Err(error), None)),
                    None => return Some((Err(Error::Stream("stream ended without its terminal record; inspect the simulation and reconnect".into())), None)),
                }
            }
        }
    }))
}

#[cfg(test)]
mod activity_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_delimiters_comments_and_multiline_data_preserve_payload() {
        let mut decoder = Decoder::default();
        let mut records = vec![];
        for byte in b": heartbeat\r\nevent: simulation\rdata: {\n data: ignored\ndata: \"x\":1}\r\n\r\ndata\n\n" {
            if let Some(Frame::Data(record)) = decoder.push(*byte).unwrap() { records.push(record); }
        }
        assert_eq!(records, [b"{\n\"x\":1}".to_vec(), vec![]]);
    }
}
