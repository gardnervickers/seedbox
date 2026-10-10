//! Observation-only committed output. Never part of device state or witnesses.
use crate::Result;
use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8; 8] = b"SBLOG001";
pub const MAX_CHUNKS: usize = 65_536;
const MAX_TOTAL_CHUNKS: usize = 1_048_576;

pub fn stream_limit(machines: usize) -> usize {
    MAX_CHUNKS.min(MAX_TOTAL_CHUNKS / machines.max(1) / 2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Console,
    Management,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub stream: Stream,
    pub time_ns: u64,
    /// Offset within this response's stream, independent of retention.
    pub offset: u64,
    pub length: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pending {
    chunks: Vec<Chunk>,
    console: u64,
    management: u64,
}
impl Pending {
    pub fn append(&mut self, stream: Stream, time_ns: u64, length: usize) {
        if length == 0 {
            return;
        }
        let offset = match stream {
            Stream::Console => &mut self.console,
            Stream::Management => &mut self.management,
        };
        if let Some(last) = self.chunks.last_mut().filter(|last| last.stream == stream) {
            // A burst keeps its first committed byte's time across later exits.
            last.length += length as u64;
        } else {
            self.chunks.push(Chunk {
                stream,
                time_ns,
                offset: *offset,
                length: length as u64,
            });
        }
        *offset += length as u64;
    }
    pub fn take(&mut self) -> Vec<Chunk> {
        std::mem::take(self).chunks
    }
}

/// Every management write ends advancement immediately. Thus a response has
/// at most a console burst followed by a management burst, regardless of the
/// number of guest exits or bytes. Missing metadata is never accepted as legacy.
pub fn validate(
    chunks: &[Chunk],
    console: usize,
    management: usize,
    start: u64,
    stop: u64,
    observe_console: bool,
) -> Result<()> {
    if chunks.len() > 2 {
        return Err("worker log metadata exceeds slice burst bound".into());
    }
    let mut lengths = [0u64; 2];
    let mut previous = None;
    for chunk in chunks {
        let slot = match chunk.stream {
            Stream::Console => 0,
            Stream::Management => 1,
        };
        if chunk.length == 0
            || chunk.offset != 0
            || lengths[slot] != 0
            || chunk.time_ns < start
            || chunk.time_ns > stop
            || previous.is_some_and(|(stream, time)| slot <= stream || chunk.time_ns < time)
            || (slot == 1 && chunk.time_ns != stop)
            || (slot == 0 && observe_console && (chunk.length != 1 || chunk.time_ns != stop))
        {
            return Err("invalid worker committed log metadata".into());
        }
        lengths[slot] = chunk.length;
        previous = Some((slot, chunk.time_ns));
    }
    if lengths != [console as u64, management as u64] {
        return Err("worker log metadata does not cover output bytes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bursts_keep_first_commit_time_and_reset_response_offsets() {
        let mut pending = Pending::default();
        pending.append(Stream::Console, 3, 2);
        pending.append(Stream::Console, 8, 1);
        pending.append(Stream::Management, 10, 4);
        let chunks = pending.take();
        assert_eq!(
            chunks[0],
            Chunk {
                stream: Stream::Console,
                time_ns: 3,
                offset: 0,
                length: 3
            }
        );
        assert_eq!(
            chunks[1],
            Chunk {
                stream: Stream::Management,
                time_ns: 10,
                offset: 0,
                length: 4
            }
        );
        validate(&chunks, 3, 4, 0, 10, false).unwrap();
        pending.append(Stream::Console, 11, 1);
        assert_eq!(pending.take()[0].offset, 0);
    }
    #[test]
    fn rejects_untrusted_missing_overlapping_regressing_or_future_metadata() {
        let good = vec![
            Chunk {
                stream: Stream::Console,
                time_ns: 3,
                offset: 0,
                length: 2,
            },
            Chunk {
                stream: Stream::Management,
                time_ns: 10,
                offset: 0,
                length: 4,
            },
        ];
        validate(&good, 2, 4, 1, 10, false).unwrap();
        assert!(validate(&[], 2, 4, 1, 10, false).is_err());
        for (field, value) in [(0, 0), (0, u64::MAX), (1, 1), (2, 0), (2, u64::MAX)] {
            let mut bad = good.clone();
            match field {
                0 => bad[0].time_ns = value,
                1 => bad[0].offset = value,
                _ => bad[0].length = value,
            }
            assert!(validate(&bad, 2, 4, 1, 10, false).is_err());
        }
        let mut bad = good.clone();
        bad.reverse();
        assert!(validate(&bad, 2, 4, 1, 10, false).is_err());
        bad = good.clone();
        bad.push(bad[0].clone());
        assert!(validate(&bad, 2, 4, 1, 10, false).is_err());
        bad = good.clone();
        bad[1].time_ns = 9;
        assert!(validate(&bad, 2, 4, 1, 10, false).is_err());
        assert!(validate(&good, 2, 4, 1, 10, true).is_err());
        assert!(serde_json::from_str::<Chunk>(
            r#"{"stream":"Console","time_ns":0,"offset":0,"length":1,"unknown":0}"#
        )
        .is_err());
    }
    #[test]
    fn per_stream_budget_fits_reader_for_maximum_world() {
        for machines in [1, 8, 9, 254] {
            let limit = stream_limit(machines);
            assert!(limit <= MAX_CHUNKS);
            assert!(limit * 2 * machines <= MAX_TOTAL_CHUNKS);
        }
    }
}
