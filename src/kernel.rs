//! In-memory Linux ABI resource models. No host file or socket operations.
use crate::{
    backend::{MAX_IO, Process, State, SyscallRequest},
    error::{Error, Result},
    linux_errno as errno,
    runtime::Simulation,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    net::Ipv4Addr,
    rc::{Rc, Weak},
};
use unicorn_engine::Prot;

pub const MAX_FILE: usize = 32 * 1024 * 1024;
#[derive(Clone)]
pub struct File {
    pub contents: Vec<u8>,
    pub durable: Vec<u8>,
}
pub struct FileHandle {
    pub path: String,
    pub offset: usize,
    pub flags: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Address {
    pub ip: Ipv4Addr,
    pub port: u16,
}
impl Address {
    pub fn value(self) -> Value {
        json!([self.ip.to_string(), self.port])
    }
}
pub struct Socket {
    pub nonblocking: bool,
    pub bound: Option<Address>,
    pub peer: Option<Address>,
    pub packets: VecDeque<(Address, Vec<u8>)>,
    pub waiters: VecDeque<u64>,
    pub tcp: Option<Tcp>,
}
pub struct Tcp {
    pub listening: bool,
    pub backlog: usize,
    pub accepts: VecDeque<Rc<RefCell<Socket>>>,
    pub peer: Option<Weak<RefCell<Socket>>>,
    pub peer_node: usize,
    pub bytes: VecDeque<u8>,
    pub eof: bool,
    pub reset: bool,
    pub next_delivery: u64,
    pub local: bool,
    pub access: u8,
}
impl Default for Tcp {
    fn default() -> Self {
        Self {
            listening: false,
            backlog: 128,
            accepts: VecDeque::new(),
            peer: None,
            peer_node: 0,
            bytes: VecDeque::new(),
            eof: false,
            reset: false,
            next_delivery: 0,
            local: false,
            access: 3,
        }
    }
}
pub struct EpollInterest {
    pub events: u32,
    pub data: u64,
}
pub struct Epoll {
    pub interests: BTreeMap<u64, EpollInterest>,
}
#[derive(Clone)]
pub enum Descriptor {
    File(Rc<RefCell<FileHandle>>),
    Socket(Rc<RefCell<Socket>>),
    Entropy,
    Epoll(Rc<RefCell<Epoll>>),
}
pub struct Node {
    pub process: Process,
    pub ip: Ipv4Addr,
    pub files: BTreeMap<String, File>,
    pub descriptors: BTreeMap<u64, Descriptor>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cwd: String,
    pub signal_actions: BTreeMap<u64, Vec<u8>>,
    pub cpu_ns: u64,
    pub umask: u64,
}
impl Node {
    pub fn allocate(&mut self, resource: Descriptor) -> Result<i64> {
        let fd = (3..1024)
            .find(|fd| !self.descriptors.contains_key(fd))
            .ok_or_else(|| Error::Invariant("guest descriptor limit exceeded".into()))?;
        self.descriptors.insert(fd, resource);
        Ok(fd as i64)
    }
}
#[derive(Clone)]
pub enum DiskOperation {
    Read { address: u64, length: u64 },
    Write(Vec<u8>),
    Fsync,
    ReadVector(Vec<(u64, u64)>),
}
impl DiskOperation {
    pub fn number(&self) -> u64 {
        match self {
            Self::Read { .. } => 0,
            Self::Write(_) => 1,
            Self::Fsync => 74,
            Self::ReadVector(_) => 19,
        }
    }
}
#[derive(Clone)]
pub struct Receive {
    pub socket: Rc<RefCell<Socket>>,
    pub address: u64,
    pub length: u64,
    pub peer: u64,
    pub peer_length: u64,
}
#[derive(Clone)]
pub enum WaitKind {
    Sleep,
    Futex {
        address: u64,
    },
    Disk {
        handle: Rc<RefCell<FileHandle>>,
        operation: DiskOperation,
    },
    Socket(Receive),
    Accept {
        socket: Rc<RefCell<Socket>>,
        address: u64,
        length: u64,
        nonblocking: bool,
    },
    Ready(crate::network::ReadyWait),
}
impl WaitKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Futex { .. } => "futex",
            Self::Disk { .. } => "disk",
            Self::Socket(_) => "socket",
            Self::Accept { .. } => "accept",
            Self::Ready(_) => "readiness",
        }
    }
}
pub enum Action {
    Complete(i64),
    Blocked {
        kind: WaitKind,
        deadline: Option<u64>,
    },
}

