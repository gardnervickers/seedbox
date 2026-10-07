//! Minimal flattened device tree writer: big-endian cells, explicit phandles.
use crate::boot::{BootLayout, DEFAULT_COMMAND_LINE, DTB_RESERVATION};
use crate::*;
use sha2::{Digest, Sha256};
struct Tree {
    structure: Vec<u8>,
    strings: Vec<u8>,
}
impl Tree {
    fn word(&mut self, n: u32) {
        self.structure.extend(n.to_be_bytes());
    }
    fn align(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }
    fn node(&mut self, name: &str) {
        self.word(1);
        self.structure.extend(name.bytes());
        self.structure.push(0);
        self.align();
    }
    fn end(&mut self) {
        self.word(2);
    }
    fn prop(&mut self, name: &str, value: &[u8]) {
        let off = self.strings.len() as u32;
        self.strings.extend(name.bytes());
        self.strings.push(0);
        self.word(3);
        self.word(value.len() as u32);
        self.word(off);
        self.structure.extend(value);
        self.align();
    }
    fn cells(&mut self, name: &str, cells: &[u32]) {
        self.prop(
            name,
            &cells
                .iter()
                .flat_map(|c| c.to_be_bytes())
                .collect::<Vec<_>>(),
        );
    }
    fn string(&mut self, name: &str, value: &str) {
        let mut b = value.as_bytes().to_vec();
        b.push(0);
        self.prop(name, &b);
    }
    fn reg(&mut self, addr: u64, size: u64) {
        self.cells(
            "reg",
            &[
                (addr >> 32) as u32,
                addr as u32,
                (size >> 32) as u32,
                size as u32,
            ],
        );
    }
    fn finish(mut self) -> Vec<u8> {
        self.word(9);
        let struct_off = 56u32; // 40-byte header + 16-byte reserve terminator
        let strings_off = struct_off + self.structure.len() as u32;
        let header = [
            0xd00dfeed,
            strings_off + self.strings.len() as u32,
            struct_off,
            strings_off,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ];
        let mut out: Vec<u8> = header.iter().flat_map(|n| n.to_be_bytes()).collect();
        out.extend([0; 16]);
        out.extend(self.structure);
        out.extend(self.strings);
        out
    }
}
pub fn build(seed: u64, initrd_len: usize) -> Result<Vec<u8>> {
    if initrd_len == 0 || initrd_len > (DTB_ADDR - INITRD_ADDR) as usize {
        return Err("initrd is empty or overlaps the device tree".into());
    }
    Ok(build_tree(
        seed,
        RAM_SIZE as u64,
        INITRD_ADDR,
        INITRD_ADDR + initrd_len as u64,
        DEFAULT_COMMAND_LINE,
    ))
}

/// Serialize only validated boot parameters and check the actual byte length
/// against the placement reservation before any guest memory is allocated.
pub fn build_for_layout(layout: &BootLayout<'_>) -> Result<Vec<u8>> {
    let initrd = layout.initrd_range();
    let bytes = build_tree(
        layout.seed(),
        layout.memory_bytes() as u64,
        initrd.start,
        initrd.end,
        layout.command_line(),
    );
    if bytes.len() as u64 > DTB_RESERVATION {
        return Err("generated device tree exceeds boot layout reservation".into());
    }
    layout.ram_offset(layout.dtb_range().start, bytes.len())?;
    Ok(bytes)
}

fn build_tree(
    seed: u64,
    memory_bytes: u64,
    initrd_start: u64,
    initrd_end: u64,
    command_line: &str,
) -> Vec<u8> {
    let mut t = Tree {
        structure: vec![],
        strings: vec![],
    };
    t.node("");
    t.string("compatible", "seedbox,arm64-pv");
    t.string("model", "Seedbox arm64 PV ABI 2");
    t.cells("#address-cells", &[2]);
    t.cells("#size-cells", &[2]);
    t.cells("interrupt-parent", &[1]);
    t.node("chosen");
    t.string("bootargs", command_line);
    t.string("stdout-path", "/pl011@9000000");
    t.prop("linux,initrd-start", &initrd_start.to_be_bytes());
    t.prop("linux,initrd-end", &initrd_end.to_be_bytes());
    let mut seed_hash = Sha256::new();
    seed_hash.update(b"seedbox arm64 rng v1\0");
    seed_hash.update(seed.to_le_bytes());
    let entropy = seed_hash.finalize();
    t.prop("rng-seed", &entropy);
    t.prop("kaslr-seed", &entropy[..8]);
    t.end();
    t.node("memory@40000000");
    t.string("device_type", "memory");
    t.reg(RAM_BASE, memory_bytes);
    t.end();
    t.node("cpus");
    t.cells("#address-cells", &[1]);
    t.cells("#size-cells", &[0]);
    t.node("cpu@0");
    t.string("device_type", "cpu");
    t.string("compatible", "arm,arm-v8");
    t.cells("reg", &[0]);
    t.string("enable-method", "psci");
    t.end();
    t.end();
    t.node("intc@8000000");
    t.string("compatible", "arm,gic-v3");
    t.cells("#interrupt-cells", &[3]);
    t.prop("interrupt-controller", &[]);
    t.cells("phandle", &[1]);
    t.cells(
        "reg",
        &[
            0,
            GIC_DIST as u32,
            0,
            0x10000,
            0,
            GIC_REDIST as u32,
            0,
            0x20000,
        ],
    );
    t.end();
    t.node("apb-pclk");
    t.string("compatible", "fixed-clock");
    t.cells("#clock-cells", &[0]);
    t.cells("clock-frequency", &[24_000_000]);
    t.cells("phandle", &[2]);
    t.end();
    t.node("pl011@9000000");
    t.prop("compatible", b"arm,pl011\0arm,primecell\0");
    t.reg(UART_ADDR, 0x1000);
    t.cells("interrupts", &[0, UART_SPI - 32, 4]);
    t.cells("clocks", &[2, 2]);
    t.prop("clock-names", b"uartclk\0apb_pclk\0");
    t.end();
    t.node("psci");
    t.string("compatible", "arm,psci-0.2");
    t.string("method", "hvc");
    t.end();
    t.node("seedbox-time@a000000");
    t.string("compatible", "seedbox,pv-time-v2");
    t.cells(
        "reg",
        &[
            0,
            TIME_ADDR as u32,
            0,
            PAGE_SIZE as u32,
            0,
            EVENT_ADDR as u32,
            0,
            PAGE_SIZE as u32,
        ],
    );
    t.prop("reg-names", b"time\0event\0");
    t.cells("interrupts", &[0, TIMER_SPI - 32, 4]);
    t.end();
    t.end();
    t.finish()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independently_parse_layout_and_seeds() {
        let bytes = build(1, 4096).unwrap();
        let f = ::fdt::Fdt::new(&bytes).unwrap();
        assert_eq!(f.cpus().count(), 1);
        let memory = f.memory().regions().next().unwrap();
        assert_eq!(memory.starting_address as u64, RAM_BASE);
        assert_eq!(memory.size, Some(RAM_SIZE));
        assert!(f.find_node("/timer").is_none());
        let pv = f.find_node("/seedbox-time@a000000").unwrap();
        assert_eq!(
            pv.property("interrupts").unwrap().value,
            &[0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 4]
        );
        assert_eq!(bytes, build(1, 4096).unwrap());
        assert_ne!(bytes, build(2, 4096).unwrap());
        assert!(build(1, 0).is_err());
        assert!(build(1, usize::MAX).is_err());
    }
}
