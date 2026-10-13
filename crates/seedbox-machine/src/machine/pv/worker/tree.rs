//! FDT for one CPU, PV clock, PL011 and a lifecycle-only virtio console.
use super::super::boot::{DTB, RAM_BASE, RAM_BYTES};
use sha2::{Digest, Sha256};
struct Tree {
    body: Vec<u8>,
    names: Vec<u8>,
}
impl Tree {
    fn cell(&mut self, value: u32) {
        self.body.extend(value.to_be_bytes());
    }
    fn align(&mut self) {
        while !self.body.len().is_multiple_of(4) {
            self.body.push(0);
        }
    }
    fn node(&mut self, name: &str) {
        self.cell(1);
        self.body.extend(name.bytes());
        self.body.push(0);
        self.align();
    }
    fn end(&mut self) {
        self.cell(2);
    }
    fn prop(&mut self, name: &str, value: &[u8]) {
        let offset = self.names.len() as u32;
        self.names.extend(name.bytes());
        self.names.push(0);
        self.cell(3);
        self.cell(value.len() as u32);
        self.cell(offset);
        self.body.extend(value);
        self.align();
    }
    fn cells(&mut self, name: &str, value: &[u32]) {
        self.prop(
            name,
            &value
                .iter()
                .flat_map(|x| x.to_be_bytes())
                .collect::<Vec<_>>(),
        );
    }
    fn string(&mut self, name: &str, value: &str) {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.prop(name, &bytes);
    }
    fn reg(&mut self, address: u64, size: u64) {
        self.cells(
            "reg",
            &[
                (address >> 32) as u32,
                address as u32,
                (size >> 32) as u32,
                size as u32,
            ],
        );
    }
}
pub(super) fn build(
    initrd_address: u64,
    initrd_len: usize,
    seed: u64,
    command_line: &str,
) -> Result<Vec<u8>, String> {
    let mut t = Tree {
        body: Vec::new(),
        names: Vec::new(),
    };
    t.node("");
    t.string("compatible", "seedbox,arm64-pv");
    t.string("model", "Seedbox PV machine");
    t.cells("#address-cells", &[2]);
    t.cells("#size-cells", &[2]);
    t.cells("interrupt-parent", &[1]);
    t.node("chosen");
    t.string("bootargs", command_line);
    t.string("stdout-path", "/pl011@9000000");
    t.prop("linux,initrd-start", &initrd_address.to_be_bytes());
    t.prop(
        "linux,initrd-end",
        &(initrd_address + initrd_len as u64).to_be_bytes(),
    );
    let entropy = Sha256::digest([b"seedbox pv boot v1".as_slice(), &seed.to_le_bytes()].concat());
    t.prop("rng-seed", &entropy);
    t.prop("kaslr-seed", &entropy[..8]);
    t.end();
    t.node("memory@40000000");
    t.string("device_type", "memory");
    t.reg(RAM_BASE, RAM_BYTES as u64);
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
        &[0, 0x0800_0000, 0, 0x10000, 0, 0x080a_0000, 0, 0x20000],
    );
    t.end();
    t.node("pclk");
    t.string("compatible", "fixed-clock");
    t.cells("#clock-cells", &[0]);
    t.cells("clock-frequency", &[24_000_000]);
    t.cells("phandle", &[2]);
    t.end();
    t.node("pl011@9000000");
    t.prop("compatible", b"arm,pl011\0arm,primecell\0");
    t.reg(0x0900_0000, 4096);
    t.cells("interrupts", &[0, 1, 4]);
    t.cells("clocks", &[2, 2]);
    t.prop("clock-names", b"uartclk\0apb_pclk\0");
    t.end();
    t.node("psci");
    t.string("compatible", "arm,psci-0.2");
    t.string("method", "hvc");
    t.end();
    t.node("seedbox-time@a000000");
    t.string("compatible", "seedbox,pv-time-v3");
    t.cells("reg", &[0, 0x0a00_0000, 0, 16384, 0, 0x0a01_0000, 0, 16384]);
    t.prop("reg-names", b"time\0event\0");
    t.cells("interrupts", &[0, 8, 4]);
    t.end();
    t.node("virtio@b010000");
    t.string("compatible", "virtio,mmio");
    t.reg(0x0b01_0000, 16384);
    t.cells("interrupts", &[0, 10, 4]);
    t.prop("dma-coherent", &[]);
    t.end();
    t.end();
    t.cell(9);
    let strings = 56 + t.body.len();
    let header = [
        0xd00dfeed,
        (strings + t.names.len()) as u32,
        56,
        strings as u32,
        40,
        17,
        16,
        0,
        t.names.len() as u32,
        t.body.len() as u32,
    ];
    let mut bytes: Vec<u8> = header.into_iter().flat_map(u32::to_be_bytes).collect();
    bytes.extend([0; 16]);
    bytes.extend(t.body);
    bytes.extend(t.names);
    if bytes.len() > 65536 {
        return Err("PV kernel arguments exceed device-tree capacity".into());
    }
    debug_assert!(DTB >= RAM_BASE);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kernel_arguments_are_preserved_and_capacity_is_checked() {
        let arguments = "rdinit=/init custom=value console=other console=last";
        let bytes = build(0x42000000, 512, 7, arguments).unwrap();
        let mut expected = arguments.as_bytes().to_vec();
        expected.push(0);
        assert!(
            bytes
                .windows(expected.len())
                .any(|window| window == expected)
        );
        assert!(
            !bytes
                .windows(b"earlycon=pl011".len())
                .any(|window| window == b"earlycon=pl011")
        );
        assert!(build(0x42000000, 512, 7, &"x".repeat(65536)).is_err());
    }
}
