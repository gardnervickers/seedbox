//! TCP byte streams and level-triggered readiness in the coordinator's memory.
//! Transport faults reset streams; UDP retains packet loss/duplication semantics.
use crate::{
    backend::SyscallRequest,
    error::{Error, Result},
    kernel::{
        Action, Address, Descriptor, Epoll, EpollInterest, MAX_FILE, Receive, Socket, Tcp,
        WaitKind, sockaddr,
    },
    linux_errno as errno,
    runtime::{Event, Simulation},
};
use serde_json::json;
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    net::Ipv4Addr,
    rc::Rc,
};
use unicorn_engine::Prot;

#[derive(Clone)]
pub enum ReadyWait {
    Poll {
        address: u64,
        count: u64,
    },
    Epoll {
        epoll: Rc<RefCell<Epoll>>,
        address: u64,
        maximum: u64,
    },
}
fn fresh(nonblocking: bool) -> Rc<RefCell<Socket>> {
    Rc::new(RefCell::new(Socket {
        nonblocking,
        bound: None,
        peer: None,
        packets: VecDeque::new(),
        waiters: VecDeque::new(),
        tcp: Some(Tcp::default()),
    }))
}
fn ready(socket: &Socket) -> u32 {
    if let Some(tcp) = &socket.tcp {
        if tcp.listening {
            return if tcp.accepts.is_empty() { 0 } else { 1 };
        }
        if tcp.reset {
            return 1 | 4 | 8 | 16;
        }
        if tcp.peer.is_none() {
            return 16;
        }
        let mut result = if tcp.access & 2 != 0 { 4 } else { 0 };
        if !tcp.bytes.is_empty() || tcp.eof {
            result |= 1;
        }
        if tcp.eof {
            result |= 8192;
        }
        result
    } else {
        4 | if socket.packets.is_empty() { 0 } else { 1 }
    }
}
impl Simulation {
    pub(crate) fn new_tcp(&mut self, node: usize, nonblocking: bool) -> Result<Action> {
        Ok(Action::Complete(
            self.nodes[node].allocate(Descriptor::Socket(fresh(nonblocking)))?,
        ))
    }
    fn tcp_bind(&mut self, node: usize, socket: &Rc<RefCell<Socket>>, target: Address) -> i64 {
        if socket.borrow().bound.is_some() {
            return -errno::EINVAL;
        }
        let ip = self.nodes[node].ip;
        if target.ip != ip && !target.ip.is_unspecified() {
            return -errno::EADDRNOTAVAIL;
        }
        let port = if target.port == 0 {
            (49152..=65535)
                .find(|port| !self.tcp_bindings.contains_key(&Address { ip, port: *port }))
                .unwrap_or(0)
        } else {
            target.port
        };
        let address = Address { ip, port };
        if port == 0 || self.tcp_bindings.contains_key(&address) {
            return -errno::EADDRINUSE;
        }
        self.tcp_bindings.insert(address, (node, socket.clone()));
        socket.borrow_mut().bound = Some(address);
        0
    }
    pub(crate) fn tcp_address_call(
        &mut self,
        node: usize,
        req: &SyscallRequest,
        socket: &Rc<RefCell<Socket>>,
    ) -> Result<Action> {
        let a = req.args;
        let target = sockaddr(&self.nodes[node].process, a[1], a[2])?;
        if req.number == 49 {
            return Ok(Action::Complete(self.tcp_bind(node, socket, target)));
        }
        if socket.borrow().tcp.as_ref().unwrap().peer.is_some() {
            return Ok(Action::Complete(-errno::EISCONN));
        }
        let Some((server, listener)) = self.tcp_bindings.get(&target).cloned() else {
            return Ok(Action::Complete(-errno::ECONNREFUSED));
        };
        {
            let l = listener.borrow();
            let t = l.tcp.as_ref().unwrap();
            if !t.listening || t.accepts.len() >= t.backlog || self.partitioned(node, server) {
                return Ok(Action::Complete(-errno::ECONNREFUSED));
            }
        }
        if socket.borrow().bound.is_none() {
            let result = self.tcp_bind(
                node,
                socket,
                Address {
                    ip: self.nodes[node].ip,
                    port: 0,
                },
            );
            if result != 0 {
                return Ok(Action::Complete(result));
            }
        }
        let accepted = fresh(false);
        {
            let mut accepted = accepted.borrow_mut();
            accepted.bound = Some(target);
            accepted.peer = socket.borrow().bound;
            let t = accepted.tcp.as_mut().unwrap();
            t.peer = Some(Rc::downgrade(socket));
            t.peer_node = node;
        }
        {
            let mut s = socket.borrow_mut();
            s.peer = Some(target);
            let t = s.tcp.as_mut().unwrap();
            t.peer = Some(Rc::downgrade(&accepted));
            t.peer_node = server;
        }
        listener
            .borrow_mut()
            .tcp
            .as_mut()
            .unwrap()
            .accepts
            .push_back(accepted);
        self.emit("tcp_connect",json!({"node":self.nodes[node].process.name,"source":socket.borrow().bound.unwrap().value(),"destination":target.value()}))?;
        self.wake_ready()?;
        Ok(Action::Complete(if socket.borrow().nonblocking {
            -errno::EINPROGRESS
        } else {
            0
        }))
    }
    fn peer_output(&mut self, node: usize, address: u64, length: u64, peer: Address) -> Result<()> {
        if address == 0 {
            return Ok(());
        }
        let count = u32::from_le_bytes(
            self.nodes[node]
                .process
                .read(length, 4)?
                .try_into()
                .unwrap(),
        );
        let mut bytes = vec![2, 0];
        bytes.extend(peer.port.to_be_bytes());
        bytes.extend(peer.ip.octets());
        bytes.extend([0; 8]);
        self.nodes[node]
            .process
            .check_memory(address, u64::from(count).min(16), Prot::WRITE)?;
        self.nodes[node]
            .process
            .check_memory(length, 4, Prot::WRITE)?;
        self.nodes[node]
            .process
            .write(address, &bytes[..(count as usize).min(16)])?;
        self.nodes[node]
            .process
            .write(length, &16u32.to_le_bytes())?;
        Ok(())
    }
    fn accept_tcp(
        &mut self,
        node: usize,
        socket: &Rc<RefCell<Socket>>,
        address: u64,
        length: u64,
        nonblocking: bool,
    ) -> Result<Option<i64>> {
        let accepted = socket
            .borrow()
            .tcp
            .as_ref()
            .unwrap()
            .accepts
            .front()
            .cloned();
        let Some(accepted) = accepted else {
            return Ok(None);
        };
        self.peer_output(node, address, length, accepted.borrow().peer.unwrap())?;
        let fd = self.nodes[node].allocate(Descriptor::Socket(accepted.clone()))?;
        accepted.borrow_mut().nonblocking = nonblocking;
        socket
            .borrow_mut()
            .tcp
            .as_mut()
            .unwrap()
            .accepts
            .pop_front();
        self.emit(
            "tcp_accept",
            json!({"node":self.nodes[node].process.name,"fd":fd}),
        )?;
        Ok(Some(fd))
    }
    pub(crate) fn remove_epoll_fd(&mut self, node: usize, fd: u64) {
        for d in self.nodes[node].descriptors.values() {
            if let Descriptor::Epoll(e) = d {
                e.borrow_mut().interests.remove(&fd);
            }
        }
    }
    pub(crate) fn close_tcp(&mut self, node: usize, fd: u64) -> Result<Action> {
        let socket = self.socket(node, fd).unwrap();
        let bound = socket.borrow().bound;
        if let Some(bound) = bound {
            if self
                .tcp_bindings
                .get(&bound)
                .is_some_and(|(_, s)| Rc::ptr_eq(s, &socket))
            {
                self.tcp_bindings.remove(&bound);
            }
        }
        // Deliver FIN after this endpoint's queued bytes, in the same direction.
        self.send_stream_event(node, &socket, Vec::new(), true)?;
        let waits: Vec<_> = self
            .waits
            .iter()
            .filter_map(|(id, w)| match &w.kind {
                WaitKind::Socket(r) if Rc::ptr_eq(&r.socket, &socket) => Some(*id),
                WaitKind::Accept { socket: s, .. } if Rc::ptr_eq(s, &socket) => Some(*id),
                _ => None,
            })
            .collect();
        self.nodes[node].descriptors.remove(&fd);
        self.remove_epoll_fd(node, fd);
        for id in waits {
            self.wake(id, -errno::EBADF)?;
        }
        self.wake_ready()?;
        Ok(Action::Complete(0))
    }
    fn send_stream_event(
        &mut self,
        node: usize,
        socket: &Rc<RefCell<Socket>>,
        payload: Vec<u8>,
        eof: bool,
    ) -> Result<()> {
        let (target, destination, local, previous) = {
            let s = socket.borrow();
            let t = s.tcp.as_ref().unwrap();
            (
                t.peer.as_ref().and_then(|s| s.upgrade()),
                t.peer_node,
                t.local,
                t.next_delivery,
            )
        };
        let Some(target) = target else {
            return Ok(());
        };
        if local {
            return self.deliver_stream(destination, &target, payload, eof, false);
        }
        let cfg = self.config.network.clone();
        let domain = format!(
            "tcp:{}",
            String::from_utf8(crate::runtime::canonical(&json!([
                self.nodes[node].process.name,
                self.nodes[destination].process.name
            ])))
            .unwrap()
        );
        let rng = self.stream(&domain);
        let loss = rng.below(1_000_000);
        let jitter = rng.below(2 * cfg.jitter_ns + 1) as i64 - cfg.jitter_ns as i64;
        let delay = (cfg.latency_ns as i64 + jitter).max(0) as u64;
        let deadline = self.deadline(delay)?.max(previous);
        socket.borrow_mut().tcp.as_mut().unwrap().next_delivery = deadline;
        self.emit("tcp_send",json!({"node":self.nodes[node].process.name,"destination":self.nodes[destination].process.name,"data":hex::encode(&payload),"fin":eof,"deadline_ns":deadline}))?;
        self.schedule(
            deadline,
            Event::Stream {
                source_node: node,
                destination_node: destination,
                socket: target,
                payload,
                eof,
                reset: loss < cfg.loss_ppm || self.partitioned(node, destination),
            },
        )
    }
    pub(crate) fn tcp_send_bytes(&mut self, node: usize, fd: u64, bytes: Vec<u8>) -> Result<i64> {
        let Some(socket) = self.socket(node, fd) else {
            return Ok(-errno::ENOTSOCK);
        };
        {
            let s = socket.borrow();
            let Some(t) = s.tcp.as_ref() else {
                return Err(Error::Unsupported(
                    "vectored datagram writes are not modeled".into(),
                ));
            };
            if t.access & 2 == 0 {
                return Ok(-errno::EBADF);
            }
            if t.reset {
                return Ok(-errno::ECONNRESET);
            }
            if t.peer.as_ref().and_then(|s| s.upgrade()).is_none() {
                return Ok(-errno::EPIPE);
            }
        }
        let length = bytes.len();
        self.send_stream_event(node, &socket, bytes, false)?;
        Ok(length as i64)
    }
    pub(crate) fn tcp_send(
        &mut self,
        node: usize,
        req: &SyscallRequest,
        write: bool,
    ) -> Result<Action> {
        let a = req.args;
        if !write && (a[3] & !(0x40 | 0x4000) != 0 || a[4] != 0) {
            return Err(Error::Unsupported(
                "unsupported TCP sendto arguments".into(),
            ));
        }
        let bytes = self.nodes[node].process.read(a[1], a[2])?;
        Ok(Action::Complete(self.tcp_send_bytes(node, a[0], bytes)?))
    }
    fn consume_stream(&mut self, node: usize, receive: &Receive) -> Result<Option<i64>> {
        let mut socket = receive.socket.borrow_mut();
        let t = socket.tcp.as_mut().unwrap();
        if t.access & 1 == 0 {
            return Ok(Some(-errno::EBADF));
        }
        if t.reset {
            return Ok(Some(-errno::ECONNRESET));
        }
        if !t.bytes.is_empty() {
            let size = t.bytes.len().min(receive.length as usize);
            let bytes: Vec<_> = t.bytes.iter().take(size).copied().collect();
            self.nodes[node].process.write(receive.address, &bytes)?;
            t.bytes.drain(..size);
            return Ok(Some(size as i64));
        }
        if t.eof {
            return Ok(Some(0));
        }
        if t.peer.is_none() {
            return Ok(Some(-errno::ENOTCONN));
        }
        Ok(None)
    }
    pub(crate) fn tcp_receive(
        &mut self,
        node: usize,
        req: &SyscallRequest,
        read: bool,
    ) -> Result<Action> {
        let a = req.args;
        let socket = self.socket(node, a[0]).unwrap();
        if !read && (a[3] & !0x40 != 0 || a[4] != 0) {
            return Err(Error::Unsupported(
                "unsupported TCP recvfrom arguments".into(),
            ));
        }
        self.nodes[node]
            .process
            .check_memory(a[1], a[2], Prot::WRITE)?;
        if a[2] == 0 {
            return Ok(Action::Complete(0));
        }
        let receive = Receive {
            socket: socket.clone(),
            address: a[1],
            length: a[2],
            peer: 0,
            peer_length: 0,
        };
        if let Some(value) = self.consume_stream(node, &receive)? {
            return Ok(Action::Complete(value));
        }
        if socket.borrow().nonblocking || (!read && a[3] & 0x40 != 0) {
            return Ok(Action::Complete(-errno::EAGAIN));
        }
        Ok(Action::Blocked {
            kind: WaitKind::Socket(receive),
            deadline: None,
        })
    }
    pub(crate) fn deliver_stream(
        &mut self,
        node: usize,
        socket: &Rc<RefCell<Socket>>,
        payload: Vec<u8>,
        eof: bool,
        reset: bool,
    ) -> Result<()> {
        let peer = socket
            .borrow()
            .tcp
            .as_ref()
            .unwrap()
            .peer
            .as_ref()
            .and_then(|p| p.upgrade());
        {
            let mut socket = socket.borrow_mut();
            let t = socket.tcp.as_mut().unwrap();
            if t.reset {
                return Ok(());
            }
            if reset || t.bytes.len() + payload.len() > MAX_FILE {
                t.reset = true;
            } else {
                t.bytes.extend(&payload);
                t.eof |= eof;
            }
        }
        if reset {
            if let Some(peer) = peer {
                peer.borrow_mut().tcp.as_mut().unwrap().reset = true;
            }
        }
        self.emit("tcp_deliver",json!({"node":self.nodes[node].process.name,"data":hex::encode(&payload),"fin":eof,"reset":reset}))?;
        self.wake_ready()
    }
    fn descriptor_ready(&self, node: usize, fd: u64) -> u32 {
        match self.nodes[node].descriptors.get(&fd) {
            Some(Descriptor::Socket(s)) => ready(&s.borrow()),
            Some(Descriptor::File(_) | Descriptor::Entropy) => 5,
            _ if fd <= 2 => 5,
            _ => 32,
        }
    }
    fn finish_ready(&mut self, node: usize, wait: &ReadyWait) -> Result<i64> {
        match wait {
            ReadyWait::Poll { address, count } => {
                let mut data = self.nodes[node].process.read(*address, count * 8)?;
                let mut result = 0;
                for item in data.chunks_exact_mut(8) {
                    let fd = i32::from_le_bytes(item[..4].try_into().unwrap());
                    let events = u16::from_le_bytes(item[4..6].try_into().unwrap()) as u32;
                    let revents = if fd < 0 {
                        0
                    } else {
                        self.descriptor_ready(node, fd as u64) & (events | 8 | 16 | 32)
                    };
                    item[6..8].copy_from_slice(&(revents as u16).to_le_bytes());
                    if revents != 0 {
                        result += 1;
                    }
                }
                self.nodes[node].process.write(*address, &data)?;
                Ok(result)
            }
            ReadyWait::Epoll {
                epoll,
                address,
                maximum,
            } => {
                let e = epoll.borrow();
                let mut bytes = Vec::new();
                let mut count = 0;
                for (fd, interest) in &e.interests {
                    let events = self.descriptor_ready(node, *fd) & (interest.events | 8 | 16);
                    if events == 0 {
                        continue;
                    }
                    bytes.extend(events.to_le_bytes());
                    bytes.extend(interest.data.to_le_bytes());
                    count += 1;
                    if count == *maximum {
                        break;
                    }
                }
                self.nodes[node].process.write(*address, &bytes)?;
                Ok(count as i64)
            }
        }
    }
    pub(crate) fn wake_ready(&mut self) -> Result<()> {
        let waits: Vec<_> = self
            .waits
            .iter()
            .map(|(id, w)| (*id, w.node, w.kind.clone()))
            .collect();
        for (id, node, kind) in waits {
            let outcome = match kind {
                WaitKind::Ready(wait) => self
                    .finish_ready(node, &wait)
                    .map(|v| if v != 0 { Some(v) } else { None }),
                WaitKind::Socket(r) if r.socket.borrow().tcp.is_some() => {
                    self.consume_stream(node, &r)
                }
                WaitKind::Accept {
                    socket,
                    address,
                    length,
                    nonblocking,
                } => self.accept_tcp(node, &socket, address, length, nonblocking),
                _ => continue,
            };
            let value = match outcome {
                Ok(v) => v,
                Err(Error::Memory(_)) => Some(-errno::EFAULT),
                Err(e) => return Err(e),
            };
            if let Some(value) = value {
                self.wake(id, value)?;
            }
        }
        Ok(())
    }
    pub(crate) fn network_syscall(&mut self, node: usize, req: &SyscallRequest) -> Result<Action> {
        use Action::Complete;
        let a = req.args;
        match req.number {
            50 => {
                let Some(socket) = self.socket(node, a[0]) else {
                    return Ok(Complete(-errno::ENOTSOCK));
                };
                let mut socket = socket.borrow_mut();
                if socket.bound.is_none() {
                    return Ok(Complete(-errno::EINVAL));
                }
                let Some(t) = socket.tcp.as_mut() else {
                    return Ok(Complete(-errno::EINVAL));
                };
                t.listening = true;
                t.backlog = (a[1] as usize).clamp(1, 1024);
                Ok(Complete(0))
            }
            43 | 288 => {
                let flags = if req.number == 288 { a[3] } else { 0 };
                if flags & !0x80800 != 0 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let Some(socket) = self.socket(node, a[0]) else {
                    return Ok(Complete(-errno::ENOTSOCK));
                };
                if !socket.borrow().tcp.as_ref().is_some_and(|t| t.listening) {
                    return Ok(Complete(-errno::EINVAL));
                }
                if let Some(value) =
                    self.accept_tcp(node, &socket, a[1], a[2], flags & 0x800 != 0)?
                {
                    return Ok(Complete(value));
                }
                if socket.borrow().nonblocking {
                    return Ok(Complete(-errno::EAGAIN));
                }
                Ok(Action::Blocked {
                    kind: WaitKind::Accept {
                        socket,
                        address: a[1],
                        length: a[2],
                        nonblocking: flags & 0x800 != 0,
                    },
                    deadline: None,
                })
            }
            51 | 52 => {
                let Some(socket) = self.socket(node, a[0]) else {
                    return Ok(Complete(-errno::ENOTSOCK));
                };
                let peer = if req.number == 51 {
                    socket.borrow().bound.or(Some(Address {
                        ip: Ipv4Addr::UNSPECIFIED,
                        port: 0,
                    }))
                } else {
                    socket.borrow().peer
                };
                let Some(peer) = peer else {
                    return Ok(Complete(-errno::ENOTCONN));
                };
                self.peer_output(node, a[1], a[2], peer)?;
                Ok(Complete(0))
            }
            54 => {
                if self.socket(node, a[0]).is_none() {
                    return Ok(Complete(-errno::ENOTSOCK));
                }
                if !((a[1] == 1 && [2, 9, 15].contains(&a[2]))
                    || (a[1] == 6 && [1, 4, 5, 6].contains(&a[2])))
                {
                    return Err(Error::Unsupported(format!(
                        "unsupported socket option {}/{}",
                        a[1], a[2]
                    )));
                }
                if a[4] != 4 {
                    return Ok(Complete(-errno::EINVAL));
                }
                self.nodes[node].process.read(a[3], 4)?;
                Ok(Complete(0))
            }
            55 => {
                let Some(socket) = self.socket(node, a[0]) else {
                    return Ok(Complete(-errno::ENOTSOCK));
                };
                if a[1] != 1 || a[2] != 4 {
                    return Err(Error::Unsupported(
                        "only SO_ERROR getsockopt is modeled".into(),
                    ));
                }
                let size =
                    u32::from_le_bytes(self.nodes[node].process.read(a[4], 4)?.try_into().unwrap());
                if size < 4 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let error = if socket.borrow().tcp.as_ref().is_some_and(|t| t.reset) {
                    errno::ECONNRESET as u32
                } else {
                    0
                };
                self.nodes[node].process.write(a[3], &error.to_le_bytes())?;
                self.nodes[node].process.write(a[4], &4u32.to_le_bytes())?;
                Ok(Complete(0))
            }
            72 => {
                let descriptor = self.nodes[node].descriptors.get(&a[0]).cloned();
                if descriptor.is_none() && a[0] > 2 {
                    return Ok(Complete(-errno::EBADF));
                }
                let value = match a[1] {
                    1 | 2 => 0,
                    3 => match descriptor {
                        Some(Descriptor::Socket(s)) => {
                            let s = s.borrow();
                            let access = s
                                .tcp
                                .as_ref()
                                .map(|t| if t.local { i64::from(t.access - 1) } else { 2 })
                                .unwrap_or(2);
                            access | if s.nonblocking { 0x800 } else { 0 }
                        }
                        Some(Descriptor::File(f)) => f.borrow().flags as i64,
                        _ => 0,
                    },
                    4 => {
                        if a[2] & !(3 | 0x800 | 0x8000 | 0x400) != 0 {
                            return Err(Error::Unsupported(
                                "unsupported fcntl status flags".into(),
                            ));
                        }
                        match descriptor {
                            Some(Descriptor::Socket(s)) => {
                                s.borrow_mut().nonblocking = a[2] & 0x800 != 0
                            }
                            Some(Descriptor::File(f)) => {
                                let mut f = f.borrow_mut();
                                f.flags = (f.flags & !(0x800 | 0x400)) | (a[2] & (0x800 | 0x400));
                            }
                            _ => {}
                        }
                        0
                    }
                    _ => {
                        return Err(Error::Unsupported(format!(
                            "unsupported fcntl command {}",
                            a[1]
                        )));
                    }
                };
                Ok(Complete(value))
            }
            213 | 291 => {
                if (req.number == 213 && a[0] == 0) || (req.number == 291 && a[0] & !0x80000 != 0) {
                    return Ok(Complete(-errno::EINVAL));
                }
                Ok(Complete(self.nodes[node].allocate(Descriptor::Epoll(
                    Rc::new(RefCell::new(Epoll {
                        interests: BTreeMap::new(),
                    })),
                ))?))
            }
            233 => {
                let Some(Descriptor::Epoll(epoll)) =
                    self.nodes[node].descriptors.get(&a[0]).cloned()
                else {
                    return Ok(Complete(-errno::EBADF));
                };
                if !self.nodes[node].descriptors.contains_key(&a[2]) {
                    return Ok(Complete(-errno::EBADF));
                }
                if a[0] == a[2] {
                    return Ok(Complete(-errno::EINVAL));
                }
                let mut epoll = epoll.borrow_mut();
                let exists = epoll.interests.contains_key(&a[2]);
                if a[1] == 2 {
                    return Ok(Complete(if epoll.interests.remove(&a[2]).is_some() {
                        0
                    } else {
                        -errno::ENOENT
                    }));
                }
                if a[1] == 1 && exists {
                    return Ok(Complete(-errno::EEXIST));
                }
                if a[1] == 3 && !exists {
                    return Ok(Complete(-errno::ENOENT));
                }
                if ![1, 3].contains(&a[1]) {
                    return Ok(Complete(-errno::EINVAL));
                }
                let data = self.nodes[node].process.read(a[3], 12)?;
                let events = u32::from_le_bytes(data[..4].try_into().unwrap());
                if events & !(1 | 2 | 4 | 8 | 16 | 8192) != 0 {
                    return Err(Error::Unsupported(
                        "only level-triggered epoll is modeled".into(),
                    ));
                }
                epoll.interests.insert(
                    a[2],
                    EpollInterest {
                        events,
                        data: u64::from_le_bytes(data[4..].try_into().unwrap()),
                    },
                );
                Ok(Complete(0))
            }
            7 | 232 | 281 => {
                if req.number == 281 && a[4] != 0 {
                    return Err(Error::Unsupported(
                        "epoll_pwait signal-mask replacement is not modeled".into(),
                    ));
                }
                let (wait, timeout) = if req.number == 7 {
                    if a[1] > 1024 {
                        return Ok(Complete(-errno::EINVAL));
                    }
                    self.nodes[node]
                        .process
                        .check_memory(a[0], a[1] * 8, Prot::WRITE)?;
                    (
                        ReadyWait::Poll {
                            address: a[0],
                            count: a[1],
                        },
                        a[2] as i32,
                    )
                } else {
                    if a[2] == 0 || a[2] > 1024 {
                        return Ok(Complete(-errno::EINVAL));
                    }
                    let Some(Descriptor::Epoll(e)) =
                        self.nodes[node].descriptors.get(&a[0]).cloned()
                    else {
                        return Ok(Complete(-errno::EBADF));
                    };
                    self.nodes[node]
                        .process
                        .check_memory(a[1], a[2] * 12, Prot::WRITE)?;
                    (
                        ReadyWait::Epoll {
                            epoll: e,
                            address: a[1],
                            maximum: a[2],
                        },
                        a[3] as i32,
                    )
                };
                let count = self.finish_ready(node, &wait)?;
                if count != 0 || timeout == 0 {
                    return Ok(Complete(count));
                }
                Ok(Action::Blocked {
                    kind: WaitKind::Ready(wait),
                    deadline: if timeout < 0 {
                        None
                    } else {
                        Some(self.deadline(timeout as u64 * 1_000_000)?)
                    },
                })
            }
            22 | 293 => {
                let flags = if req.number == 293 { a[1] } else { 0 };
                if flags & !0x80800 != 0 {
                    return Ok(Complete(-errno::EINVAL));
                }
                self.nodes[node]
                    .process
                    .check_memory(a[0], 8, Prot::WRITE)?;
                let reader = fresh(flags & 0x800 != 0);
                let writer = fresh(flags & 0x800 != 0);
                {
                    let mut r = reader.borrow_mut();
                    let t = r.tcp.as_mut().unwrap();
                    t.peer = Some(Rc::downgrade(&writer));
                    t.peer_node = node;
                    t.local = true;
                    t.access = 1;
                }
                {
                    let mut w = writer.borrow_mut();
                    let t = w.tcp.as_mut().unwrap();
                    t.peer = Some(Rc::downgrade(&reader));
                    t.peer_node = node;
                    t.local = true;
                    t.access = 2;
                }
                let r = self.nodes[node].allocate(Descriptor::Socket(reader))?;
                let w = self.nodes[node].allocate(Descriptor::Socket(writer))?;
                let data: Vec<_> = [r as u32, w as u32]
                    .iter()
                    .flat_map(|fd| fd.to_le_bytes())
                    .collect();
                self.nodes[node].process.write(a[0], &data)?;
                Ok(Complete(0))
            }
            n => Err(Error::Unsupported(format!("unsupported syscall {n}"))),
        }
    }
}
