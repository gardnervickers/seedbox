//! One seeded coordinator owns instruction order, virtual time, and completions.
use crate::{
    backend::{Process, State, StopReason, SyscallRequest},
    config::{self, NodeSpec, RunConfig},
    error::{Error, Result},
    kernel::{Action, Address, Descriptor, File, Node, Socket, WaitKind, path},
    linux_errno as errno,
    random::RandomStream,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    net::Ipv4Addr,
    rc::Rc,
};
use unicorn_engine::Prot;

/// Sorted keys, compact separators, ASCII escapes. Compatible with v1 reports.
pub fn canonical(value: &Value) -> Vec<u8> {
    let serialized = serde_json::to_string(value).expect("serialize report value");
    let mut bytes = Vec::with_capacity(serialized.len());
    for ch in serialized.chars() {
        if ch.is_ascii() {
            bytes.push(ch as u8);
        } else {
            let mut units = [0; 2];
            for unit in ch.encode_utf16(&mut units) {
                bytes.extend(format!("\\u{unit:04x}").bytes());
            }
        }
    }
    bytes
}
pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) struct Wait {
    pub id: u64,
    pub node: usize,
    pub tid: u64,
    pub request: SyscallRequest,
    pub kind: WaitKind,
}
pub(crate) enum Event {
    Completion(u64),
    Packet {
        source_node: usize,
        destination_node: usize,
        source: Address,
        destination: Address,
        payload: Vec<u8>,
    },
    Stream {
        source_node: usize,
        destination_node: usize,
        socket: Rc<RefCell<Socket>>,
        payload: Vec<u8>,
        eof: bool,
        reset: bool,
    },
}

pub struct Simulation {
    pub config: RunConfig,
    pub seed: u64,
    pub instruction_trace: bool,
    pub now: u64,
    pub instructions: u64,
    pub nodes: Vec<Node>,
    pub trace: Vec<Value>,
    pub status: String,
    pub error: Option<String>,
    manifest: Value,
    instruction_hash: Sha256,
    streams: BTreeMap<String, RandomStream>,
    ip_names: BTreeMap<Ipv4Addr, usize>,
    pub(crate) waits: BTreeMap<u64, Wait>,
    events: BTreeMap<(u64, u64), Event>,
    pub(crate) bindings: BTreeMap<Address, (usize, Rc<RefCell<Socket>>)>,
    pub(crate) tcp_bindings: BTreeMap<Address, (usize, Rc<RefCell<Socket>>)>,
    pub(crate) futexes: BTreeMap<(usize, u64), VecDeque<u64>>,
    event_sequence: u64,
    wait_sequence: u64,
    tid_sequence: u64,
}

