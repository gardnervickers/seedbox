//! Closed persistence of the Rust-owned closure; QEMU migration stays opaque.
//!
//! The body retains factory policy, logical/active boot identity, ended RAM
//! evidence and pending reports before the six raw body lengths. No
//! source/engine provenance is recorded.
//! `SBXTCG01`, LE u32 version, u8 guest arch (1=x86-64), u8 native
//! format (0=none, 1=patched-QEMU migration), two zero reserved bytes, LE u64
//! body length. The body contains Linux kind (1), memory MiB, optional MAC,
//! command line/epoch, reached/state/seed/pending/shutdown, empty stopped-boundary
//! writes/input, the common closed bridge, disk base kind/capacity and client
//! ownership. Six LE u64 lengths precede six raw bodies: kernel, initramfs,
//! firmware, migration, client SBXSTOR1, immutable image base. Strings are
//! u64-length UTF-8; booleans/options use only 0/1; states use 0=Ready,
//! 1=Blocked(optional deadline), 2=Shutdown, 3=Crashed. Unknown tags, reserved
//! bits and trailing bytes reject. No host paths or executable identity occur.
use super::{Boot, ExitReason, Manifest, Snapshot, State, Timestamp, COMMAND_LINE_SIZE};
use seedbox_machine::definition::{DiskImage, MacAddress};
use seedbox_machine::snapshot::{reserve, CodecError, DecodeLimits, Reader, Result, Writer};
use std::io::{Read, Write};

