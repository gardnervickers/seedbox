//! Committed-time observation of retained binary console and management streams.
//! Chunk times are the first byte's VMM commit time, not application timestamps.
//! Equal times are ordered by machine name, then the machine's output sequence
//! across both streams.
use crate::{orchestration, retained, Error, Result};
use seedbox_engine_core::log_index::{self, ChunkRecord, MAGIC, MAX_CHUNKS, RECORD_BYTES};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const MAX_TOTAL_CHUNKS: usize = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Console,
    Mgmt,
}
impl Stream {
    fn extension(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Mgmt => "mgmt",
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub machine: Option<String>,
    pub stream: Option<Stream>,
    /// Inclusive lower bound on the chunk's first-byte time, in simulated nanoseconds.
    pub since_ns: Option<u64>,
    /// Exclusive upper bound on the chunk's first-byte time, in simulated nanoseconds.
    pub until_ns: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Chunk {
    pub machine: String,
    pub incarnation: u32,
    pub stream: Stream,
    pub time_ns: u64,
    pub order: u64,
    pub offset: u64,
    pub length: u64,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Logs,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub status: Status,
    pub chunks: Vec<Chunk>,
    /// Missing indexes, unindexed suffixes and retention omissions remain explicit,
    /// even when a filter selects no chunks. Completeness cannot be inferred from a filter.
    pub omissions: BTreeMap<String, String>,
}
/// All metadata is validated before filtering. Raw streams are checksum-verified
/// once and held open; copying a selected chunk uses constant-size buffers.
pub struct Reader {
    pub report: Report,
    streams: BTreeMap<(String, Stream), File>,
    directory: PathBuf,
}
fn bad(directory: &Path, message: impl std::fmt::Display) -> Error {
    retained::bad("logs", directory, message)
}
impl Reader {
    pub fn open(directory: &Path, filter: &Filter) -> Result<Self> {
        if filter
            .since_ns
            .zip(filter.until_ns)
            .is_some_and(|(a, b)| a > b)
        {
            return Err(Error::Setup(
                "--until-ns must be at least --since-ns".into(),
            ));
        }
        let record = orchestration::inspect(directory)
            .map_err(|e| bad(&directory.join("manifest.json"), e))?;
        if let Some(machine) = &filter.machine {
            if !record.identity.inputs.guests.contains_key(machine) {
                return Err(Error::Setup(format!(
                    "--machine {machine}: unknown machine; use a machine from inspect --json"
                )));
            }
        }
        let mut reader = Self {
            report: Report {
                status: Status::Logs,
                chunks: Vec::new(),
                omissions: record.omitted_logs.clone(),
            },
            streams: BTreeMap::new(),
            directory: directory.to_owned(),
        };
        let mut total = 0;
        for machine in record.identity.inputs.guests.keys() {
            let mut orders = BTreeMap::new();
            for stream in [Stream::Console, Stream::Mgmt] {
                let name = format!("machines/{machine}.{}", stream.extension());
                let index_name = log_index::index_name(&name);
                let Some(mut index) = retained::log(directory, &record, &index_name, "logs")?
                else {
                    reader.report.omissions.entry(index_name).or_insert(
                        "committed-time index unavailable; raw bytes cannot be timed".into(),
                    );
                    continue;
                };
                let size = index.metadata().map_err(|e| bad(directory, e))?.len();
                let header = MAGIC.len() as u64;
                let record_bytes = RECORD_BYTES as u64;
                if size < header
                    || size > header + record_bytes * MAX_CHUNKS as u64
                    || (size - header) % record_bytes != 0
                {
                    return Err(bad(
                        directory,
                        format!("{index_name}: truncated or oversized chunk index"),
                    ));
                }
                let mut magic = [0; MAGIC.len()];
                index
                    .read_exact(&mut magic)
                    .map_err(|e| bad(directory, e))?;
                if &magic != MAGIC {
                    return Err(bad(
                        directory,
                        format!("{index_name}: unsupported chunk index version"),
                    ));
                }
                let raw = retained::log(directory, &record, &name, "logs")?;
                let raw_len = raw
                    .as_ref()
                    .map(|f| f.metadata().map(|m| m.len()))
                    .transpose()
                    .map_err(|e| bad(directory, e))?;
                if raw.is_none() {
                    reader
                        .report
                        .omissions
                        .entry(name.clone())
                        .or_insert("raw stream unavailable; indexed bytes omitted".into());
                }
                let mut end = 0u64;
                let mut previous = None;
                for _ in 0..(size - header) / record_bytes {
                    total += 1;
                    if total > MAX_TOTAL_CHUNKS {
                        return Err(bad(directory, "chunk indexes exceed total record bound"));
                    }
                    let mut bytes = [0; RECORD_BYTES];
                    index
                        .read_exact(&mut bytes)
                        .map_err(|e| bad(directory, e))?;
                    let ChunkRecord {
                        time_ns: time,
                        incarnation,
                        order,
                        offset,
                        length,
                    } = ChunkRecord::from_bytes(&bytes);
                    let next = offset.checked_add(length);
                    if incarnation > u32::MAX as u64
                        || offset != end
                        || length == 0
                        || next.is_none()
                        || raw_len.zip(next).is_some_and(|(len, next)| next > len)
                        || previous
                            .is_some_and(|(t, i, o)| time < t || incarnation < i || order <= o)
                        || orders.insert(order, (time, incarnation)).is_some()
                    {
                        return Err(bad(
                            directory,
                            format!("{index_name}: invalid chunk bounds or order"),
                        ));
                    }
                    end = next.ok_or_else(|| bad(directory, "chunk end overflow"))?;
                    previous = Some((time, incarnation, order));
                    if raw.is_some()
                        && filter.machine.as_ref().is_none_or(|m| m == machine)
                        && filter.stream.is_none_or(|s| s == stream)
                        && filter.since_ns.is_none_or(|t| time >= t)
                        && filter.until_ns.is_none_or(|t| time < t)
                    {
                        reader.report.chunks.push(Chunk {
                            machine: machine.clone(),
                            stream,
                            time_ns: time,
                            incarnation: incarnation as u32,
                            order,
                            offset,
                            length,
                        });
                    }
                }
                if let Some(len) = raw_len {
                    if end < len {
                        reader.report.omissions.insert(index_name,format!("unindexed raw bytes [{end}, {len}); capture bound, interrupted capture or sink failure"));
                    }
                }
                if let Some(raw) = raw {
                    reader.streams.insert((machine.clone(), stream), raw);
                }
            }
            let mut previous = None;
            for &(time, incarnation) in orders.values() {
                if previous.is_some_and(|(t, i)| time < t || incarnation < i) {
                    return Err(bad(
                        directory,
                        "cross-stream time/incarnation order regressed",
                    ));
                }
                previous = Some((time, incarnation));
            }
        }
        reader.report.chunks.sort_by(|a, b| {
            (a.time_ns, &a.machine, a.order).cmp(&(b.time_ns, &b.machine, b.order))
        });
        Ok(reader)
    }
    /// Copy the exact original bytes of a selected chunk by its report index.
    pub fn copy_chunk(&mut self, index: usize, output: &mut impl Write) -> Result<()> {
        let chunk = self
            .report
            .chunks
            .get(index)
            .ok_or_else(|| Error::Setup("logs: chunk index out of range".into()))?;
        let file = self
            .streams
            .get_mut(&(chunk.machine.clone(), chunk.stream))
            .ok_or_else(|| Error::Integrity("chunk stream unavailable".into()))?;
        file.seek(SeekFrom::Start(chunk.offset)).map_err(|e| {
            bad(
                &self.directory,
                format!("{} {:?}: {e}", chunk.machine, chunk.stream),
            )
        })?;
        let copied = std::io::copy(&mut file.take(chunk.length), output).map_err(|e| {
            bad(
                &self.directory,
                format!("copying {} {:?}: {e}", chunk.machine, chunk.stream),
            )
        })?;
        if copied != chunk.length {
            return Err(Error::Integrity(
                "raw stream truncated after validation".into(),
            ));
        }
        Ok(())
    }
}

impl Reader {
    /// Copy every selected chunk's exact bytes in report order.
    pub fn write_raw(&mut self, output: &mut impl Write) -> Result<()> {
        for index in 0..self.report.chunks.len() {
            self.copy_chunk(index, output)?;
        }
        Ok(())
    }

    /// Write the one `logs --json` document: status, omissions, then every
    /// selected chunk with its streamed `bytes_hex`.
    pub fn write_json(&mut self, output: &mut impl Write) -> Result<()> {
        writeln!(
            output,
            "{{\n  \"status\": {},\n  \"omissions\": {},\n  \"chunks\": [",
            serde_json::to_string(&self.report.status)?,
            serde_json::to_string_pretty(&self.report.omissions)?
        )?;
        let count = self.report.chunks.len();
        for index in 0..count {
            output.write_all(b"    ")?;
            self.write_chunk_json(index, output)?;
            output.write_all(if index + 1 < count { b",\n" } else { b"\n" })?;
        }
        writeln!(output, "  ]\n}}")?;
        Ok(())
    }
}

/// A chunk's JSON representation adds `bytes_hex` to its metadata. This writer
/// streams arbitrary payload sizes without accumulating raw bytes or hex strings.
impl Reader {
    pub fn write_chunk_json(&mut self, index: usize, output: &mut impl Write) -> Result<()> {
        let chunk = self
            .report
            .chunks
            .get(index)
            .ok_or_else(|| Error::Setup("logs: chunk index out of range".into()))?;
        let mut metadata = serde_json::to_vec(chunk)?;
        metadata.pop(); // Close the object after the streamed bytes_hex field.
        output.write_all(&metadata)?;
        output.write_all(b",\"bytes_hex\":\"")?;
        struct Hex<'a, W>(&'a mut W);
        impl<W: Write> Write for Hex<'_, W> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                const DIGITS: &[u8] = b"0123456789abcdef";
                let mut buffer = [0; 8192];
                for part in bytes.chunks(4096) {
                    for (i, b) in part.iter().enumerate() {
                        buffer[2 * i] = DIGITS[(b >> 4) as usize];
                        buffer[2 * i + 1] = DIGITS[(b & 15) as usize];
                    }
                    self.0.write_all(&buffer[..part.len() * 2])?;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }
        self.copy_chunk(index, &mut Hex(output))?;
        output.write_all(b"\"}")?;
        Ok(())
    }
}