impl Simulation {
    pub fn new(
        specs: Vec<NodeSpec>,
        seed: u64,
        config: RunConfig,
        instruction_trace: bool,
    ) -> Result<Self> {
        config::validate(&config, &specs)?;
        let manifest = json!({
            "runtime": env!("CARGO_PKG_VERSION"), "backend": "unicorn-x86_64-rust-v1", "runtime_sha256": env!("SEEDBOX_SOURCE_SHA256"),
            "instruction_trace": instruction_trace, "unicorn": env!("SEEDBOX_UNICORN_VERSION"), "unicorn_bindings": "2.1.5", "decoder": "iced-x86-1.21.0", "rustc": env!("SEEDBOX_RUSTC_VERSION"),
            "config": config.fingerprint(), "nodes": specs.iter().map(|s| json!({"name": s.name, "ip": s.ip.to_string(), "argv": s.argv, "image_sha256": digest(&s.image),
                "files": s.files.iter().map(|(path, data)| (path.clone(), Value::String(hex::encode(data)))).collect::<BTreeMap<_,_>>() })).collect::<Vec<_>>()
        });
        let mut sim = Self {
            config,
            seed,
            instruction_trace,
            now: 0,
            instructions: 0,
            nodes: vec![],
            trace: vec![],
            status: "ready".into(),
            error: None,
            manifest,
            instruction_hash: Sha256::new(),
            streams: BTreeMap::new(),
            ip_names: BTreeMap::new(),
            waits: BTreeMap::new(),
            events: BTreeMap::new(),
            bindings: BTreeMap::new(),
            tcp_bindings: BTreeMap::new(),
            futexes: BTreeMap::new(),
            event_sequence: 0,
            wait_sequence: 0,
            tid_sequence: 0,
        };
        for spec in specs {
            sim.tid_sequence += 1;
            let pid = sim.tid_sequence;
            let process = Process::new(
                &spec.name,
                &spec.image,
                sim.stream(&format!("entropy:{}", spec.name)),
                &spec.argv,
                pid,
            )?;
            let mut files = BTreeMap::new();
            for (name, data) in spec.files {
                if files
                    .insert(
                        path(&name),
                        File {
                            contents: data.clone(),
                            durable: data,
                        },
                    )
                    .is_some()
                {
                    return Err(Error::InvalidInput(
                        "initial file paths normalize to the same path".into(),
                    ));
                }
            }
            sim.ip_names.insert(spec.ip, sim.nodes.len());
            sim.emit(
                "start",
                json!({"node": spec.name, "tid": pid, "ip": spec.ip.to_string()}),
            )?;
            sim.nodes.push(Node {
                process,
                ip: spec.ip,
                files,
                descriptors: BTreeMap::new(),
                stdout: vec![],
                stderr: vec![],
                cwd: "/".into(),
                signal_actions: BTreeMap::new(),
                cpu_ns: 0,
                umask: 0o022,
            });
        }
        Ok(sim)
    }
    pub(crate) fn stream(&mut self, domain: &str) -> &mut RandomStream {
        self.streams
            .entry(domain.into())
            .or_insert_with(|| RandomStream::new(self.seed, domain))
    }
    pub(crate) fn emit(&mut self, event: &str, data: Value) -> Result<()> {
        if self.trace.len() >= 1_000_000 {
            return Err(Error::Invariant("trace event limit exceeded".into()));
        }
        let mut entry = json!({"index": self.trace.len(), "time_ns": self.now, "event": event});
        entry.as_object_mut().unwrap().extend(
            data.as_object()
                .ok_or_else(|| Error::Invariant("trace data must be an object".into()))?
                .clone(),
        );
        self.trace.push(entry);
        Ok(())
    }
    pub(crate) fn deadline(&self, delay: u64) -> Result<u64> {
        self.now
            .checked_add(delay)
            .filter(|d| *d < (1u64 << 63))
            .ok_or_else(|| Error::Invariant("event deadline outside supported virtual time".into()))
    }
    pub(crate) fn schedule(&mut self, deadline: u64, event: Event) -> Result<()> {
        if deadline < self.now || deadline >= (1u64 << 63) {
            return Err(Error::Invariant(
                "event deadline outside supported virtual time".into(),
            ));
        }
        self.event_sequence += 1;
        self.events.insert((deadline, self.event_sequence), event);
        Ok(())
    }
    fn block(
        &mut self,
        node: usize,
        tid: u64,
        req: SyscallRequest,
        kind: WaitKind,
        deadline: Option<u64>,
    ) -> Result<()> {
        self.wait_sequence += 1;
        let id = self.wait_sequence;
        let thread = self.nodes[node].process.threads.get_mut(&tid).unwrap();
        thread.state = State::Blocked;
        thread.wait_id = Some(id);
        match &kind {
            WaitKind::Futex { address } => self
                .futexes
                .entry((node, *address))
                .or_default()
                .push_back(id),
            WaitKind::Socket(data) => data.socket.borrow_mut().waiters.push_back(id),
            _ => {}
        }
        self.emit("block", json!({"node": self.nodes[node].process.name, "tid": tid, "wait": id, "kind": kind.as_str(), "deadline_ns": deadline}))?;
        self.waits.insert(
            id,
            Wait {
                id,
                node,
                tid,
                request: req,
                kind,
            },
        );
        if let Some(deadline) = deadline {
            self.schedule(deadline, Event::Completion(id))?;
        }
        Ok(())
    }
    fn complete(&mut self, node: usize, tid: u64, req: &SyscallRequest, value: i64) -> Result<()> {
        self.nodes[node].process.complete(tid, req, value)?;
        self.emit("complete", json!({"node": self.nodes[node].process.name, "tid": tid, "syscall": req.number, "result": value}))
    }
    pub(crate) fn wake(&mut self, id: u64, value: i64) -> Result<bool> {
        let Some(wait) = self.waits.remove(&id) else {
            return Ok(false);
        };
        let thread = &self.nodes[wait.node].process.threads[&wait.tid];
        if thread.state != State::Blocked || thread.wait_id != Some(id) {
            return Err(Error::Invariant("wait/thread state inconsistent".into()));
        }
        self.emit("wake", json!({"node": self.nodes[wait.node].process.name, "tid": wait.tid, "wait": wait.id, "kind": wait.kind.as_str()}))?;
        self.complete(wait.node, wait.tid, &wait.request, value)?;
        Ok(true)
    }
    fn exit(&mut self, node: usize, tid: u64, code: u64, group: bool) -> Result<()> {
        let tids: Vec<_> = if group {
            self.nodes[node].process.threads.keys().copied().collect()
        } else {
            vec![tid]
        };
        for tid in tids {
            let thread = self.nodes[node].process.threads.get_mut(&tid).unwrap();
            if thread.state == State::Exited {
                continue;
            }
            if let Some(id) = thread.wait_id.take() {
                self.waits.remove(&id);
            }
            thread.state = State::Exited;
            thread.exit_code = Some(code as u8);
            let clear_tid = thread.clear_tid;
            self.emit(
                "exit",
                json!({"node": self.nodes[node].process.name, "tid": tid, "code": code as u8}),
            )?;
            if clear_tid != 0 {
                self.nodes[node].process.write(clear_tid, &[0; 4])?;
                self.wake_futex(node, clear_tid, 1)?;
            }
        }
        // Linux closes every descriptor when the last thread exits. This is
        // also how peers observe EOF after an application's exit_group.
        if self.nodes[node]
            .process
            .threads
            .values()
            .all(|t| t.state == State::Exited)
        {
            let fds: Vec<_> = self.nodes[node].descriptors.keys().copied().collect();
            for fd in fds {
                if matches!(self.nodes[node].descriptors.get(&fd),Some(Descriptor::Socket(s)) if s.borrow().tcp.is_some())
                {
                    self.close_tcp(node, fd)?;
                } else if let Some(Descriptor::Socket(s)) = self.nodes[node].descriptors.remove(&fd)
                {
                    if let Some(bound) = s.borrow().bound {
                        self.bindings.remove(&bound);
                    }
                } else {
                    self.nodes[node].descriptors.remove(&fd);
                }
            }
        }
        Ok(())
    }
    fn clone_thread(&mut self, node: usize, tid: u64, req: &SyscallRequest) -> Result<()> {
        let [flags, stack, parent_tid, child_tid, tls, _] = req.args;
        let required = 0x50F00;
        // CLONE_DETACHED is an obsolete Linux flag still passed by musl.
        let allowed = required | 0x80000 | 0x100000 | 0x200000 | 0x400000 | 0x1000000;
        if flags & required != required || flags & !allowed != 0 {
            return Err(Error::Unsupported(
                "unsupported clone flags; only shared-address-space threads are supported".into(),
            ));
        }
        if self.nodes[node].process.threads.len() >= 256 {
            return Err(Error::Invariant("guest thread limit exceeded".into()));
        }
        let check = (|| -> Result<()> {
            let process = &self.nodes[node].process;
            process.check_memory(
                stack
                    .checked_sub(8)
                    .ok_or_else(|| Error::Memory("invalid child stack".into()))?,
                8,
                Prot::WRITE,
            )?;
            if flags & 0x100000 != 0 {
                process.check_memory(parent_tid, 4, Prot::WRITE)?;
            }
            if flags & (0x200000 | 0x1000000) != 0 {
                process.check_memory(child_tid, 4, Prot::WRITE)?;
            }
            Ok(())
        })();
        if let Err(Error::Memory(_)) = check {
            return self.complete(node, tid, req, -errno::EFAULT);
        }
        check?;
        self.tid_sequence += 1;
        let id = self.tid_sequence;
        if flags & 0x100000 != 0 {
            self.nodes[node]
                .process
                .write(parent_tid, &(id as u32).to_le_bytes())?;
        }
        if flags & 0x1000000 != 0 {
            self.nodes[node]
                .process
                .write(child_tid, &(id as u32).to_le_bytes())?;
        }
        self.complete(node, tid, req, id as i64)?;
        self.nodes[node].process.clone_thread(
            tid,
            id,
            stack,
            if flags & 0x80000 != 0 {
                Some(tls)
            } else {
                None
            },
        )?;
        if flags & 0x200000 != 0 {
            self.nodes[node]
                .process
                .threads
                .get_mut(&id)
                .unwrap()
                .clear_tid = child_tid;
        }
        self.emit(
            "spawn",
            json!({"node": self.nodes[node].process.name, "parent": tid, "tid": id}),
        )
    }
    pub(crate) fn partitioned(&self, source: usize, destination: usize) -> bool {
        self.config.network.partitions.iter().any(|p| {
            p.source == self.nodes[source].process.name
                && p.destination == self.nodes[destination].process.name
                && p.start_ns <= self.now
                && self.now < p.end_ns
        })
    }
    pub(crate) fn send_packet(
        &mut self,
        node: usize,
        source: Address,
        destination: Address,
        payload: Vec<u8>,
    ) -> Result<()> {
        self.emit("packet_send", json!({"node": self.nodes[node].process.name, "source": source.value(), "destination": destination.value(), "data": hex::encode(&payload)}))?;
        let Some(&destination_node) = self.ip_names.get(&destination.ip) else {
            return self.emit(
                "packet_drop",
                json!({"reason": "outside_world", "destination": destination.value()}),
            );
        };
        let cfg = self.config.network.clone();
        let domain = format!(
            "network:{}",
            String::from_utf8(canonical(&json!([
                self.nodes[node].process.name,
                self.nodes[destination_node].process.name
            ])))
            .unwrap()
        );
        let rng = self.stream(&domain);
        let loss = rng.below(1_000_000);
        let duplicate = rng.below(1_000_000);
        let jitter = rng.below(2 * cfg.jitter_ns + 1) as i64 - cfg.jitter_ns as i64;
        if self.partitioned(node, destination_node) || loss < cfg.loss_ppm {
            return self.emit("packet_drop", json!({"reason": if self.partitioned(node, destination_node) { "partition" } else { "loss" }, "destination": destination.value()}));
        }
        let delay = (cfg.latency_ns as i64 + jitter).max(0) as u64;
        let deadline = self.deadline(delay)?;
        self.schedule(
            deadline,
            Event::Packet {
                source_node: node,
                destination_node,
                source,
                destination,
                payload: payload.clone(),
            },
        )?;
        if duplicate < cfg.duplicate_ppm {
            self.schedule(
                self.deadline(delay + 1)?,
                Event::Packet {
                    source_node: node,
                    destination_node,
                    source,
                    destination,
                    payload,
                },
            )?;
        }
        Ok(())
    }
    fn discard_cancelled(&mut self) {
        loop {
            let stale = self.events.first_key_value().is_some_and(
                |(_, e)| matches!(e, Event::Completion(id) if !self.waits.contains_key(id)),
            );
            if !stale {
                break;
            }
            self.events.pop_first();
        }
    }
    fn deliver_due(&mut self) -> Result<()> {
        while self
            .events
            .first_key_value()
            .is_some_and(|((deadline, _), _)| *deadline <= self.now)
        {
            let ((deadline, sequence), event) = self.events.pop_first().unwrap();
            if deadline != self.now {
                return Err(Error::Invariant(
                    "execution overran an event deadline".into(),
                ));
            }
            match event {
                Event::Stream {
                    source_node,
                    destination_node,
                    socket,
                    payload,
                    eof,
                    reset,
                } => {
                    let reset = reset || self.partitioned(source_node, destination_node);
                    self.deliver_stream(destination_node, &socket, payload, eof, reset)?;
                }
                Event::Packet {
                    source_node,
                    destination_node,
                    source,
                    destination,
                    payload,
                } => {
                    if self.partitioned(source_node, destination_node) {
                        self.emit(
                            "packet_drop",
                            json!({"reason": "partition", "destination": destination.value()}),
                        )?;
                    } else {
                        self.emit("packet_deliver", json!({"sequence": sequence, "destination": destination.value(), "data": hex::encode(&payload)}))?;
                        self.deliver_packet(destination, source, payload)?;
                    }
                }
                Event::Completion(id) => {
                    let Some(wait) = self.waits.get(&id) else {
                        continue;
                    };
                    let node = wait.node;
                    let kind = wait.kind.clone();
                    let value = match kind {
                        WaitKind::Disk { handle, operation } => {
                            match self.finish_disk(node, &handle, &operation) {
                                Ok(value) => value,
                                Err(Error::Memory(_)) => -errno::EFAULT,
                                Err(e) => return Err(e),
                            }
                        }
                        WaitKind::Futex { .. } => -errno::ETIMEDOUT,
                        WaitKind::Sleep => 0,
                        WaitKind::Ready(_) => 0,
                        WaitKind::Accept { .. } => {
                            return Err(Error::Invariant("unexpected timed accept".into()));
                        }
                        WaitKind::Socket(_) => {
                            return Err(Error::Invariant("unexpected timed wait".into()));
                        }
                    };
                    self.wake(id, value)?;
                }
            }
        }
        Ok(())
    }
    pub fn run(&mut self) -> Result<Value> {
        if self.status != "ready" {
            return Err(Error::InvalidInput(
                "create a fresh simulation for every run".into(),
            ));
        }
        self.status = "running".into();
        if let Err(error) = self.run_loop() {
            self.status = if matches!(error, Error::Unsupported(_)) {
                "unsupported"
            } else {
                "simulator_error"
            }
            .into();
            self.error = Some(error.to_string());
        }
        Ok(self.report())
    }
    fn run_loop(&mut self) -> Result<()> {
        loop {
            self.discard_cancelled();
            self.deliver_due()?;
            if self
                .nodes
                .iter()
                .flat_map(|n| n.process.threads.values())
                .all(|t| t.state == State::Exited)
            {
                self.status = if self
                    .nodes
                    .iter()
                    .flat_map(|n| n.process.threads.values())
                    .all(|t| t.exit_code == Some(0))
                {
                    "completed"
                } else {
                    "guest_failure"
                }
                .into();
                break;
            }
            let mut runnable: Vec<_> = self
                .nodes
                .iter()
                .enumerate()
                .flat_map(|(n, node)| {
                    node.process
                        .threads
                        .values()
                        .filter(|t| t.state == State::Runnable)
                        .map(move |t| (n, t.tid))
                })
                .collect();
            runnable.sort_by_key(|(_, tid)| *tid);
            if runnable.is_empty() {
                if self.events.is_empty() {
                    self.status = "deadlock".into();
                    self.error = Some("all live threads are blocked with no future event".into());
                    let blocked: Vec<_> = self.waits.values().map(|w| json!({"node": self.nodes[w.node].process.name, "tid": w.tid, "kind": w.kind.as_str()})).collect();
                    self.emit("deadlock", json!({"blocked": blocked}))?;
                    break;
                }
                let next = self.events.first_key_value().unwrap().0.0;
                self.emit("advance", json!({"deadline_ns": next}))?;
                self.now = next;
                continue;
            }
            let c = self.config.scheduler.clone();
            let remaining = c.max_instructions - self.instructions;
            if remaining == 0 {
                self.status = "instruction_limit".into();
                self.error = Some("guest instruction limit reached".into());
                self.emit(
                    "instruction_limit",
                    json!({"instructions": self.instructions}),
                )?;
                break;
            }
            let next = self.events.first_key_value().map(|((d, _), _)| *d);
            if let Some(next) = next {
                if next - self.now < c.tick_ns {
                    self.now = next;
                    continue;
                }
            }
            let (node, tid) =
                runnable[self.stream("schedule").below(runnable.len() as u64) as usize];
            let quantum = c.quantum_min
                + self
                    .stream("schedule")
                    .below(c.quantum_max - c.quantum_min + 1);
            let mut budget = quantum.min(remaining);
            if let Some(next) = next {
                budget = budget.min((next - self.now) / c.tick_ns);
            }
            self.emit(
                "dispatch",
                json!({"node": self.nodes[node].process.name, "tid": tid, "budget": budget}),
            )?;
            let stop = self.nodes[node].process.run(tid, budget)?;
            if stop.steps > budget
                || stop.entered.len() as u64 != stop.steps
                || stop.steps == 0
                    && !matches!(stop.reason, StopReason::Fault | StopReason::Unsupported)
            {
                return Err(Error::Invariant(
                    "backend violated its instruction budget".into(),
                ));
            }
            for (i, entered) in stop.entered.iter().enumerate() {
                let name = self.nodes[node].process.name.as_bytes();
                self.instruction_hash
                    .update((name.len() as u32).to_le_bytes());
                self.instruction_hash.update(name);
                self.instruction_hash.update(tid.to_le_bytes());
                self.instruction_hash.update(entered.pc.to_le_bytes());
                self.instruction_hash
                    .update((entered.bytes.len() as u32).to_le_bytes());
                self.instruction_hash.update(&entered.bytes);
                if self.instruction_trace {
                    self.emit("instruction", json!({"node": self.nodes[node].process.name, "tid": tid, "pc": entered.pc, "bytes": hex::encode(&entered.bytes), "time_ns": self.now + i as u64 * c.tick_ns}))?;
                }
            }
            self.instructions += stop.steps;
            self.nodes[node].cpu_ns += stop.steps * c.tick_ns;
            self.now += stop.steps * c.tick_ns;
            self.emit("stop", json!({"node": self.nodes[node].process.name, "tid": tid, "reason": stop.reason.as_str(), "instructions": stop.steps}))?;
            if matches!(stop.reason, StopReason::Fault | StopReason::Unsupported) {
                self.status = stop.reason.as_str().into();
                self.error = stop.detail;
                break;
            }
            if stop.reason == StopReason::Syscall {
                let req = stop
                    .request
                    .ok_or_else(|| Error::Invariant("missing syscall request".into()))?;
                self.emit("syscall", json!({"node": self.nodes[node].process.name, "tid": tid, "number": req.number, "args": req.args}))?;
                match req.number {
                    60 | 231 => self.exit(node, tid, req.args[0], req.number == 231)?,
                    56 => self.clone_thread(node, tid, &req)?,
                    _ => match self.syscall(node, tid, &req)? {
                        Action::Complete(value) => self.complete(node, tid, &req, value)?,
                        Action::Blocked { kind, deadline } => {
                            self.block(node, tid, req, kind, deadline)?
                        }
                    },
                }
            }
        }
        Ok(())
    }
    pub fn report(&self) -> Value {
        let nodes: BTreeMap<_, _> = self.nodes.iter().map(|node| (node.process.name.clone(), json!({
            "stdout_hex": hex::encode(&node.stdout), "stderr_hex": hex::encode(&node.stderr),
            "threads": node.process.threads.iter().map(|(id, t)| (id.to_string(), json!({"state": t.state.as_str(), "exit_code": t.exit_code}))).collect::<BTreeMap<_,_>>(),
            "files": node.files.iter().map(|(path, f)| (path.clone(), json!({"contents_hex": hex::encode(&f.contents), "durable_hex": hex::encode(&f.durable)}))).collect::<BTreeMap<_,_>>()
        }))).collect();
        json!({ "schema": 1, "seed": self.seed, "manifest": self.manifest, "manifest_sha256": digest(&canonical(&self.manifest)), "status": self.status, "error": self.error,
            "virtual_time_ns": self.now, "instructions": self.instructions, "instruction_sha256": hex::encode(self.instruction_hash.clone().finalize()), "trace_sha256": digest(&canonical(&json!(self.trace))), "nodes": nodes, "trace": self.trace })
    }
}