const MAGIC: &[u8; 8] = b"SBXTCG01";
const VERSION: u32 = 1;
const HEADER: u64 = 24;
const ARCH: u8 = 1; // Guest x86-64, on every supported host.
const NATIVE: u8 = 1; // Opaque patched-QEMU migration, not a host ABI.
fn invalid(s: &'static str) -> CodecError {
    CodecError::Invalid(s)
}
fn sum(values: &[usize]) -> Result<u64> {
    values.iter().try_fold(0u64, |a, &b| {
        a.checked_add(b as u64).ok_or(invalid("length overflow"))
    })
}
fn timestamp<R: Read>(r: &mut Reader<'_, R>) -> Result<Timestamp> {
    Ok(Timestamp::from_nanos(r.u64()?))
}
fn option_time<R: Read>(r: &mut Reader<'_, R>) -> Result<Option<Timestamp>> {
    if r.boolean()? {
        Ok(Some(timestamp(r)?))
    } else {
        Ok(None)
    }
}
fn write_time<W: Write>(w: &mut Writer<'_, W>, at: Option<Timestamp>) -> Result<()> {
    w.boolean(at.is_some())?;
    if let Some(at) = at {
        w.u64(at.as_nanos())?;
    }
    Ok(())
}
fn write_state<W: Write>(w: &mut Writer<'_, W>, state: State) -> Result<()> {
    match state {
        State::Ready => w.u8(0),
        State::Blocked { deadline } => {
            w.u8(1)?;
            write_time(w, deadline)
        }
        State::Exited(ExitReason::Shutdown) => w.u8(2),
        State::Exited(ExitReason::Crashed) => w.u8(3),
        _ => Err(invalid("machine state")),
    }
}
fn read_state<R: Read>(r: &mut Reader<'_, R>) -> Result<State> {
    match r.u8()? {
        0 => Ok(State::Ready),
        1 => Ok(State::Blocked {
            deadline: option_time(r)?,
        }),
        2 => Ok(State::Exited(ExitReason::Shutdown)),
        3 => Ok(State::Exited(ExitReason::Crashed)),
        _ => Err(invalid("machine state")),
    }
}
fn validate_scalars(s: &Snapshot) -> Result<()> {
    super::retained::validate(s)?;
    let live = s.image.is_some();
    if s.disk.is_some() != (s.manifest.disk.is_some() && !live) {
        return Err(invalid("TCG snapshot disk does not match its manifest"));
    }
    if s.manifest.memory_mib == 0 {
        return Err(invalid("guest memory"));
    }
    if let Some(mac) = s.manifest.network {
        MacAddress::new(mac).map_err(|_| invalid("network MAC"))?;
    }
    if s.reached.as_nanos() > i64::MAX as u64
        || !matches!(
            s.state,
            State::Ready
                | State::Blocked { .. }
                | State::Exited(ExitReason::Shutdown | ExitReason::Crashed)
        )
        || matches!(s.state, State::Blocked { deadline: Some(d) } if d <= s.reached)
        || (!live && s.pending_boots == 0 && !matches!(s.state, State::Exited(_)))
        || (live && s.manifest.disk.is_some() && matches!(s.state, State::Exited(_)))
    {
        return Err(invalid("machine state or blocked deadline"));
    }
    // Source run drains input, output and prepared replies before capture;
    // construction is the only pending-boot boundary. Never resume a run Err.
    // This relies on accepted input being delivered by the end of each run.
    if !s.input.is_empty()
        || !s.writes.is_empty()
        || s.pending_boots > 1
        || s.pending_at.is_some()
        || (s.pending_boots == 1
            && (live
                || s.reached.as_nanos() != 0
                || s.boot_seed != 0
                || s.state != State::Ready
                || s.shutdown_requested))
    {
        return Err(invalid("capture boundary ownership"));
    }
    Ok(())
}
type BootParts<'a> = (&'a [u8], &'a [u8], &'a [u8], &'a str, u32);
fn boot(boot: &Boot) -> Result<BootParts<'_>> {
    match boot {
        Boot::Linux {
            kernel,
            initramfs,
            firmware,
            command_line,
            epoch_seconds,
        } => Ok((kernel, initramfs, firmware, command_line, *epoch_seconds)),
        #[cfg(any(test, feature = "test-support"))]
        Boot::Program(_) => Err(invalid("unsupported persisted boot kind")),
    }
}
fn validate_command(command: &str) -> Result<()> {
    if command.contains('\0') || command.len() >= COMMAND_LINE_SIZE {
        return Err(invalid("kernel command line"));
    }
    Ok(())
}
fn validate_boot(kernel: &[u8], initrd: &[u8], firmware: &[u8], memory: u32) -> Result<()> {
    seedbox_machine::machine::assets::bzimage(kernel).map_err(|_| invalid("x86-64 bzImage"))?;
    seedbox_machine::machine::assets::bios(firmware).map_err(|_| invalid("x86 BIOS"))?;
    if initrd.len() as u64 > u64::from(memory) * 1024 * 1024 {
        return Err(invalid("initramfs exceeds guest RAM"));
    }
    Ok(())
}
fn validate_disk(bytes: Option<&[u8]>, disk: Option<&DiskImage>) -> Result<()> {
    if let Some(bytes) = bytes {
        let disk = disk.ok_or(invalid("TCG snapshot disk does not match its manifest"))?;
        seedbox_machine::machine::disk::codec::validate_overlays(bytes, disk.sectors())
            .map_err(|_| invalid("disk overlays"))?;
    }
    Ok(())
}
fn write_body<W: Write>(s: &Snapshot, w: &mut Writer<'_, W>) -> Result<()> {
    // Exhaustive closure: adding an owned field requires an explicit decision.
    let Snapshot {
        manifest,
        reached,
        state,
        image,
        boot_seed,
        pending_boots,
        pending_at,
        shutdown_requested,
        writes,
        input,
        guest,
        disk,
        policy: _,
        logical_boot: _,
        active_boot: _,
        last_ended: _,
        last_evidence: _,
        native_witness: _,
        pending_ends: _,
    } = s;
    let Manifest {
        boot: boot_input,
        memory_mib,
        disk: base_disk,
        network,
    } = manifest;
    let (kernel, initrd, firmware, command, epoch) = boot(boot_input)?;
    w.u8(1)?; // Linux; raw test programs are not a persisted production boot kind.
    w.u32(*memory_mib)?;
    w.boolean(network.is_some())?;
    if let Some(mac) = network {
        w.raw(mac)?;
    }
    w.string(command)?;
    w.u32(epoch)?;
    w.u64(reached.as_nanos())?;
    write_state(w, *state)?;
    w.u64(*boot_seed)?;
    w.u64(*pending_boots)?;
    write_time(w, *pending_at)?;
    w.boolean(*shutdown_requested)?;
    w.u64(writes.len() as u64)?; // Boundary ownership, always zero.
    w.bytes(input)?;
    guest.write_snapshot(w)?;
    super::retained::write(s, w)?;
    match base_disk {
        None => w.u8(0)?,
        Some(disk) => {
            w.u8(if disk.image().is_some() { 2 } else { 1 })?;
            w.u64(disk.capacity_bytes())?;
        }
    }
    w.boolean(disk.is_some())?;
    let base = base_disk
        .as_ref()
        .and_then(DiskImage::image)
        .map_or(&[][..], |v| &v[..]);
    let bodies = [
        kernel,
        initrd,
        firmware,
        image.as_deref().unwrap_or(&[]),
        disk.as_deref().unwrap_or(&[]),
        base,
    ];
    for b in bodies {
        w.u64(b.len() as u64)?;
    }
    for b in bodies {
        w.raw(b)?;
    }
    Ok(())
}
fn body<R: Read>(r: &mut Reader<'_, R>, n: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reserve(&mut bytes, n)?;
    bytes.resize(n, 0);
    for chunk in bytes.chunks_mut(65536) {
        r.exact(chunk)?;
    }
    Ok(bytes)
}
impl Snapshot {
    /// Stream one complete versioned x86-64 TCG snapshot, independent of source
    /// files/processes: exact Linux boot inputs, lifecycle and bridge state,
    /// immutable disk base and both unpromoted overlays, or opaque live migration.
    /// Borrows all bodies in at most 64 KiB writes without copying the migration
    /// image. No execution, RNG, filesystem lookup or publication occurs. An I/O
    /// error leaves caller-owned partial output; this unchanged snapshot is reusable.
    /// Trusted bridge validation may allocate fallible identity scratch without
    /// decode quotas, never a second migration image. Test-only raw Program boots
    /// are not part of this persisted format.
    // Each concrete snapshot owns its encoding; a generic serializer would
    // expose private manifest, bridge and migration state.
    pub fn write_to(&self, output: &mut impl Write) -> Result<()> {
        validate_scalars(self)?;
        let (kernel, initrd, firmware, command, _) = boot(&self.manifest.boot)?;
        validate_command(command)?;
        validate_boot(kernel, initrd, firmware, self.manifest.memory_mib)?;
        self.guest
            .validate_snapshot(self.reached.as_nanos(), true)?;
        if self.image.as_ref().is_some_and(Vec::is_empty) {
            return Err(invalid("empty migration image"));
        }
        validate_disk(self.disk.as_deref(), self.manifest.disk.as_ref())?;
        let mut sink = std::io::sink();
        let mut count = Writer::new(&mut sink);
        write_body(self, &mut count)?;
        let mut w = Writer::new(output);
        w.raw(MAGIC)?;
        w.u32(VERSION)?;
        w.u8(ARCH)?;
        w.u8(if self.image.is_some() { NATIVE } else { 0 })?;
        w.u16(0)?;
        w.u64(count.count())?;
        write_body(self, &mut w)
    }
    /// Decode a closed, exact-EOF x86-64 TCG snapshot under caller-selected wire,
    /// item and logical-storage quotas, including validation indexes and ownership
    /// copies. Pure: no paths, tempdirs, executable lookup, process, native state
    /// application, clock or RNG. x86-64 is the guest architecture on every host;
    /// this is not a host-architecture admission gate. One retained migration Vec
    /// is read directly without a full-image staging copy. Zero disk bases stay
    /// compact; image bases and both client layers remain exact, never flattened.
    ///
    /// Validates Rust framing, boot structure, bridge and source-proven stopped
    /// boundary relations only. Opaque migration integrity does not certify QEMU
    /// parser, registers, RAM or DMA safety, or authenticity. The caller explicitly
    /// supplies compatible patched QEMU and external execution context to restore;
    /// later native restore can fail. Quotas are not RSS/native admission or CPU
    /// isolation. Readers/allocators retain ordinary blocking/failure behavior.
    // Budgeted import needs closed parsing here; neither private QMP JSON nor
    // native migration is a safe Rust schema.
    pub fn read_from(input: &mut impl Read, limits: &DecodeLimits) -> Result<Self> {
        if limits.encoded_bytes < HEADER {
            return Err(CodecError::Limit("encoded bytes"));
        }
        let mut fixed = [0u8; HEADER as usize];
        input.read_exact(&mut fixed)?;
        if &fixed[..8] != MAGIC || u32::from_le_bytes(fixed[8..12].try_into().unwrap()) != VERSION {
            return Err(invalid("format or version"));
        }
        let live = match fixed[13] {
            0 => false,
            NATIVE => true,
            _ => return Err(invalid("architecture or native format")),
        };
        if fixed[12] != ARCH || fixed[14..16] != [0, 0] {
            return Err(invalid("architecture or native format"));
        }
        let length = u64::from_le_bytes(fixed[16..24].try_into().unwrap());
        let mut r = Reader::new(input, length, HEADER, limits)?;
        if r.u8()? != 1 {
            return Err(invalid("unsupported persisted boot kind"));
        }
        let memory_mib = r.u32()?;
        if memory_mib == 0 {
            return Err(invalid("guest memory"));
        }
        let network = if r.boolean()? {
            let mut mac = [0; 6];
            r.exact(&mut mac)?;
            MacAddress::new(mac).map_err(|_| invalid("network MAC"))?;
            Some(mac)
        } else {
            None
        };
        let command_line = r.string()?;
        validate_command(&command_line)?;
        let epoch_seconds = r.u32()?;
        let reached = timestamp(&mut r)?;
        let state = read_state(&mut r)?;
        let boot_seed = r.u64()?;
        let pending_boots = r.u64()?;
        let pending_at = option_time(&mut r)?;
        let shutdown_requested = r.boolean()?;
        // Reject before reserving or reading forbidden output/input elements.
        if r.u64()? != 0 || r.u64()? != 0 {
            return Err(invalid("capture boundary ownership"));
        }
        let guest = seedbox_machine::pv::bridge::GuestBridge::read_snapshot(
            &mut r,
            reached.as_nanos(),
            true,
        )?;
        let aux = super::retained::read(&mut r)?;
        let tag = r.u8()?;
        let capacity = match tag {
            0 => None,
            1 | 2 => Some(r.u64()?),
            _ => return Err(invalid("disk base tag")),
        };
        let zero = capacity
            .map(DiskImage::zeroed)
            .transpose()
            .map_err(|_| invalid("disk capacity"))?;
        let client = r.boolean()?;
        if client != (capacity.is_some() && !live) {
            return Err(invalid("TCG snapshot disk does not match its manifest"));
        }
        let mut lengths = [0; 6];
        for n in &mut lengths {
            *n = r.blob_length()?;
        }
        let [nk, ni, nf, nm, no, nb] = lengths;
        if sum(&lengths)? != r.remaining() {
            return Err(invalid("raw body framing"));
        }
        if (live && nm == 0)
            || (!live && nm != 0)
            || (!client && no != 0)
            || (tag == 2 && Some(nb as u64) != capacity)
            || (tag != 2 && nb != 0)
            || ni as u64 > u64::from(memory_mib) * 1024 * 1024
        {
            return Err(invalid("raw body ownership or capacity"));
        }
        if nf == 0 || !nf.is_multiple_of(65536) {
            return Err(invalid("x86 BIOS"));
        }
        if client
            && (no < 32
                || no as u128
                    > seedbox_machine::machine::disk::codec::encoded_limit(capacity.unwrap() / 512))
        {
            return Err(invalid("overlay length exceeds disk geometry"));
        }
        // Vec -> Arc copies for three boot bodies and an image base. The single
        // opaque migration allocation and client overlay Vec are never copied.
        r.charge(sum(&[nk, ni, nf, nb])?)?;
        // Validate scalar boundary ownership before any raw-body allocation/read;
        // empty Arc boot placeholders allocate no element storage.
        let mut snapshot = Self {
            manifest: Manifest {
                boot: Boot::Linux {
                    kernel: [].into(),
                    initramfs: [].into(),
                    firmware: [].into(),
                    command_line,
                    epoch_seconds,
                },
                memory_mib,
                disk: zero,
                network,
            },
            reached,
            state,
            image: live.then(Vec::new),
            boot_seed,
            pending_boots,
            pending_at,
            shutdown_requested,
            writes: Vec::new(),
            input: Vec::new(),
            guest,
            disk: client.then(Vec::new),
            policy: aux.policy,
            logical_boot: aux.logical_boot,
            active_boot: aux.active_boot,
            last_ended: aux.last_ended,
            last_evidence: aux.last_evidence,
            native_witness: aux.native_witness,
            pending_ends: aux.pending_ends,
        };
        validate_scalars(&snapshot)?;
        let kernel = body(&mut r, nk)?;
        seedbox_machine::machine::assets::bzimage(&kernel)
            .map_err(|_| invalid("x86-64 bzImage"))?;
        let initramfs = body(&mut r, ni)?;
        let firmware = body(&mut r, nf)?;
        validate_boot(&kernel, &initramfs, &firmware, memory_mib)?;
        match &mut snapshot.manifest.boot {
            Boot::Linux {
                kernel: k,
                initramfs: i,
                firmware: f,
                ..
            } => {
                *k = kernel.into();
                *i = initramfs.into();
                *f = firmware.into();
            }
            #[cfg(any(test, feature = "test-support"))]
            Boot::Program(_) => return Err(invalid("unsupported persisted boot kind")),
        }
        // Exactly one allocation for the opaque QEMU migration image.
        if live {
            snapshot.image = Some(body(&mut r, nm)?);
        }
        if client {
            snapshot.disk = Some(body(&mut r, no)?);
        }
        validate_disk(snapshot.disk.as_deref(), snapshot.manifest.disk.as_ref())?;
        if tag == 2 {
            snapshot.manifest.disk =
                Some(DiskImage::from_bytes(body(&mut r, nb)?).map_err(|_| invalid("disk base"))?);
        }
        r.finish()?;
        Ok(snapshot)
    }
}
