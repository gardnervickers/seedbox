//! x86 test BIOS images for raw TCG programs.
//!
//! The CPU starts at the reset vector in real mode. A fixed [`STUB`] enters
//! 32-bit flat protected mode. It does not rely on QEMU skipping segment
//! limits, because LAPIC, ACPI and virtio-mmio registers lie near 4 GiB.
//! Execution then falls through to the program code, which starts at
//! [`PROGRAM`].
//!
//! The IDT routes vectors 0x20 and 0x21 to one handler, which writes the
//! LAPIC EOI and returns. Vector 0x20 is the LAPIC timer; vector 0x21 is a
//! routed device interrupt. All other vectors are absent, so any exception
//! ends in a triple fault (a guest reset).
//!
//! The stack is RAM below 0x8000, and RAM from 0x1000 is scratch. A program
//! stops idling only through `sti; hlt`, because x86 needs IF set for a
//! maskable interrupt to end HLT.
//!
//! Integration tests reach this builder as `test_support::raw_program`.

/// Instructions executed before the first program instruction.
pub const STUB: u64 = 12;
/// Linear (and EIP) address of the first program instruction.
pub const PROGRAM: u64 = 0xFFFF_0000 + STUB32.len() as u64;
/// ACPI GED sleep control: SLP_EN | SLP_TYP S5 powers the board off.
pub const GED_SLEEP: u32 = 0xFEA0_0200;
/// ACPI GED reset register: 0x42 requests a guest reset.
pub const GED_RESET: u32 = 0xFEA0_0202;

/// `mov byte [GED_SLEEP], 0x34`: one instruction that powers the board off.
pub const SHUTDOWN: [u8; 7] = [0xC6, 0x05, 0x00, 0x02, 0xA0, 0xFE, 0x34];

/// GDB register indexes in the 17-register x86-64 layout.
pub const RBX: usize = 1;
pub const RCX: usize = 2;
pub const RIP: usize = 16;

/// Reload the flat data segments and the stack, then fall into the program.
const STUB32: [u8; 16] = [
    0xB8, 0x10, 0x00, 0x00, 0x00, // mov eax, 0x10: flat data selector.
    0x8E, 0xD8, // mov ds, eax
    0x8E, 0xC0, // mov es, eax
    0x8E, 0xD0, // mov ss, eax
    0xBC, 0x00, 0x80, 0x00, 0x00, // mov esp, 0x8000
];
const IDT: usize = 0xF000;
const HANDLER: usize = 0xF200;
const GDT: usize = 0xFF00;
const GDTR: usize = 0xFF20;
const IDTR: usize = 0xFF28;
const STUB16: usize = 0xFFC0;
const RESET: usize = 0xFFF0;
const ROM: u32 = 0xFFFF_0000;