pub fn path(input: &str) -> String {
    let mut parts = Vec::new();
    for part in input.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

impl Simulation {
    pub(crate) fn syscall(
        &mut self,
        node: usize,
        tid: u64,
        req: &SyscallRequest,
    ) -> Result<Action> {
        let outcome = self.handle_syscall(node, tid, req);
        match outcome {
            Err(Error::Memory(_)) => Ok(Action::Complete(-errno::EFAULT)),
            other => other,
        }
    }
    fn handle_syscall(&mut self, node: usize, tid: u64, req: &SyscallRequest) -> Result<Action> {
        use Action::Complete;
        let a = req.args;
        match req.number {
            39 => Ok(Complete(self.nodes[node].process.pid as i64)),
            186 => Ok(Complete(tid as i64)),
            24 => Ok(Complete(0)),
            218 => {
                if a[0] != 0 {
                    self.nodes[node]
                        .process
                        .check_memory(a[0], 4, Prot::WRITE)?;
                }
                self.nodes[node]
                    .process
                    .threads
                    .get_mut(&tid)
                    .unwrap()
                    .clear_tid = a[0];
                Ok(Complete(tid as i64))
            }
            228 => {
                if a[0] > 1 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let now = self.now + if a[0] == 0 { self.config.epoch_ns } else { 0 };
                let data: Vec<_> = [(now / 1_000_000_000) as i64, (now % 1_000_000_000) as i64]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect();
                self.nodes[node].process.write(a[1], &data)?;
                Ok(Complete(0))
            }
            318 => {
                if a[2] & !3 != 0 || a[1] > MAX_IO {
                    return Ok(Complete(-errno::EINVAL));
                }
                self.nodes[node]
                    .process
                    .check_memory(a[0], a[1], Prot::WRITE)?;
                let name = self.nodes[node].process.name.clone();
                let data = self.stream(&format!("entropy:{name}")).read(a[1] as usize);
                self.nodes[node].process.write(a[0], &data)?;
                Ok(Complete(data.len() as i64))
            }
            35 => {
                let delay = match timespec(&self.nodes[node].process.read(a[0], 16)?) {
                    Some(value) => value,
                    None => return Ok(Complete(-errno::EINVAL)),
                };
                if a[1] != 0 {
                    self.nodes[node]
                        .process
                        .check_memory(a[1], 16, Prot::WRITE)?;
                }
                Ok(Action::Blocked {
                    kind: WaitKind::Sleep,
                    deadline: Some(self.deadline(delay)?),
                })
            }
            202 => self.futex(node, req),
            2 | 257 => {
                let (directory, name, flags) = if req.number == 2 {
                    ((-100i64) as u64, a[0], a[1])
                } else {
                    (a[0], a[1], a[2])
                };
                if directory != (-100i64) as u64 {
                    return Ok(Complete(-errno::EBADF));
                }
                let name = self.nodes[node].process.cstring(name)?;
                let path = self.absolute_path(node, &name);
                if flags & !(3 | 0x40 | 0x80 | 0x200 | 0x400 | 0x800 | 0x8000 | 0x80000) != 0 {
                    return Err(Error::Unsupported(format!(
                        "unsupported open flags {flags:#x} for {path}"
                    )));
                }
                if flags & 3 == 3 {
                    return Ok(Complete(-errno::EINVAL));
                }
                if flags & 0x200 != 0 && flags & 3 == 0 {
                    return Ok(Complete(-errno::EACCES));
                }
                let n = &mut self.nodes[node];
                if path == "/dev/urandom" || path == "/dev/random" {
                    if flags & 3 != 0 {
                        return Ok(Complete(-errno::EACCES));
                    }
                    return Ok(Complete(n.allocate(Descriptor::Entropy)?));
                }
                if n.files.contains_key(&path) && flags & 0x40 != 0 && flags & 0x80 != 0 {
                    return Ok(Complete(-errno::EEXIST));
                }
                if !n.files.contains_key(&path) {
                    if flags & 0x40 == 0 {
                        return Ok(Complete(-errno::ENOENT));
                    }
                    n.files.insert(
                        path.clone(),
                        File {
                            contents: vec![],
                            durable: vec![],
                        },
                    );
                }
                if flags & 0x200 != 0 {
                    n.files.get_mut(&path).unwrap().contents.clear();
                }
                Ok(Complete(n.allocate(Descriptor::File(Rc::new(
                    RefCell::new(FileHandle {
                        path,
                        offset: 0,
                        flags,
                    }),
                )))?))
            }
            3 => {
                if matches!(self.nodes[node].descriptors.get(&a[0]), Some(Descriptor::Socket(s)) if s.borrow().tcp.is_some())
                {
                    return self.close_tcp(node, a[0]);
                }
                let Some(resource) = self.nodes[node].descriptors.remove(&a[0]) else {
                    return Ok(Complete(-errno::EBADF));
                };
                self.remove_epoll_fd(node, a[0]);
                if let Descriptor::Socket(socket) = resource {
                    let bound = socket.borrow().bound;
                    if let Some(bound) = bound {
                        self.bindings.remove(&bound);
                    }
                    let waits: Vec<_> = socket.borrow_mut().waiters.drain(..).collect();
                    for wait in waits {
                        self.wake(wait, -errno::EBADF)?;
                    }
                }
                Ok(Complete(0))
            }
            8 => {
                let handle = match self.nodes[node].descriptors.get(&a[0]) {
                    Some(Descriptor::File(h)) => h.clone(),
                    Some(Descriptor::Socket(_)) => return Ok(Complete(-errno::ESPIPE)),
                    Some(Descriptor::Entropy) => return Ok(Complete(-errno::ESPIPE)),
                    Some(Descriptor::Epoll(_)) => return Ok(Complete(-errno::ESPIPE)),
                    None => return Ok(Complete(-errno::EBADF)),
                };
                if a[2] > 2 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let mut handle = handle.borrow_mut();
                let base = match a[2] {
                    0 => 0,
                    1 => handle.offset,
                    _ => self.nodes[node].files[&handle.path].contents.len(),
                };
                let end = base as i128 + a[1] as i64 as i128;
                if !(0..=MAX_FILE as i128).contains(&end) {
                    return Ok(Complete(-errno::EINVAL));
                }
                handle.offset = end as usize;
                Ok(Complete(end as i64))
            }
            0 | 1 | 74 => self.file_operation(node, req),
            41 => {
                if a[0] == 2 && a[1] & !0x80800 == 1 && [0, 6].contains(&a[2]) {
                    return self.new_tcp(node, a[1] & 0x800 != 0);
                }
                if a[0] != 2 {
                    return Ok(Complete(-errno::EAFNOSUPPORT));
                }
                if a[1] & !0x80800 != 2 || ![0, 17].contains(&a[2]) {
                    return Err(Error::Unsupported(
                        "only IPv4 UDP sockets are supported".into(),
                    ));
                }
                let socket = Socket {
                    nonblocking: a[1] & 0x800 != 0,
                    bound: None,
                    peer: None,
                    packets: VecDeque::new(),
                    waiters: VecDeque::new(),
                    tcp: None,
                };
                Ok(Complete(self.nodes[node].allocate(Descriptor::Socket(
                    Rc::new(RefCell::new(socket)),
                ))?))
            }
            42 | 49 => {
                if a[2] < 16 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let Some(socket) = self.socket(node, a[0]) else {
                    return Ok(Complete(-errno::ENOTSOCK));
                };
                if socket.borrow().tcp.is_some() {
                    return self.tcp_address_call(node, req, &socket);
                }
                let target = sockaddr(&self.nodes[node].process, a[1], a[2])?;
                if req.number == 42 {
                    socket.borrow_mut().peer = Some(target);
                    if socket.borrow().bound.is_none() {
                        let ip = self.nodes[node].ip;
                        self.bind(node, &socket, Address { ip, port: 0 });
                    }
                    Ok(Complete(0))
                } else {
                    Ok(Complete(self.bind(node, &socket, target)))
                }
            }
            44 => self.send(node, req, false),
            45 => self.receive(node, req, false),
            _ => self.linux_syscall(node, tid, req),
        }
    }
    fn file_operation(&mut self, node: usize, req: &SyscallRequest) -> Result<Action> {
        let a = req.args;
        let n = req.number;
        if n == 0 && a[0] == 0 {
            return Ok(Action::Complete(0));
        }
        if n == 1 && [1, 2].contains(&a[0]) {
            let data = self.nodes[node].process.read(a[1], a[2])?;
            let output = if a[0] == 1 {
                &mut self.nodes[node].stdout
            } else {
                &mut self.nodes[node].stderr
            };
            if output.len() + data.len() > MAX_FILE {
                return Err(Error::Invariant("guest output limit exceeded".into()));
            }
            output.extend(&data);
            self.emit("output", json!({"node": self.nodes[node].process.name, "fd": a[0], "data": hex::encode(data)}))?;
            return Ok(Action::Complete(a[2] as i64));
        }
        let resource = self.nodes[node].descriptors.get(&a[0]).cloned();
        let handle = match resource {
            Some(Descriptor::File(handle)) => handle,
            Some(Descriptor::Socket(_)) => {
                return match n {
                    0 => self.receive(node, req, true),
                    1 => self.send(node, req, true),
                    _ => Ok(Action::Complete(-errno::EINVAL)),
                };
            }
            Some(Descriptor::Entropy) => {
                if n != 0 {
                    return Ok(Action::Complete(-errno::EBADF));
                }
                self.nodes[node]
                    .process
                    .check_memory(a[1], a[2], Prot::WRITE)?;
                let name = self.nodes[node].process.name.clone();
                let bytes = self.stream(&format!("entropy:{name}")).read(a[2] as usize);
                self.nodes[node].process.write(a[1], &bytes)?;
                return Ok(Action::Complete(a[2] as i64));
            }
            Some(Descriptor::Epoll(_)) => return Ok(Action::Complete(-errno::EINVAL)),
            None => return Ok(Action::Complete(-errno::EBADF)),
        };
        let flags = handle.borrow().flags;
        if n == 0 && flags & 3 == 1 || n == 1 && flags & 3 == 0 {
            return Ok(Action::Complete(-errno::EBADF));
        }
        if [0, 1].contains(&n) && a[2] == 0 {
            return Ok(Action::Complete(0));
        }
        let operation = match n {
            0 => {
                self.nodes[node]
                    .process
                    .check_memory(a[1], a[2], Prot::WRITE)?;
                DiskOperation::Read {
                    address: a[1],
                    length: a[2],
                }
            }
            1 => DiskOperation::Write(self.nodes[node].process.read(a[1], a[2])?),
            _ => DiskOperation::Fsync,
        };
        Ok(Action::Blocked {
            kind: WaitKind::Disk { handle, operation },
            deadline: Some(self.deadline(self.config.disk.latency_ns)?),
        })
    }
    pub(crate) fn finish_disk(
        &mut self,
        node: usize,
        handle: &Rc<RefCell<FileHandle>>,
        operation: &DiskOperation,
    ) -> Result<i64> {
        let name = self.nodes[node].process.name.clone();
        let fault_rate = self.config.disk.error_ppm;
        if self.stream(&format!("disk:{name}")).below(1_000_000) < fault_rate {
            self.emit(
                "disk_fault",
                json!({"node": name, "op": operation.number()}),
            )?;
            return Ok(-errno::EIO);
        }
        let mut handle = handle.borrow_mut();
        match operation {
            DiskOperation::ReadVector(vectors) => {
                for (address, length) in vectors {
                    self.nodes[node]
                        .process
                        .check_memory(*address, *length, Prot::WRITE)?;
                }
                let contents = &self.nodes[node].files[&handle.path].contents;
                let start = handle.offset.min(contents.len());
                let size = vectors.iter().map(|(_, length)| *length).sum::<u64>() as usize;
                let end = (start + size).min(contents.len());
                let bytes = contents[start..end].to_vec();
                let mut offset = 0;
                for (address, length) in vectors {
                    let stop = (offset + *length as usize).min(bytes.len());
                    self.nodes[node]
                        .process
                        .write(*address, &bytes[offset..stop])?;
                    offset = stop;
                }
                handle.offset += bytes.len();
                Ok(bytes.len() as i64)
            }
            DiskOperation::Read { address, length } => {
                let contents = &self.nodes[node].files[&handle.path].contents;
                let start = handle.offset.min(contents.len());
                let end = (handle.offset + *length as usize).min(contents.len());
                let bytes = contents[start..end].to_vec();
                self.nodes[node].process.write(*address, &bytes)?;
                handle.offset += bytes.len();
                Ok(bytes.len() as i64)
            }
            DiskOperation::Write(bytes) => {
                let file = self.nodes[node].files.get_mut(&handle.path).unwrap();
                let offset = if handle.flags & 0x400 != 0 {
                    file.contents.len()
                } else {
                    handle.offset
                };
                let end = offset + bytes.len();
                if end > MAX_FILE {
                    return Ok(-errno::EFBIG);
                }
                if end > file.contents.len() {
                    file.contents.resize(end, 0);
                }
                file.contents[offset..end].copy_from_slice(bytes);
                handle.offset = end;
                Ok(bytes.len() as i64)
            }
            DiskOperation::Fsync => {
                let file = self.nodes[node].files.get_mut(&handle.path).unwrap();
                file.durable.clone_from(&file.contents);
                Ok(0)
            }
        }
    }
    fn futex(&mut self, node: usize, req: &SyscallRequest) -> Result<Action> {
        let [address, operation, value, timeout, _, _] = req.args;
        if address % 4 != 0 {
            return Ok(Action::Complete(-errno::EINVAL));
        }
        if ![0, 1, 128, 129].contains(&operation) {
            return Err(Error::Unsupported("unsupported futex operation".into()));
        }
        let current = u32::from_le_bytes(
            self.nodes[node]
                .process
                .read(address, 4)?
                .try_into()
                .unwrap(),
        );
        if operation & 127 == 1 {
            return Ok(Action::Complete(self.wake_futex(node, address, value)?));
        }
        if current != value as u32 {
            return Ok(Action::Complete(-errno::EAGAIN));
        }
        let deadline = if timeout != 0 {
            let Some(delay) = timespec(&self.nodes[node].process.read(timeout, 16)?) else {
                return Ok(Action::Complete(-errno::EINVAL));
            };
            Some(self.deadline(delay)?)
        } else {
            None
        };
        Ok(Action::Blocked {
            kind: WaitKind::Futex { address },
            deadline,
        })
    }
    pub(crate) fn wake_futex(&mut self, node: usize, address: u64, maximum: u64) -> Result<i64> {
        let mut count = 0;
        while count < maximum {
            let Some(id) = self
                .futexes
                .get_mut(&(node, address))
                .and_then(VecDeque::pop_front)
            else {
                break;
            };
            if self.wake(id, 0)? {
                count += 1;
            }
        }
        Ok(count as i64)
    }
    pub(crate) fn socket(&self, node: usize, fd: u64) -> Option<Rc<RefCell<Socket>>> {
        match self.nodes[node].descriptors.get(&fd) {
            Some(Descriptor::Socket(s)) => Some(s.clone()),
            _ => None,
        }
    }
    fn bind(&mut self, node: usize, socket: &Rc<RefCell<Socket>>, target: Address) -> i64 {
        if socket.borrow().bound.is_some() {
            return -errno::EINVAL;
        }
        let ip = self.nodes[node].ip;
        if target.ip != ip && !target.ip.is_unspecified() {
            return -errno::EADDRNOTAVAIL;
        }
        let port = if target.port == 0 {
            (49152..=65535)
                .find(|port| !self.bindings.contains_key(&Address { ip, port: *port }))
                .unwrap_or(0)
        } else {
            target.port
        };
        let key = Address { ip, port };
        if port == 0 || self.bindings.contains_key(&key) {
            return -errno::EADDRINUSE;
        }
        self.bindings.insert(key, (node, socket.clone()));
        socket.borrow_mut().bound = Some(key);
        0
    }
    fn send(&mut self, node: usize, req: &SyscallRequest, write: bool) -> Result<Action> {
        let a = req.args;
        let Some(socket) = self.socket(node, a[0]) else {
            return Ok(Action::Complete(-errno::ENOTSOCK));
        };
        if socket.borrow().tcp.is_some() {
            return self.tcp_send(node, req, write);
        }
        if !write && a[3] & !0x40 != 0 {
            return Err(Error::Unsupported("unsupported sendto flags".into()));
        }
        if a[2] > 65507 {
            return Ok(Action::Complete(-errno::EMSGSIZE));
        }
        if !write && a[4] != 0 && a[5] < 16 {
            return Ok(Action::Complete(-errno::EINVAL));
        }
        let payload = self.nodes[node].process.read(a[1], a[2])?;
        let destination = if !write && a[4] != 0 {
            Some(sockaddr(&self.nodes[node].process, a[4], a[5])?)
        } else {
            socket.borrow().peer
        };
        let Some(destination) = destination else {
            return Ok(Action::Complete(-errno::EDESTADDRREQ));
        };
        if socket.borrow().bound.is_none() {
            let ip = self.nodes[node].ip;
            let result = self.bind(node, &socket, Address { ip, port: 0 });
            if result != 0 {
                return Ok(Action::Complete(result));
            }
        }
        let source = socket.borrow().bound.unwrap();
        self.send_packet(node, source, destination, payload)?;
        Ok(Action::Complete(a[2] as i64))
    }
    fn receive(&mut self, node: usize, req: &SyscallRequest, read: bool) -> Result<Action> {
        let a = req.args;
        let Some(socket) = self.socket(node, a[0]) else {
            return Ok(Action::Complete(-errno::ENOTSOCK));
        };
        if socket.borrow().tcp.is_some() {
            return self.tcp_receive(node, req, read);
        }
        if !read && a[3] & !0x40 != 0 {
            return Err(Error::Unsupported("unsupported recvfrom flags".into()));
        }
        self.nodes[node]
            .process
            .check_memory(a[1], a[2], Prot::WRITE)?;
        if !read && a[4] != 0 {
            let length =
                u32::from_le_bytes(self.nodes[node].process.read(a[5], 4)?.try_into().unwrap());
            self.nodes[node]
                .process
                .check_memory(a[4], u64::from(length).min(16), Prot::WRITE)?;
            self.nodes[node]
                .process
                .check_memory(a[5], 4, Prot::WRITE)?;
        }
        let receive = Receive {
            socket: socket.clone(),
            address: a[1],
            length: a[2],
            peer: if read { 0 } else { a[4] },
            peer_length: if read { 0 } else { a[5] },
        };
        let packet = socket.borrow_mut().packets.pop_front();
        if let Some(packet) = packet {
            return Ok(Action::Complete(
                self.consume_packet(node, &receive, packet)?,
            ));
        }
        if socket.borrow().nonblocking || !read && a[3] & 0x40 != 0 {
            return Ok(Action::Complete(-errno::EAGAIN));
        }
        Ok(Action::Blocked {
            kind: WaitKind::Socket(receive),
            deadline: None,
        })
    }
    fn consume_packet(
        &mut self,
        node: usize,
        data: &Receive,
        (source, payload): (Address, Vec<u8>),
    ) -> Result<i64> {
        let bytes = &payload[..payload.len().min(data.length as usize)];
        self.nodes[node].process.write(data.address, bytes)?;
        if data.peer != 0 {
            let old_length = u32::from_le_bytes(
                self.nodes[node]
                    .process
                    .read(data.peer_length, 4)?
                    .try_into()
                    .unwrap(),
            ) as usize;
            let mut sockaddr = vec![2, 0];
            sockaddr.extend(source.port.to_be_bytes());
            sockaddr.extend(source.ip.octets());
            sockaddr.extend([0; 8]);
            self.nodes[node]
                .process
                .write(data.peer, &sockaddr[..old_length.min(16)])?;
            self.nodes[node]
                .process
                .write(data.peer_length, &16u32.to_le_bytes())?;
        }
        Ok(bytes.len() as i64)
    }
    pub(crate) fn deliver_packet(
        &mut self,
        destination: Address,
        source: Address,
        payload: Vec<u8>,
    ) -> Result<()> {
        let Some((node, socket)) = self.bindings.get(&destination).cloned() else {
            return self.emit(
                "packet_drop",
                json!({"reason": "unbound", "destination": destination.value()}),
            );
        };
        if self.nodes[node]
            .process
            .threads
            .values()
            .all(|t| t.state == State::Exited)
        {
            return self.emit(
                "packet_drop",
                json!({"reason": "node_exited", "destination": destination.value()}),
            );
        }
        loop {
            let id = socket.borrow_mut().waiters.pop_front();
            let Some(id) = id else {
                break;
            };
            let data = match self.waits.get(&id).map(|w| w.kind.clone()) {
                Some(WaitKind::Socket(data)) => data,
                _ => continue,
            };
            let result = match self.consume_packet(node, &data, (source, payload)) {
                Ok(value) => value,
                Err(Error::Memory(_)) => -errno::EFAULT,
                Err(e) => return Err(e),
            };
            self.wake(id, result)?;
            return Ok(());
        }
        if socket.borrow().packets.len() >= 1024 {
            self.emit(
                "packet_drop",
                json!({"reason": "receive_buffer_full", "destination": destination.value()}),
            )?;
        } else {
            socket.borrow_mut().packets.push_back((source, payload));
        }
        self.wake_ready()
    }
}

pub(crate) fn timespec(data: &[u8]) -> Option<u64> {
    let seconds = i64::from_le_bytes(data[..8].try_into().ok()?);
    let ns = i64::from_le_bytes(data[8..].try_into().ok()?);
    if !(0..=100_000_000).contains(&seconds) || !(0..1_000_000_000).contains(&ns) {
        return None;
    }
    Some(seconds as u64 * 1_000_000_000 + ns as u64)
}
pub(crate) fn sockaddr(process: &Process, address: u64, length: u64) -> Result<Address> {
    if length < 16 {
        return Err(Error::Memory("short sockaddr".into()));
    }
    let data = process.read(address, 16)?;
    if data[..2] != [2, 0] {
        return Err(Error::Unsupported("only IPv4 sockaddr is supported".into()));
    }
    Ok(Address {
        ip: Ipv4Addr::new(data[4], data[5], data[6], data[7]),
        port: u16::from_be_bytes([data[2], data[3]]),
    })
}
