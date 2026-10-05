//! The deterministic x86-64 Linux userspace ABI profile used by static libc.
use crate::{
    backend::{MAX_IO, SyscallRequest},
    error::{Error, Result},
    kernel::{Action, Descriptor, DiskOperation, MAX_FILE, WaitKind, path},
    linux_errno as errno,
    runtime::Simulation,
};
use serde_json::json;
use unicorn_engine::Prot;

fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

impl Simulation {
    pub(crate) fn absolute_path(&self, node: usize, name: &str) -> String {
        if name.starts_with('/') {
            path(name)
        } else {
            path(&format!("{}/{name}", self.nodes[node].cwd))
        }
    }
    pub(crate) fn linux_syscall(
        &mut self,
        node: usize,
        tid: u64,
        req: &SyscallRequest,
    ) -> Result<Action> {
        use Action::Complete;
        let a = req.args;
        let result = match req.number {
            9 => {
                if a[3] & 0x20 == 0 {
                    return Err(Error::Unsupported("file-backed mmap is not modeled".into()));
                }
                self.nodes[node].process.mmap(a[0], a[1], a[2], a[3])?
            }
            10 => self.nodes[node].process.mprotect(a[0], a[1], a[2])?,
            11 => self.nodes[node].process.munmap(a[0], a[1])?,
            12 => self.nodes[node].process.brk(a[0])? as i64,
            158 => {
                if a[0] != 0x1002 {
                    return Err(Error::Unsupported("only ARCH_SET_FS is modeled".into()));
                }
                if a[1] >= 0x800000000000 {
                    -errno::EINVAL
                } else {
                    self.nodes[node].process.set_tls(tid, a[1])?;
                    0
                }
            }
            13 => {
                if a[3] != 8 || a[0] == 0 || a[0] > 64 || [9, 19].contains(&a[0]) {
                    return Ok(Complete(-errno::EINVAL));
                }
                let new = if a[1] != 0 {
                    Some(self.nodes[node].process.read(a[1], 32)?)
                } else {
                    None
                };
                if a[2] != 0 {
                    let old = self.nodes[node]
                        .signal_actions
                        .get(&a[0])
                        .cloned()
                        .unwrap_or(vec![0; 32]);
                    self.nodes[node].process.write(a[2], &old)?;
                }
                if let Some(new) = new {
                    self.nodes[node].signal_actions.insert(a[0], new);
                }
                0
            }
            14 => {
                if a[3] != 8 || a[0] > 2 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let new = if a[1] != 0 {
                    Some(u64::from_le_bytes(
                        self.nodes[node].process.read(a[1], 8)?.try_into().unwrap(),
                    ))
                } else {
                    None
                };
                if a[2] != 0 {
                    let old = self.nodes[node].process.threads[&tid].signal_mask;
                    self.nodes[node].process.write(a[2], &old.to_le_bytes())?;
                }
                if let Some(new) = new {
                    let new = new & !(1 << 8 | 1 << 18);
                    let mask = &mut self.nodes[node]
                        .process
                        .threads
                        .get_mut(&tid)
                        .unwrap()
                        .signal_mask;
                    *mask = match a[0] {
                        0 => *mask | new,
                        1 => *mask & !new,
                        _ => new,
                    };
                }
                0
            }
            16 => -errno::ENOTTY,
            38 => {
                if a[0] > 2 {
                    return Ok(Complete(-errno::EINVAL));
                }
                self.nodes[node].process.write(a[1], &[0; 32])?;
                0
            }
            37 => {
                if a[0] > 2 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let timer = self.nodes[node].process.read(a[1], 32)?;
                if timer.iter().any(|byte| *byte != 0) {
                    return Err(Error::Unsupported(
                        "signal-generating interval timers are not modeled".into(),
                    ));
                }
                if a[2] != 0 {
                    self.nodes[node].process.write(a[2], &[0; 32])?;
                }
                0
            }
            19 => {
                if a[2] > 1024 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let raw = self.nodes[node].process.read(a[1], a[2] * 16)?;
                let mut vectors = Vec::new();
                let mut total = 0;
                for v in raw.chunks_exact(16) {
                    let address = u64::from_le_bytes(v[..8].try_into().unwrap());
                    let length = u64::from_le_bytes(v[8..].try_into().unwrap());
                    if length > MAX_IO || total + length > MAX_IO {
                        return Ok(Complete(-errno::EINVAL));
                    }
                    self.nodes[node]
                        .process
                        .check_memory(address, length, Prot::WRITE)?;
                    vectors.push((address, length));
                    total += length;
                }
                match self.nodes[node].descriptors.get(&a[0]).cloned() {
                    Some(Descriptor::Entropy) => {
                        let name = self.nodes[node].process.name.clone();
                        let bytes = self.stream(&format!("entropy:{name}")).read(total as usize);
                        let mut offset = 0;
                        for (address, length) in vectors {
                            self.nodes[node]
                                .process
                                .write(address, &bytes[offset..offset + length as usize])?;
                            offset += length as usize;
                        }
                        total as i64
                    }
                    Some(Descriptor::File(handle)) => {
                        if handle.borrow().flags & 3 == 1 {
                            return Ok(Complete(-errno::EBADF));
                        }
                        return Ok(Action::Blocked {
                            kind: WaitKind::Disk {
                                handle,
                                operation: DiskOperation::ReadVector(vectors),
                            },
                            deadline: Some(self.deadline(self.config.disk.latency_ns)?),
                        });
                    }
                    None if a[0] == 0 => 0,
                    Some(Descriptor::Socket(_)) => {
                        return Err(Error::Unsupported(
                            "vectored socket reads are not modeled".into(),
                        ));
                    }
                    _ => -errno::EBADF,
                }
            }
            20 => {
                if a[2] > 1024 {
                    return Ok(Complete(-errno::EINVAL));
                }
                let vectors = self.nodes[node].process.read(a[1], a[2] * 16)?;
                let mut output = Vec::new();
                for v in vectors.chunks_exact(16) {
                    let address = u64::from_le_bytes(v[..8].try_into().unwrap());
                    let length = u64::from_le_bytes(v[8..].try_into().unwrap());
                    if length > MAX_IO || output.len() as u64 + length > MAX_IO {
                        return Ok(Complete(-errno::EINVAL));
                    }
                    output.extend(self.nodes[node].process.read(address, length)?);
                }
                if ![1, 2].contains(&a[0]) {
                    return Ok(Complete(self.tcp_send_bytes(node, a[0], output)?));
                }
                let target = if a[0] == 1 {
                    &mut self.nodes[node].stdout
                } else {
                    &mut self.nodes[node].stderr
                };
                if target.len() + output.len() > MAX_FILE {
                    return Err(Error::Invariant("guest output limit exceeded".into()));
                }
                target.extend(&output);
                self.emit("output", json!({"node":self.nodes[node].process.name,"fd":a[0],"data":hex::encode(&output)}))?;
                output.len() as i64
            }
            4 | 5 | 6 | 262 => {
                let (filename, output) = match req.number {
                    5 => (None, a[1]),
                    262 => {
                        if a[0] != (-100i64) as u64 || a[3] & !0x100 != 0 {
                            return Ok(Complete(-errno::EINVAL));
                        }
                        (Some(self.nodes[node].process.cstring(a[1])?), a[2])
                    }
                    _ => (Some(self.nodes[node].process.cstring(a[0])?), a[1]),
                };
                let (size, mode) = if let Some(filename) = filename {
                    let name = self.absolute_path(node, &filename);
                    if name == self.nodes[node].cwd || name == "/" {
                        (0, 0o40755)
                    } else if let Some(file) = self.nodes[node].files.get(&name) {
                        (file.contents.len(), 0o100644)
                    } else {
                        return Ok(Complete(-errno::ENOENT));
                    }
                } else {
                    match self.nodes[node].descriptors.get(&a[0]) {
                        Some(Descriptor::File(f)) => (
                            self.nodes[node].files[&f.borrow().path].contents.len(),
                            0o100644,
                        ),
                        Some(Descriptor::Socket(_)) => (0, 0o140777),
                        Some(Descriptor::Entropy) => (0, 0o20666),
                        Some(Descriptor::Epoll(_)) => (0, 0o600),
                        None if a[0] <= 2 => (0, 0o20666),
                        None => return Ok(Complete(-errno::EBADF)),
                    }
                };
                let mut bytes = vec![0; 144];
                put64(&mut bytes, 0, 1);
                put64(&mut bytes, 8, 1);
                put64(&mut bytes, 16, 1);
                put32(&mut bytes, 24, mode);
                put32(&mut bytes, 28, 1000);
                put32(&mut bytes, 32, 1000);
                put64(&mut bytes, 48, size as u64);
                put64(&mut bytes, 56, 4096);
                put64(&mut bytes, 64, (size as u64).div_ceil(512));
                for offset in [72, 88, 104] {
                    put64(&mut bytes, offset, self.config.epoch_ns / 1_000_000_000);
                }
                self.nodes[node].process.write(output, &bytes)?;
                0
            }
            21 => {
                let name = self.nodes[node].process.cstring(a[0])?;
                let name = self.absolute_path(node, &name);
                if name == "/" || self.nodes[node].files.contains_key(&name) {
                    0
                } else {
                    -errno::ENOENT
                }
            }
            79 => {
                let mut data = self.nodes[node].cwd.as_bytes().to_vec();
                data.push(0);
                if a[1] < data.len() as u64 {
                    -errno::ERANGE
                } else {
                    self.nodes[node].process.write(a[0], &data)?;
                    data.len() as i64
                }
            }
            80 => {
                let name = self.nodes[node].process.cstring(a[0])?;
                let target = self.absolute_path(node, &name);
                if target != "/" {
                    return Ok(Complete(-errno::ENOENT));
                }
                self.nodes[node].cwd = target;
                0
            }
            63 => {
                let mut data = vec![0; 390];
                for (index, text) in [
                    "Linux",
                    "seedbox",
                    "6.1.0-seedbox",
                    "#1 deterministic",
                    "x86_64",
                    "seedbox",
                ]
                .iter()
                .enumerate()
                {
                    data[index * 65..index * 65 + text.len()].copy_from_slice(text.as_bytes());
                }
                self.nodes[node].process.write(a[0], &data)?;
                0
            }
            96 => {
                let now = self.config.epoch_ns + self.now;
                if a[0] != 0 {
                    let data: Vec<_> = [now / 1_000_000_000, now % 1_000_000_000 / 1000]
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect();
                    self.nodes[node].process.write(a[0], &data)?;
                }
                if a[1] != 0 {
                    self.nodes[node].process.write(a[1], &[0; 8])?;
                }
                0
            }
            201 => {
                let seconds = (self.config.epoch_ns + self.now) / 1_000_000_000;
                if a[0] != 0 {
                    self.nodes[node]
                        .process
                        .write(a[0], &seconds.to_le_bytes())?;
                }
                seconds as i64
            }
            97 | 302 => {
                let (resource, output) = if req.number == 97 {
                    (a[0], a[1])
                } else {
                    if a[0] != 0 && a[0] != self.nodes[node].process.pid {
                        return Ok(Complete(-errno::EINVAL));
                    }
                    if a[2] != 0 {
                        return Err(Error::Unsupported(
                            "changing resource limits is not modeled".into(),
                        ));
                    }
                    (a[1], a[3])
                };
                let (soft, hard): (u64, u64) = match resource {
                    3 => (1024 * 1024, 1024 * 1024),
                    7 => (1024, 1024),
                    9 => (256 * 1024 * 1024, 256 * 1024 * 1024),
                    _ => (u64::MAX, u64::MAX),
                };
                if output != 0 {
                    let data: Vec<_> = [soft, hard].iter().flat_map(|v| v.to_le_bytes()).collect();
                    self.nodes[node].process.write(output, &data)?;
                }
                0
            }
            98 => {
                if ![0, 1, u64::MAX].contains(&a[0]) {
                    return Ok(Complete(-errno::EINVAL));
                }
                let mut data = vec![0; 144];
                let cpu = self.nodes[node].cpu_ns;
                put64(&mut data, 0, cpu / 1_000_000_000);
                put64(&mut data, 8, cpu % 1_000_000_000 / 1000);
                self.nodes[node].process.write(a[1], &data)?;
                0
            }
            99 => {
                let mut data = vec![0; 112];
                put64(&mut data, 0, self.now / 1_000_000_000);
                put64(&mut data, 32, 256 * 1024 * 1024);
                put64(&mut data, 40, 128 * 1024 * 1024);
                data[80..82].copy_from_slice(&1u16.to_le_bytes());
                put32(&mut data, 104, 1);
                self.nodes[node].process.write(a[0], &data)?;
                0
            }
            102 | 104 | 107 | 108 => 1000,
            95 => {
                let old = self.nodes[node].umask;
                self.nodes[node].umask = a[0] & 0o777;
                old as i64
            }
            110 => 0,
            204 => {
                if a[1] < 8 {
                    return Ok(Complete(-errno::EINVAL));
                }
                self.nodes[node].process.write(a[2], &1u64.to_le_bytes())?;
                8
            }
            229 => {
                if a[0] > 1 {
                    return Ok(Complete(-errno::EINVAL));
                }
                if a[1] != 0 {
                    let data: Vec<_> = [0u64, self.config.scheduler.tick_ns]
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect();
                    self.nodes[node].process.write(a[1], &data)?;
                }
                0
            }
            157 => {
                if a[0] != 15 {
                    return Err(Error::Unsupported(format!(
                        "unsupported prctl option {}",
                        a[0]
                    )));
                }
                self.nodes[node].process.read(a[1], 16)?;
                0
            }
            _ => return self.network_syscall(node, req),
        };
        Ok(Complete(result))
    }
}