/// A complete 64 KiB BIOS image running `code` after the stub.
pub fn bios(code: &[u8]) -> Vec<u8> {
    let mut image = vec![0; super::PROGRAM_BYTES];
    let put = |image: &mut Vec<u8>, at: usize, bytes: &[u8]| {
        image[at..at + bytes.len()].copy_from_slice(bytes);
    };
    assert!(STUB32.len() + code.len() <= IDT, "program overlaps the IDT");
    put(&mut image, 0, &STUB32);
    put(&mut image, STUB32.len(), code);
    // Null, flat 32-bit code (0x08) and flat data (0x10) descriptors.
    let gdt = [0u64, 0x00CF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF];
    for (index, descriptor) in gdt.iter().enumerate() {
        put(&mut image, GDT + 8 * index, &descriptor.to_le_bytes());
    }
    let pointer = |limit: u16, base: usize| {
        let mut bytes = limit.to_le_bytes().to_vec();
        bytes.extend((ROM + base as u32).to_le_bytes());
        bytes
    };
    put(&mut image, GDTR, &pointer(23, GDT));
    // Present 32-bit interrupt gates for vectors 0x20 and 0x21 only.
    put(&mut image, IDTR, &pointer(0x22 * 8 - 1, IDT));
    let handler = ROM + HANDLER as u32;
    for vector in [0x20, 0x21] {
        let mut gate = (handler as u16).to_le_bytes().to_vec();
        gate.extend([0x08, 0x00, 0x00, 0x8E]);
        gate.extend(((handler >> 16) as u16).to_le_bytes());
        put(&mut image, IDT + 8 * vector, &gate);
    }
    // mov dword [0xFEE000B0], 0 (LAPIC EOI); iretd.
    put(
        &mut image,
        HANDLER,
        &[0xC7, 0x05, 0xB0, 0x00, 0xE0, 0xFE, 0, 0, 0, 0, 0xCF],
    );
    // Real mode, with the reset CS base 0xFFFF0000: CS-relative descriptor
    // loads, then PE and a far jump to the flat code segment at the ROM base.
    let [gdtr_low, gdtr_high] = (GDTR as u16).to_le_bytes();
    let [idtr_low, idtr_high] = (IDTR as u16).to_le_bytes();
    put(
        &mut image,
        STUB16,
        &[
            0x66, 0x2E, 0x0F, 0x01, 0x16, gdtr_low, gdtr_high, // o32 lgdt cs:[GDTR]
            0x66, 0x2E, 0x0F, 0x01, 0x1E, idtr_low, idtr_high, // o32 lidt cs:[IDTR]
            0x0F, 0x20, 0xC0, // mov eax, cr0
            0x0C, 0x01, // or al, 1: protection enable.
            0x0F, 0x22, 0xC0, // mov cr0, eax
            0x66, 0xEA, 0x00, 0x00, 0xFF, 0xFF, 0x08, 0x00, // jmp far 0x08:0xFFFF0000
        ],
    );
    // Reset vector: jmp near STUB16 within the reset code segment.
    let relative = (STUB16 as i32 - (RESET as i32 + 3)) as i16;
    put(&mut image, RESET, &[0xE9]);
    put(&mut image, RESET + 1, &relative.to_le_bytes());
    image
}

/// RAM scratch word written by [`storing_loop`].
pub const SCRATCH: u32 = 0x1000;
/// Instructions in [`Asm::untimed`].
pub const UNTIMED: u64 = 8 + 1 + 2 * UNTIMED_ITERATIONS;
const UNTIMED_ITERATIONS: u64 = 420;

/// `inc ebx; jmp` forever: EBX counts every second instruction.
pub fn counting_loop() -> Vec<u8> {
    Asm::default()
        .label("loop")
        .inc(Reg::Ebx)
        .jmp("loop")
        .image()
}

/// `mov edi, SCRATCH`, then `inc ebx; mov [edi], ebx; jmp` forever.
pub fn storing_loop() -> Vec<u8> {
    Asm::default()
        .mov_imm(Reg::Edi, SCRATCH)
        .label("loop")
        .inc(Reg::Ebx)
        .store_at(Reg::Edi, 0, Reg::Ebx)
        .jmp("loop")
        .image()
}

/// LAPIC register addresses used by the programs.
pub mod lapic {
    pub const SPURIOUS: u32 = 0xFEE0_00F0;
    /// IRR bits 32..63; vector 0x20 is bit 0.
    pub const IRR_1: u32 = 0xFEE0_0210;
    pub const TIMER: u32 = 0xFEE0_0320;
    pub const INITIAL_COUNT: u32 = 0xFEE0_0380;
    pub const DIVIDE: u32 = 0xFEE0_03E0;
}

/// x86 register numbers as encoded in instructions (not GDB indexes).
#[derive(Clone, Copy)]
pub enum Reg {
    Eax = 0,
    Ecx = 1,
    Edx = 2,
    Ebx = 3,
    Esi = 6,
    Edi = 7,
}

/// Condition codes for [`Asm::jcc`].
#[derive(Clone, Copy)]
pub enum Cond {
    Equal = 4,
    NotEqual = 5,
}

