//! Pure serializable controller fixture client. The same issue/observe logic can
//! run natively or inside a privileged Linux guest; only its byte transport differs.
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
const PUT: u8 = 3;
pub fn application_fixture_frame(kind: u8, id: RequestId, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0; 14];
    b[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    b[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 2]);
    b[12..14].copy_from_slice(&[0x88, 0xb5]);
    b.push(kind);
    b.extend(id.to_le_bytes());
    b.extend(data);
    b
}
pub fn fixture_query(machine: MachineId, kind: u8, data: &[u8], timeout: u64) -> Command {
    Command::Query {
        destination: machine,
        frame: application_fixture_frame(kind, RequestId(0), data),
        correlation_u64_offset: 15,
        timeout_ticks: Tick(timeout),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Predicate {
    FrameDelivered {
        local: String,
        expected: bool,
    },
    FramePayload {
        local: String,
        bytes: Vec<u8>,
    },
    Partial {
        locals: Vec<String>,
    },
    Completion {
        local: String,
        kind: CompletionKind,
    },
    Bytes {
        local: String,
        bytes: Vec<u8>,
    },
    Health {
        local: String,
        generation: Option<u64>,
        value: u64,
    },
    Stale {
        local: String,
        minimum: u64,
    },
    Quorum {
        locals: Vec<String>,
        value: u64,
        minimum: usize,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Template {
    Literal(Command),
    Heal(String),
    Await(String),
    InspectFrame(String),
    Assert {
        label: String,
        predicate: Predicate,
    },
    PutFromHealth {
        machine: MachineId,
        source: String,
    },
    PartialNote(Vec<String>),
    QuorumNote {
        locals: Vec<String>,
        value: u64,
        down: Option<MachineId>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instruction {
    pub command: Template,
    pub save: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingClient {
    pub command: CommandId,
    pub save: String,
    pub pc: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeController {
    pub controller: ControllerId,
    pub incarnation: u64,
    pub next_command: u64,
    pub last_observation: u64,
    pub pc: usize,
    pub pending: Option<PendingClient>,
    pub locals: BTreeMap<String, Observation>,
    pub program: Vec<Instruction>,
    pub result: Option<Result<()>>,
    pub last_received: Option<ObservationEnvelope>,
}
impl NativeController {
    pub fn new(program: Vec<Instruction>) -> Self {
        Self {
            controller: ControllerId(1),
            incarnation: 1,
            next_command: 1,
            last_observation: 0,
            pc: 0,
            pending: None,
            locals: BTreeMap::new(),
            program,
            result: None,
            last_received: None,
        }
    }
    fn inspected(&self, local: &str) -> &Value {
        match &self.locals[local] {
            Observation::Inspected(v) => v,
            _ => panic!("expected inspected local {local}"),
        }
    }
    fn completion(&self, local: &str) -> Option<&Completion> {
        match &self.locals[local] {
            Observation::Completion(c) => Some(c),
            _ => None,
        }
    }
    fn health(&self, local: &str) -> Option<(u64, u64, u64)> {
        let CompletionKind::Reply(b) = &self.completion(local)?.kind else {
            return None;
        };
        if b.len() != 24 {
            return None;
        }
        Some((
            u64::from_le_bytes(b[..8].try_into().unwrap()),
            u64::from_le_bytes(b[8..16].try_into().unwrap()),
            u64::from_le_bytes(b[16..24].try_into().unwrap()),
        ))
    }
    fn delivered(&self, local: &str) -> bool {
        !self.inspected(local).is_null()
    }
    fn evaluate(&self, p: &Predicate) -> bool {
        match p {
            Predicate::FrameDelivered { local, expected } => self.delivered(local) == *expected,
            Predicate::FramePayload { local, bytes } => {
                self.inspected(local)["bytes"] == json!(bytes)
            }
            Predicate::Partial { locals } => {
                let n = locals.iter().filter(|l| self.delivered(l)).count();
                n > 0 && n < locals.len()
            }
            Predicate::Completion { local, kind } => {
                self.completion(local).is_some_and(|c| c.kind == *kind)
            }
            Predicate::Bytes { local, bytes } => *self.inspected(local) == json!(bytes),
            Predicate::Health {
                local,
                generation,
                value,
            } => self.health(local).is_some_and(|(g, v, d)| {
                generation.is_none_or(|wanted| wanted == g) && v == *value && d == *value
            }),
            Predicate::Stale { local, minimum } => self
                .inspected(local)
                .as_u64()
                .is_some_and(|n| n >= *minimum),
            Predicate::Quorum {
                locals,
                value,
                minimum,
            } => {
                locals
                    .iter()
                    .filter(|l| {
                        self.health(l)
                            .is_some_and(|(_, v, d)| v == *value && d == *value)
                    })
                    .count()
                    >= *minimum
            }
        }
    }
    fn command(&self, t: &Template) -> Command {
        match t {
            Template::Literal(c) => c.clone(),
            Template::Heal(local) => {
                let Observation::FaultApplied(handle) = self.locals[local] else {
                    panic!("expected fault handle")
                };
                Command::Heal { handle }
            }
            Template::Await(local) => {
                let request = match self.locals[local] {
                    Observation::Submitted(owner) => owner.request,
                    Observation::SleepArmed { request } => request,
                    _ => panic!("expected submitted request"),
                };
                Command::Await { request }
            }
            Template::InspectFrame(local) => {
                let Observation::Transmitted { wire_id, .. } = self.locals[local] else {
                    panic!("expected transmitted frame")
                };
                Command::Inspect(Inspect::Frame { wire_id })
            }
            Template::Assert { label, predicate } => Command::Assert {
                label: label.clone(),
                condition: self.evaluate(predicate),
            },
            Template::PutFromHealth { machine, source } => {
                let (_, value, _) = self.health(source).expect("valid peer recovery source");
                fixture_query(*machine, PUT, &value.to_le_bytes(), 40)
            }
            Template::PartialNote(locals) => {
                let delivered: Vec<_> = locals
                    .iter()
                    .filter(|l| self.delivered(l))
                    .map(|l| self.inspected(l)["id"].clone())
                    .collect();
                Command::Note {
                    label: "partial_loss_observation".into(),
                    detail: json!({"sent":locals.len(),"delivered":delivered.len(),"delivered_ids":delivered}),
                }
            }
            Template::QuorumNote {
                locals,
                value,
                down,
            } => {
                let count = locals
                    .iter()
                    .filter(|l| {
                        self.health(l)
                            .is_some_and(|(_, v, d)| v == *value && d == *value)
                    })
                    .count();
                Command::Note {
                    label: "quorum_read".into(),
                    detail: json!({"matching_replicas":count,"value":value,"down":down}),
                }
            }
        }
    }
    pub fn issue(&mut self) -> Result<Option<Vec<u8>>> {
        if self.pending.is_some() || self.result.is_some() {
            return Ok(None);
        }
        if self.pc == self.program.len() {
            self.result = Some(Ok(()));
            return Ok(None);
        }
        let instruction = &self.program[self.pc];
        let command = self.command(&instruction.command);
        let id = CommandId(self.next_command);
        self.next_command += 1;
        self.pending = Some(PendingClient {
            command: id,
            save: instruction.save.clone(),
            pc: self.pc,
        });
        encode_control(&ControlMessage::Command(CommandEnvelope {
            version: PROTOCOL_VERSION,
            controller: self.controller,
            incarnation: self.incarnation,
            id,
            command,
        }))
        .map(Some)
    }
    pub fn observe(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let ControlMessage::Observation(e) = decode_control(bytes)? else {
            return Err("native client received non-observation".into());
        };
        if e.controller != self.controller || e.incarnation != self.incarnation {
            return Err("observation owner mismatch".into());
        }
        if e.sequence.0 == self.last_observation {
            if self.last_received.as_ref().is_none_or(|last| {
                serde_json::to_value(last).unwrap() != serde_json::to_value(&e).unwrap()
            }) {
                return Err("duplicate observation payload mismatch".into());
            }
        } else {
            if e.sequence.0 != self.last_observation + 1 {
                return Err("lost or reordered native observation".into());
            }
            let pending = self
                .pending
                .take()
                .ok_or("observation without pending continuation")?;
            if e.command != pending.command || pending.pc != self.pc {
                return Err("observation command/phase mismatch".into());
            }
            self.locals.insert(pending.save, e.observation.clone());
            self.pc += 1;
            self.last_observation = e.sequence.0;
            match &e.observation {
                Observation::Assertion {
                    label,
                    passed: false,
                } => self.result = Some(Err(label.clone())),
                Observation::Error(e) => self.result = Some(Err(e.clone())),
                _ => {}
            }
            self.last_received = Some(e.clone());
        }
        encode_control(&ControlMessage::Ack {
            version: PROTOCOL_VERSION,
            controller: self.controller,
            incarnation: self.incarnation,
            sequence: e.sequence,
        })
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(p) = &self.pending
            && (p.pc != self.pc || p.command.0 + 1 != self.next_command)
        {
            return Err("native pending phase corrupt".into());
        }
        if self.pc > self.program.len() {
            return Err("native program phase outside program".into());
        }
        Ok(())
    }
}
