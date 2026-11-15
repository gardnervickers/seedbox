//! What a run said and what happened to it, with the simulated time of each
//! fact. A fact seen at time `t` on a run recurs at `t` on any run that takes
//! the same inputs up to `t`, so its time is a place to go back to.
//!
//! The trace is append-only and a fork shares everything recorded so far,
//! including lines still incomplete, so a trace is saved with each
//! checkpoint at no cost.
use seedbox_driver::{
    driver::{Cause, Event, EventKind, MachineId, Transition},
    pv::{Response, Stream},
};
use seedbox_machine::machine::Timestamp;
use std::sync::Arc;

/// One recorded fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// When the fact occurred. For a line this is the time of the output
    /// that carried its first byte: the earliest moment the stream shows it.
    pub at: Timestamp,
    /// The machine it belongs to, when it belongs to one.
    pub machine: Option<MachineId>,
    /// That machine's boot.
    pub boot: u64,
    pub entry: Entry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// One line of application output, without its newline. `truncated` when
    /// the boot ended before the line did.
    Line {
        stream: Stream,
        text: String,
        truncated: bool,
    },
    /// The boot's supervisor reported the application ready to be driven.
    Ready,
    /// The application ended by itself with this status.
    Exit { code: u8 },
    /// A change the Driver applied to the machine: pause, crash, reboot, ...
    Transition { change: Transition, cause: Cause },
    /// Any other committed fact, as the Driver published it: admissions,
    /// rejections, settlements, breakpoint hits. Machine output is never
    /// kept here.
    Event(Box<EventKind>),
}

impl Record {
    /// Whether this is something a machine did or that happened to it at a
    /// time of its own, as opposed to a control fact stamped by the commit
    /// that confirmed it. Only occurrences are compared between runs.
    pub fn is_occurrence(&self) -> bool {
        !matches!(self.entry, Entry::Event(_))
    }
}

/// A line not yet ended by a newline.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Partial {
    machine: MachineId,
    stream: Stream,
    boot: u64,
    at: Timestamp,
    bytes: Vec<u8>,
}

/// Records shared with every trace forked after them.
#[derive(Debug)]
struct Shared {
    earlier: Option<Arc<Shared>>,
    /// The index of `records[0]` in the whole trace.
    base: usize,
    records: Vec<Record>,
}

#[derive(Debug, Default)]
pub struct Trace {
    shared: Option<Arc<Shared>>,
    /// The number of records in `shared` and everything before it.
    base: usize,
    records: Vec<Record>,
    partial: Vec<Partial>,
}

impl Trace {
    pub fn len(&self) -> usize {
        self.base + self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A trace that continues from this one's records and incomplete lines.
    /// Both keep recording independently.
    pub fn fork(&mut self) -> Trace {
        if !self.records.is_empty() {
            self.shared = Some(Arc::new(Shared {
                earlier: self.shared.take(),
                base: self.base,
                records: std::mem::take(&mut self.records),
            }));
            self.base = self.len_shared();
        }
        Trace {
            shared: self.shared.clone(),
            base: self.base,
            records: Vec::new(),
            partial: self.partial.clone(),
        }
    }

    fn len_shared(&self) -> usize {
        self.shared
            .as_ref()
            .map_or(0, |shared| shared.base + shared.records.len())
    }

    /// The records from index `from` on, oldest first.
    pub fn since(&self, from: usize) -> impl Iterator<Item = &Record> {
        let mut blocks: Vec<(usize, &[Record])> = vec![(self.base, &self.records)];
        let mut shared = self.shared.as_deref();
        while let Some(block) = shared {
            if block.base + block.records.len() <= from {
                break;
            }
            blocks.push((block.base, &block.records));
            shared = block.earlier.as_deref();
        }
        blocks.into_iter().rev().flat_map(move |(base, records)| {
            &records[from.saturating_sub(base).min(records.len())..]
        })
    }

    /// Every record, oldest first.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.since(0)
    }

    /// Record what the Driver published; returns the index of the first new
    /// record. Output is cut into lines; a line is recorded once its newline
    /// arrives, or when its boot ends first.
    pub fn take(&mut self, events: impl IntoIterator<Item = Event>) -> usize {
        let first = self.len();
        for event in events {
            match event.kind {
                EventKind::Output(output) => match output.payload {
                    Response::ApplicationOutput { stream, bytes } => {
                        self.output(output.machine, output.boot, output.at, stream, &bytes)
                    }
                    Response::Ready => {
                        self.push(output.at, Some(output.machine), output.boot, Entry::Ready)
                    }
                    Response::ApplicationOutcome { code } => {
                        self.end_lines(output.machine);
                        self.push(
                            output.at,
                            Some(output.machine),
                            output.boot,
                            Entry::Exit { code },
                        )
                    }
                    _ => {}
                },
                EventKind::Transition {
                    machine,
                    boot,
                    change,
                    cause,
                } => {
                    if matches!(change, Transition::Crashed | Transition::Rebooted { .. }) {
                        self.end_lines(machine);
                    }
                    self.push(
                        event.at,
                        Some(machine),
                        boot,
                        Entry::Transition { change, cause },
                    )
                }
                kind => self.push(event.at, None, 0, Entry::Event(Box::new(kind))),
            }
        }
        first
    }