/// A minimal 32-bit x86 assembler. It has only the encodings these tests
/// use, each a single instruction, with labels for rel32 jumps.
#[derive(Default)]
pub struct Asm {
    bytes: Vec<u8>,
    labels: std::collections::HashMap<&'static str, usize>,
    fixups: Vec<(usize, &'static str)>,
}

impl Asm {
    fn emit(&mut self, bytes: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(bytes);
        self
    }
    fn imm(&mut self, value: u32) -> &mut Self {
        self.emit(&value.to_le_bytes())
    }
    pub fn label(&mut self, name: &'static str) -> &mut Self {
        self.labels.insert(name, self.bytes.len());
        self
    }
    /// `mov r32, imm32`.
    pub fn mov_imm(&mut self, register: Reg, value: u32) -> &mut Self {
        self.emit(&[0xB8 + register as u8]).imm(value)
    }
    /// `mov dword [address], imm32`.
    pub fn store_imm(&mut self, address: u32, value: u32) -> &mut Self {
        self.emit(&[0xC7, 0x05]).imm(address).imm(value)
    }
    /// `mov r32, [address]`.
    pub fn load(&mut self, register: Reg, address: u32) -> &mut Self {
        self.emit(&[0x8B, 0x05 | (register as u8) << 3])
            .imm(address)
    }
    /// `mov [base + offset], r32`.
    pub fn store_at(&mut self, base: Reg, offset: u32, register: Reg) -> &mut Self {
        let modrm = 0x80 | (register as u8) << 3 | base as u8;
        self.emit(&[0x89, modrm]).imm(offset)
    }
    /// `mov byte [address], imm8`.
    pub fn store_byte(&mut self, address: u32, value: u8) -> &mut Self {
        self.emit(&[0xC6, 0x05]).imm(address).emit(&[value])
    }
    /// `mov [base + offset], r16`.
    pub fn store16_at(&mut self, base: Reg, offset: u32, register: Reg) -> &mut Self {
        let modrm = 0x80 | (register as u8) << 3 | base as u8;
        self.emit(&[0x66, 0x89, modrm]).imm(offset)
    }
    /// `mov dword [base + offset], imm32`.
    pub fn store_imm_at(&mut self, base: Reg, offset: u32, value: u32) -> &mut Self {
        self.emit(&[0xC7, 0x80 | base as u8]).imm(offset).imm(value)
    }
    /// `mov r32, [base + offset]`.
    pub fn load_at(&mut self, register: Reg, base: Reg, offset: u32) -> &mut Self {
        let modrm = 0x80 | (register as u8) << 3 | base as u8;
        self.emit(&[0x8B, modrm]).imm(offset)
    }
    /// `add r32, imm32`.
    pub fn add_imm(&mut self, register: Reg, value: u32) -> &mut Self {
        self.emit(&[0x81, 0xC0 | register as u8]).imm(value)
    }
    /// `add dst, src`.
    pub fn add(&mut self, destination: Reg, source: Reg) -> &mut Self {
        self.emit(&[0x01, 0xC0 | (source as u8) << 3 | destination as u8])
    }
    /// `cmp r32, imm32`.
    pub fn cmp_imm(&mut self, register: Reg, value: u32) -> &mut Self {
        self.emit(&[0x81, 0xF8 | register as u8]).imm(value)
    }
    pub fn inc(&mut self, register: Reg) -> &mut Self {
        self.emit(&[0x40 + register as u8])
    }
    pub fn dec(&mut self, register: Reg) -> &mut Self {
        self.emit(&[0x48 + register as u8])
    }
    /// `mov al, imm8`.
    pub fn mov_al(&mut self, value: u8) -> &mut Self {
        self.emit(&[0xB0, value])
    }
    /// `out imm8, al`.
    pub fn out(&mut self, port: u8) -> &mut Self {
        self.emit(&[0xE6, port])
    }
    /// `out dx, al`.
    pub fn out_dx(&mut self) -> &mut Self {
        self.emit(&[0xEE])
    }
    /// `in al, dx`.
    pub fn in_dx(&mut self) -> &mut Self {
        self.emit(&[0xEC])
    }
    /// `rep movsd`: copy ECX doublewords from `[esi]` to `[edi]`.
    pub fn rep_movsd(&mut self) -> &mut Self {
        self.emit(&[0xF3, 0xA5])
    }
    pub fn hlt(&mut self) -> &mut Self {
        self.emit(&[0xF4])
    }
    pub fn sti(&mut self) -> &mut Self {
        self.emit(&[0xFB])
    }
    pub fn cli(&mut self) -> &mut Self {
        self.emit(&[0xFA])
    }
    pub fn ud2(&mut self) -> &mut Self {
        self.emit(&[0x0F, 0x0B])
    }
    /// `jmp $`: spin on one instruction.
    pub fn spin(&mut self) -> &mut Self {
        self.emit(&[0xEB, 0xFE])
    }
    /// `jmp r32`: an absolute indirect jump.
    pub fn jmp_reg(&mut self, register: Reg) -> &mut Self {
        self.emit(&[0xFF, 0xE0 | register as u8])
    }
    /// `jmp rel32` to a label.
    pub fn jmp(&mut self, target: &'static str) -> &mut Self {
        self.emit(&[0xE9]);
        self.fixups.push((self.bytes.len(), target));
        self.imm(0)
    }
    /// `jcc rel32` to a label.
    pub fn jcc(&mut self, condition: Cond, target: &'static str) -> &mut Self {
        self.emit(&[0x0F, 0x80 + condition as u8]);
        self.fixups.push((self.bytes.len(), target));
        self.imm(0)
    }
    /// Raw bytes, such as [`SHUTDOWN`].
    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.emit(bytes)
    }
    /// Arm the LAPIC timer: software-enable the LAPIC, divide by one, and
    /// unmask a one-shot vector 0x20. Four instructions; the last, the count
    /// store, sets the deadline to its own completion time plus `count + 1`.
    pub fn lapic_timer(&mut self, count: u32) -> &mut Self {
        self.store_imm(lapic::SPURIOUS, 0x1FF)
            .store_imm(lapic::DIVIDE, 0xB)
            .store_imm(lapic::TIMER, 0x20)
            .store_imm(lapic::INITIAL_COUNT, count)
    }
    /// Stop the board timers that run from reset (PIT channel 0 and the
    /// RTC update cycle). Eight instructions. The PIT's one-shot reload
    /// still fires twice in the following 840 ns, so callers that need an
    /// idle CPU with no pending timer first wait past it.
    pub fn quiesce_board_timers(&mut self) -> &mut Self {
        // PIT channel 0: low byte only, mode 0, count 1.
        self.mov_al(0x10).out(0x43).mov_al(1).out(0x40);
        // RTC register A: hold the dividers in reset.
        self.mov_al(0x0A).out(0x70).mov_al(0x60).out(0x71)
    }
    /// [`Self::quiesce_board_timers`], then a countdown that outlasts the
    /// PIT's last transition at factor 1000: [`UNTIMED`] instructions, after
    /// which no virtual timer is pending.
    pub fn untimed(&mut self) -> &mut Self {
        self.quiesce_board_timers()
            .mov_imm(Reg::Ecx, UNTIMED_ITERATIONS as u32)
            .label("untimed")
            .dec(Reg::Ecx)
            .jcc(Cond::NotEqual, "untimed")
    }
    /// Resolve labels and return the program code.
    pub fn finish(&mut self) -> Vec<u8> {
        for (at, target) in std::mem::take(&mut self.fixups) {
            let relative = self.labels[target] as i64 - (at as i64 + 4);
            self.bytes[at..at + 4].copy_from_slice(&(relative as i32).to_le_bytes());
        }
        std::mem::take(&mut self.bytes)
    }
    /// [`Self::finish`] wrapped in a complete BIOS image.
    pub fn image(&mut self) -> Vec<u8> {
        bios(&self.finish())
    }
}
