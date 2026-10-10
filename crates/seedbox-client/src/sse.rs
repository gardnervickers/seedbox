//! Incremental, bounded decoding of SSE data fields. Delivery chunk boundaries
//! have no relationship to event, line or UTF-8 boundaries.

use crate::{Error, RESPONSE_LIMIT};
use futures_util::Stream;
use seedbox_api::{EventDelivery, Phase, Simulation};
use serde::de::DeserializeOwned;
use std::{collections::VecDeque, pin::Pin};

pub type SimulationStream = Pin<Box<dyn Stream<Item = Result<Simulation, Error>> + Send>>;
pub type EventStream = Pin<Box<dyn Stream<Item = Result<EventDelivery, Error>> + Send>>;
pub type LogStream = Pin<Box<dyn Stream<Item = Result<seedbox_api::LogDelivery, Error>> + Send>>;

pub fn logs(response: reqwest::Response) -> LogStream {
    decode(response, |delivery: &seedbox_api::LogDelivery| {
        matches!(delivery, seedbox_api::LogDelivery::Closed)
    })
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
    has_data: bool,
}

impl Decoder {
    fn push(&mut self, byte: u8) -> Result<Option<Vec<u8>>, Error> {
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
                return Ok(Some(std::mem::take(&mut self.data)));
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

pub fn events(response: reqwest::Response) -> EventStream {
    decode(response, |delivery: &EventDelivery| {
        matches!(delivery, EventDelivery::Closed)
    })
}

fn decode<T: DeserializeOwned + Send + 'static>(
    response: reqwest::Response,
    terminal: fn(&T) -> bool,
) -> Pin<Box<dyn Stream<Item = Result<T, Error>> + Send>> {
    let state = Some((response, Decoder::default(), VecDeque::new()));
    Box::pin(futures_util::stream::unfold(
        state,
        move |state| async move {
            let (mut response, mut decoder, mut pending) = state?;
            loop {
                while let Some(byte) = pending.pop_front() {
                    let data = match decoder.push(byte) {
                        Ok(Some(data)) => data,
                        Ok(None) => continue,
                        Err(e) => return Some((Err(e), None)),
                    };
                    let simulation: T = match serde_json::from_slice(&data) {
                        Ok(s) => s,
                        Err(e) => return Some((Err(e.into()), None)),
                    };
                    let terminal = terminal(&simulation);
                    return Some((
                        Ok(simulation),
                        if terminal {
                            None
                        } else {
                            Some((response, decoder, pending))
                        },
                    ));
                }
                match response.chunk().await {
                Ok(Some(chunk)) if chunk.len() <= RESPONSE_LIMIT => pending.extend(chunk),
                Ok(Some(_)) => return Some((Err(Error::ResponseTooLarge), None)),
                Ok(None) => {
                    return Some((
                        Err(Error::Stream(
                            "stream ended without its terminal record; inspect the simulation and reconnect"
                                .into(),
                        )),
                        None,
                    ))
                }
                Err(e) => return Some((Err(e.into()), None)),
            }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_delimiters_comments_and_multiline_data_preserve_payload() {
        let mut decoder = Decoder::default();
        let mut records = vec![];
        for byte in b": heartbeat\r\nevent: simulation\rdata: {\n data: ignored\ndata: \"x\":1}\r\n\r\ndata\n\n" {
            if let Some(record) = decoder.push(*byte).unwrap() { records.push(record); }
        }
        assert_eq!(records, [b"{\n\"x\":1}".to_vec(), vec![]]);
    }
}