    fn push(&mut self, at: Timestamp, machine: Option<MachineId>, boot: u64, entry: Entry) {
        self.records.push(Record {
            at,
            machine,
            boot,
            entry,
        });
    }

    fn output(
        &mut self,
        machine: MachineId,
        boot: u64,
        at: Timestamp,
        stream: Stream,
        bytes: &[u8],
    ) {
        let index = match self
            .partial
            .iter()
            .position(|partial| partial.machine == machine && partial.stream == stream)
        {
            // Output of a later boot never continues an earlier boot's line.
            Some(index) if self.partial[index].boot != boot => {
                let ended = self.partial.remove(index);
                self.line(ended, true);
                None
            }
            found => found,
        };
        let mut partial = match index {
            Some(index) => self.partial.remove(index),
            None => Partial {
                machine,
                stream,
                boot,
                at,
                bytes: Vec::new(),
            },
        };
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            if partial.bytes.is_empty() {
                partial.at = at;
            }
            partial.bytes.extend_from_slice(line);
            if line.ends_with(b"\n") {
                partial.bytes.pop();
                let rest = Partial {
                    bytes: Vec::new(),
                    ..partial.clone()
                };
                self.line(std::mem::replace(&mut partial, rest), false);
            }
        }
        if !partial.bytes.is_empty() {
            self.partial.push(partial);
        }
    }

    fn line(&mut self, partial: Partial, truncated: bool) {
        let mut bytes = partial.bytes;
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        self.push(
            partial.at,
            Some(partial.machine),
            partial.boot,
            Entry::Line {
                stream: partial.stream,
                text: String::from_utf8_lossy(&bytes).into_owned(),
                truncated,
            },
        );
    }

    /// The machine's boot or application ended: its incomplete lines are
    /// all there will be of them.
    fn end_lines(&mut self, machine: MachineId) {
        let (ended, kept) = std::mem::take(&mut self.partial)
            .into_iter()
            .partition(|partial| partial.machine == machine);
        self.partial = kept;
        for partial in ended {
            self.line(partial, true);
        }
    }

    /// Each machine's occurrences from index `from` on, in its own order,
    /// indexed by the machine's identity. Two runs that took the same inputs
    /// agree on these whatever barriers they stopped at; the order between
    /// machines at one time is not part of that.
    pub(crate) fn occurrences(&self, from: usize) -> Vec<Vec<&Record>> {
        let mut machines: Vec<Vec<&Record>> = Vec::new();
        for record in self.since(from).filter(|record| record.is_occurrence()) {
            let Some(index) = record.machine.map(MachineId::index) else {
                continue;
            };
            if machines.len() <= index {
                machines.resize_with(index + 1, Vec::new);
            }
            machines[index].push(record);
        }
        machines
    }

    /// A trace continuing from this one, leaving this one as it is.
    pub(crate) fn continued(&self) -> Trace {
        let shared = if self.records.is_empty() {
            self.shared.clone()
        } else {
            Some(Arc::new(Shared {
                earlier: self.shared.clone(),
                base: self.base,
                records: self.records.clone(),
            }))
        };
        Trace {
            shared,
            base: self.len(),
            records: Vec::new(),
            partial: self.partial.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_driver::driver::Position;

    fn said(sequence: u64, at: u64, text: &str) -> Event {
        let at = Timestamp::from_nanos(at);
        Event {
            sequence,
            position: Position {
                boundary_index: 0,
                completed_quanta: 0,
                at,
            },
            at,
            kind: EventKind::Output(seedbox_driver::driver::MachineResponse {
                machine: MachineId::ORACLE,
                boot: 0,
                at,
                payload: Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes: text.as_bytes().to_vec(),
                },
            }),
        }
    }
    fn texts<'a>(records: impl Iterator<Item = &'a Record>) -> Vec<(u64, String)> {
        records
            .map(|record| match &record.entry {
                Entry::Line { text, .. } => (record.at.as_nanos(), text.clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn forks_share_records_and_incomplete_lines_then_go_their_own_way() {
        let mut trace = Trace::default();
        assert_eq!(trace.take([said(0, 1, "a\nb\nc")]), 0);
        let mut first = trace.fork();
        let mut second = first.fork();
        assert_eq!((trace.len(), first.len(), second.len()), (2, 2, 2));
        // Each ends the line begun before the fork in its own way.
        assert_eq!(trace.take([said(1, 5, "!\n")]), 2);
        assert_eq!(first.take([said(1, 6, "?\nd\n")]), 2);
        let third = first.continued();
        second.take([said(1, 7, "e")]);
        let whole = |trace: &Trace| texts(trace.records());
        assert_eq!(whole(&trace)[2], (1, "c!".to_owned()));
        assert_eq!(
            whole(&first)[2..],
            [(1, "c?".to_owned()), (6, "d".to_owned())]
        );
        assert_eq!(whole(&second).len(), 2);
        assert_eq!(whole(&third), whole(&first));
        assert_eq!(texts(first.since(3)), [(6, "d".to_owned())]);
        assert!(first.since(9).next().is_none());
        // A deeper chain still reads from any index.
        let mut deep = third.continued();
        deep.take([said(2, 9, "f\n")]);
        assert_eq!(texts(deep.since(1)).len(), 4);
        assert_eq!(texts(deep.since(4)), [(9, "f".to_owned())]);
    }
}
