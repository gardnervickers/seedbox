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
        ("timer_deadline", "PAGE_TIMER_DEADLINE", 14, 1),
        ("capabilities", "PAGE_CAPABILITIES", 15, 1),
        ("syscall_ns", "PAGE_SYSCALL_NS", 16, 1),
        ("syscalls0", "PAGE_SYSCALLS0", 17, 1),
        ("plan_time", "PAGE_PLAN_TIME", 18, 1),
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
    assert_eq!(index, 19);
    assert!(HEADER.contains("#define SEEDBOX_ABI\t\t4\n"));
    assert!(MACHINE.contains("pub const ABI_PAGE_RESEED: u64 = 3;"));
    assert!(MACHINE.contains("pub const ABI_PAGE_TIMER: u64 = 4;"));
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
