// Portable ABI/source checks: the x86 VMM itself requires Linux KVM.
const MACHINE: &str = include_str!("../../seedbox-vmm/src/machine.rs");
const HEADER: &str = include_str!("../../../kernel/seedbox.h");

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .split_once(start)
        .unwrap()
        .1
        .split_once(end)
        .unwrap()
        .0
}

#[test]
fn kernel_and_vmm_pin_the_agreed_extension_indices() {
    let body = section(HEADER, "struct seedbox_time_page {", "};");
    let mut fields = std::collections::BTreeMap::new();
    let mut index = 0;
    for line in body
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("u64 "))
    {
        let decl = section(line, "u64 ", ";");
        let (name, count) = match decl.split_once('[') {
            Some((name, count)) => (name, count.trim_end_matches(']').parse::<usize>().unwrap()),
            None => (decl, 1),
        };
        fields.insert(name, (index, count));
        index += count;
    }
    let kernel = include_str!("../../../kernel/seedbox.c");
    for (name, constant, offset, count) in [
        ("syscalls", "PAGE_SYSCALLS", 3, 1),
        ("pic_imr", "PAGE_PIC_IMR", 4, 1),
        ("net_isr", "PAGE_NET_ISR", 5, 1),
        ("pv_clock", "PAGE_PV_CLOCK", 6, 1),
        ("bound", "PAGE_BOUND", 7, 1),
        ("syscall_target", "PAGE_SYSCALL_TARGET", 8, 1),
        ("entropy_gen", "PAGE_ENTROPY_GEN", 9, 1),
        ("entropy", "PAGE_ENTROPY", 10, 4),
    ] {
        assert_eq!(fields[name], (offset, count));
        assert!(MACHINE.contains(&format!("const {constant}: usize = {offset};")));
        if offset >= 6 {
            for word in 0..count {
                let member = if count == 1 {
                    name.to_owned()
                } else {
                    format!("{name}[{word}]")
                };
                assert!(kernel.contains(&format!(
                    "BUILD_BUG_ON(offsetof(struct seedbox_time_page, {member}) != {} * sizeof(u64));",
                    offset + word
                )));
            }
        }
    }
    assert_eq!(index, 14);
    assert!(HEADER.contains("#define SEEDBOX_ABI\t\t3\n"));
    assert!(MACHINE.contains("pub const ABI_PAGE_RESEED: u64 = 3;"));
}

#[test]
fn extension_writes_stay_behind_pv_and_parked_abi_3_time_fork_gates() {
    let boot = section(
        MACHINE,
        "if spec.pv_clock {\n            unsafe",
        "\n        }",
    );
    assert!(boot.contains("write_volatile(page.cast::<u64>().add(PAGE_PV_CLOCK), 1)"));
    let release = section(MACHINE, "pub fn release(", "/// Freeze the machine");
    let write = release.find("self.page_set(PAGE_ENTROPY + i, v)").unwrap();
    for gate in [
        "self.world.begin_release()?;",
        "if self.barrier {",
        "if self.abi < ABI_PAGE_RESEED {",
    ] {
        assert!(release.find(gate).unwrap() < write);
    }
    let world = include_str!("../../seedbox-engine-core/src/world.rs");
    let guard = section(world, "pub fn begin_release(", "pub fn pause(");
    assert!(guard.find("if !self.parked {").unwrap() < guard.find("self.parked = false;").unwrap());
    assert!(release.contains("return Ok(Reseed::Unsupported);"));
    // Generation publication follows the payload, with no other runtime writer.
    assert!(
        write
            < release
                .find("self.page_set(PAGE_ENTROPY_GEN, gen)")
                .unwrap()
    );
    let runtime = MACHINE.split("\n#[cfg(test)]\nmod ").next().unwrap();
    assert_eq!(runtime.matches("self.page_set(PAGE_ENTROPY").count(), 2);
    assert_eq!(runtime.matches(".add(PAGE_PV_CLOCK), 1)").count(), 1);
    assert!(!runtime.contains("page_set(PAGE_BOUND"));
    assert!(!runtime.contains("page_set(PAGE_SYSCALL_TARGET"));
    let pv = include_str!("../../seedbox-vmm/src/machine/pv.rs");
    assert!(pv.contains("m.page_set(PAGE_BOUND, soft)"));
    assert!(pv.contains("m.page_set(PAGE_SYSCALL_TARGET, syscalls + left)"));
}

#[test]
fn undo_and_dedicated_time_page_digest_keep_the_six_field_prefix() {
    assert!(MACHINE.contains("pub const PAGE_FIELDS: usize = 6;"));
    let checkpoint = section(MACHINE, "fn checkpoint(", "fn rollback(");
    assert!(checkpoint
        .contains("let page: [u64; PAGE_FIELDS] = std::array::from_fn(|i| self.page_get(i));"));
    let rollback = section(MACHINE, "fn rollback(", "fn checked_page_time(");
    assert!(rollback.contains("for i in 0..PAGE_FIELDS {"));
    let digest = section(
        MACHINE,
        "parts.push((\"msrs\", hex(h)));",
        "parts.push((\"time_page\", hex(h)));",
    );
    assert!(digest.contains("for i in 0..PAGE_FIELDS {"));
    assert!(!digest.contains("PAGE_ENTROPY"));
    let undo = include_str!("../../seedbox-vmm/src/undo.rs");
    assert!(undo.contains("pub page: [u64; crate::machine::PAGE_FIELDS]"));
}
