mod support;
use serde_json::json;
use std::fs;
use support::{json as result, python_api};

#[test]
fn arm_configuration_requires_high_resolution_timers_and_exact_defined_settings() {
    let directory = tempfile::tempdir().unwrap();
    let actual = directory.path().join("actual.json");
    let required = json!({"CONFIG_HIGH_RES_TIMERS":"y", "CONFIG_TIMERFD":"y"});
    let check = |settings: &serde_json::Value| {
        fs::write(&actual, settings.to_string()).unwrap();
        result(&python_api(
            "kernel-arm64/check-config.py",
            "subject.fixture_errors(json.loads(Path(sys.argv[2]).read_text()))",
            &[&actual],
        ))
    };
    assert_eq!(check(&required), json!([]));
    for key in ["CONFIG_HIGH_RES_TIMERS", "CONFIG_TIMERFD"] {
        for value in [None, Some("n"), Some("m")] {
            let mut bad = required.clone();
            if let Some(value) = value {
                bad[key] = value.into();
            } else {
                bad.as_object_mut().unwrap().remove(key);
            }
            assert_eq!(
                check(&bad),
                json!([format!("{key}: timerfd spin fixture requires y")])
            );
        }
    }
    assert_eq!(check(&json!({})).as_array().unwrap().len(), 2);
    let errors = result(&python_api("kernel-arm64/check-config.py", "subject.errors(subject.settings('CONFIG_PRESENT=y\\nCONFIG_REMOVED=n\\nCONFIG_HZ=100\\n'), subject.settings('# CONFIG_PRESENT is not set\\nCONFIG_HZ=1000\\n'), {'CONFIG_PRESENT', 'CONFIG_HZ'})", &[]));
    assert_eq!(
        errors,
        json!([
            "CONFIG_PRESENT: requested y, resolved n",
            "CONFIG_REMOVED: not defined in arm64 Kconfig sources",
            "CONFIG_HZ: requested 100, resolved 1000"
        ])
    );
    assert_eq!(result(&python_api("kernel-arm64/check-config.py", "subject.errors(subject.settings('# CONFIG_PRESENT is not set\\n'), {}, {'CONFIG_PRESENT'})", &[])), json!([]));
    let duplicate = python_api(
        "kernel-arm64/check-config.py",
        "subject.settings('CONFIG_PRESENT=y\\nCONFIG_PRESENT=n\\n')",
        &[],
    );
    assert!(!duplicate.status.success());
    for (file, text) in [
        ("arch/Kconfig", "config GENERIC\n"),
        ("arch/arm64/Kconfig", "\tconfig ARM64_ONLY\n"),
        ("arch/x86/Kconfig", "config X86_ONLY\n"),
        ("drivers/Kconfig", "menuconfig DRIVER\n"),
    ] {
        let path = directory.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    assert_eq!(
        result(&python_api(
            "kernel-arm64/check-config.py",
            "sorted(subject.defined_symbols(Path(sys.argv[2])))",
            &[directory.path()]
        )),
        json!(["CONFIG_ARM64_ONLY", "CONFIG_DRIVER", "CONFIG_GENERIC"])
    );
}

#[test]
fn initramfs_has_deterministic_independently_parsed_newc_records() {
    let encode = || {
        hex::decode(
            result(&python_api(
                "kernel-arm64/build-initramfs.py",
                "subject.archive(b'ELF-test').hex()",
                &[],
            ))
            .as_str()
            .unwrap(),
        )
        .unwrap()
    };
    let bytes = encode();
    assert_eq!(bytes, encode());
    assert_eq!(bytes.len() % 512, 0);
    let mut offset = 0;
    let mut entries = std::collections::BTreeMap::new();
    loop {
        assert_eq!(&bytes[offset..offset + 6], b"070701");
        let fields: Vec<u32> = (0..13)
            .map(|i| {
                u32::from_str_radix(
                    std::str::from_utf8(&bytes[offset + 6 + i * 8..offset + 14 + i * 8]).unwrap(),
                    16,
                )
                .unwrap()
            })
            .collect();
        offset += 110;
        let name = std::str::from_utf8(&bytes[offset..offset + fields[11] as usize - 1])
            .unwrap()
            .to_owned();
        offset = (offset + fields[11] as usize + 3) & !3;
        let data = bytes[offset..offset + fields[6] as usize].to_vec();
        offset = (offset + fields[6] as usize + 3) & !3;
        entries.insert(name.clone(), (fields[1], data, [fields[9], fields[10]]));
        if name == "TRAILER!!!" {
            break;
        }
    }
    assert_eq!(
        entries["init"],
        (0o100000 | 0o755, b"ELF-test".to_vec(), [0, 0])
    );
    assert_eq!(entries["dev/console"], (0o020000 | 0o600, vec![], [5, 1]));
}

#[test]
fn patching_rejects_drift_and_cannot_reenable_hardware_counter_access() {
    assert_eq!(
        result(&python_api(
            "kernel-arm64/apply.py",
            "subject.body('int f(void) { if (a) { b(); } return 0; }', 'f', 'return 1;')",
            &[]
        )),
        "int f(void) {\nreturn 1;\n}"
    );
    for expression in [
        "subject.body('int f(void);', 'f', '')",
        "subject.replace('xx', 'x', 'y')",
    ] {
        assert!(!python_api("kernel-arm64/apply.py", expression, &[])
            .status
            .success());
    }
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("source.c");
    fs::write(&file, "static void update_cntkctl_el1(struct task_struct *next)\n{\n\tif (test_ti_thread_flag(task_thread_info(next), TIF_TSC_SIGSEGV))\n\t\tsysreg_clear_set(cntkctl_el1, ARCH_TIMER_USR_VCT_ACCESS_EN, 0);\n\telse\n\t\tsysreg_clear_set(cntkctl_el1, 0, ARCH_TIMER_USR_VCT_ACCESS_EN);\n}\n").unwrap();
    let patched = result(&python_api(
        "kernel-arm64/apply.py",
        "subject.counter_access(Path(sys.argv[2]).read_text())",
        &[&file],
    ));
    let patched = patched.as_str().unwrap();
    assert!(!patched.contains("cntkctl_el1, 0, ARCH_TIMER_USR_VCT_ACCESS_EN"));
    assert_eq!(
        patched
            .matches("cntkctl_el1, ARCH_TIMER_USR_VCT_ACCESS_EN, 0")
            .count(),
        2
    );
    assert!(patched.contains("TIF_TSC_SIGSEGV"));
    fs::write(&file, patched).unwrap();
    assert!(!python_api(
        "kernel-arm64/apply.py",
        "subject.counter_access(Path(sys.argv[2]).read_text())",
        &[&file]
    )
    .status
    .success());
}

#[test]
fn counter_audit_detects_named_and_encoded_sources_without_rejecting_pv_registers() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("instructions");
    for instruction in [
        "mrs x0, cntvct_el0",
        "mrs x4, cntpctss_el0",
        "mrs x0, s3_3_c14_c0_6",
        "mrs x0, RNDR",
        "wfit x3",
        "mrs x0, CNTFRQ_EL0",
    ] {
        fs::write(&file, instruction).unwrap();
        let violations = result(&python_api(
            "kernel-arm64/audit.py",
            "subject.violations(Path(sys.argv[2]).read_text())",
            &[&file],
        ));
        assert!(!violations.as_array().unwrap().is_empty(), "{instruction}");
    }
    fs::write(
        &file,
        "msr cntv_ctl_el0, xzr\nmrs x0, cntkctl_el1\n<arch_timer_read_cntvct_el0>",
    )
    .unwrap();
    assert_eq!(
        result(&python_api(
            "kernel-arm64/audit.py",
            "subject.violations(Path(sys.argv[2]).read_text())",
            &[&file]
        )),
        json!([])
    );
}

// Helpers for the native C guest source probes below. The C shims only model
// IRQ/MMIO and report observations; their assertions are all in Rust.
fn section(file: &str, start: &str, end: &str) -> String {
    let source = fs::read_to_string(support::root().join(file)).unwrap();
    source[source.find(start).unwrap()..source.find(end).unwrap()].to_owned()
}

fn compile(source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("probe.c");
    let binary = directory.path().join("probe");
    fs::write(&input, source).unwrap();
    support::success(
        std::process::Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-Wall", "-Wextra", "-Werror", "-O2"])
            .arg(input)
            .arg("-o")
            .arg(&binary),
    );
    (directory, binary)
}

#[test]
fn high_resolution_guest_prerequisite_rejects_boot_time_resolution() {
    let helper = section(
        "kernel-arm64/init.c",
        "static void require_high_resolution(void)",
        "static void busy_wait_check(void)",
    );
    let source = format!(
        r#"
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static long resolution_ns;
static int getres(clockid_t id, struct timespec *res) {{
    printf("clock=%d\n", id == CLOCK_MONOTONIC);
    if (resolution_ns < 0) {{ errno = EIO; return -1; }}
    res->tv_sec = 0; res->tv_nsec = resolution_ns; return 0;
}}
static void fail(const char *what) {{ fprintf(stderr, "%s: errno=%d\n", what, errno); exit(2); }}
#define clock_getres getres
{helper}
int main(int argc, char **argv) {{ if (argc != 2) return 3; resolution_ns = strtol(argv[1], NULL, 10); require_high_resolution(); return 0; }}
"#
    );
    let (_directory, binary) = compile(&source);
    for (resolution, status, diagnostic) in [
        ("1", 0, ""),
        ("10000000", 2, "PV high-resolution timer mode not active"),
        ("0", 2, "PV high-resolution timer mode not active"),
        ("-1", 2, "clock_getres"),
    ] {
        let output = std::process::Command::new(&binary)
            .arg(resolution)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(status));
        assert!(output.stdout.starts_with(b"clock=1\n"));
        if resolution == "1" {
            assert_eq!(output.stdout, b"clock=1\nclock resolution=0.000000001\n");
        }
        assert!(String::from_utf8_lossy(&output.stderr).contains(diagnostic));
    }
}

#[test]
fn idle_rendezvous_unmasks_then_restores_both_incoming_irq_states() {
    let idle = section(
        "kernel-arm64/seedbox.c",
        "void seedbox_idle(void)",
        "void __init seedbox_time_init(void)",
    );
    let source = format!(
        r#"
#include <stdio.h>
#include <stdlib.h>
static unsigned long irq_flags;
static int synchronized;
static char registers[64];
static void *event = registers;
#define unlikely(x) (x)
#define panic(...) abort()
#define local_irq_save(saved) do {{ (saved) = irq_flags; irq_flags |= 128; }} while (0)
#define local_irq_enable() do {{ irq_flags &= ~128UL; synchronized = 0; }} while (0)
#define local_irq_restore(saved) do {{ irq_flags = (saved); }} while (0)
#define dsb(scope) ((void)0)
#define isb() do {{ synchronized = 1; }} while (0)
static void writeq(unsigned long value, void *address) {{
    printf("WRITE %lu %ld %lu %d\n", value, (char *)address - registers, irq_flags, synchronized);
}}
{idle}
int main(int argc, char **argv) {{
    if (argc != 2) return 3;
    irq_flags = strtoul(argv[1], NULL, 10);
    seedbox_idle(); printf("RESTORED %lu\n", irq_flags);
}}
"#
    );
    let (_directory, binary) = compile(&source);
    for flags in [0x3c0u64, 0x340] {
        let output = support::success(std::process::Command::new(&binary).arg(flags.to_string()));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("WRITE 1 16 {} 1\nRESTORED {flags}\n", flags & !128)
        );
    }
}

#[test]
fn polling_preserves_the_callers_irq_mask_and_retries_after_irq_progress() {
    let read = section(
        "kernel-arm64/seedbox.c",
        "u64 notrace seedbox_read(void)",
        "/* Charge syscall work",
    );
    let source = format!(
        r#"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint64_t u64;
#define notrace
#define unlikely(x) (x)
#define panic(...) abort()
static unsigned long irq_flags, caller_flags;
static int synchronized, writes;
static char registers[64];
static void *event = registers;
static struct {{ u64 now, last, bound, abi, deferred_irq_pending; }} storage, *page = &storage;
static u64 early_last;
#define local_irq_save(saved) do {{ (saved) = irq_flags; irq_flags |= 128; }} while (0)
#define local_irq_enable() do {{ irq_flags &= ~128UL; synchronized = 0; }} while (0)
#define local_irq_disable() do {{ irq_flags |= 128; }} while (0)
#define local_irq_restore(saved) do {{ irq_flags = (saved); }} while (0)
#define irqs_disabled_flags(saved) ((saved) & 128)
#define isb() do {{ synchronized = 1; }} while (0)
#define READ_ONCE(x) (printf("ACCESS %lu\n", irq_flags), (x))
#define WRITE_ONCE(x, v) do {{ printf("ACCESS %lu\n", irq_flags); (x) = (v); }} while (0)
static void writeq(unsigned long value, void *address) {{
    printf("WRITE %lu %ld %lu %d\n", value, (char *)address - registers, irq_flags, synchronized);
    ++writes; page->now = 100; page->bound = UINT64_MAX;
    page->deferred_irq_pending = !!(caller_flags & 128);
    if (!(caller_flags & 128)) page->last = 105;
}}
{read}
static void observe(void) {{ u64 value = seedbox_read(); printf("RESULT %llu %lu %d %llu\n", (unsigned long long)value, irq_flags, writes, (unsigned long long)storage.deferred_irq_pending); }}
int main(int argc, char **argv) {{
    if (argc != 2) return 3;
    int masked = atoi(argv[1]); caller_flags = irq_flags = masked ? 0x3c0 : 0x340;
    storage.now = storage.last = storage.bound = 100;
    observe(); observe();
    if (masked) {{ caller_flags = irq_flags = 0x340; observe(); observe(); }}
}}
"#
    );
    let (_directory, binary) = compile(&source);
    for masked in [false, true] {
        let output = support::success(std::process::Command::new(&binary).arg(if masked {
            "1"
        } else {
            "0"
        }));
        let text = String::from_utf8(output.stdout).unwrap();
        for line in text.lines().filter_map(|l| l.strip_prefix("ACCESS ")) {
            assert_ne!(
                line.parse::<u64>().unwrap() & 128,
                0,
                "unmasked time-page access"
            );
        }
        let results: Vec<_> = text.lines().filter(|l| l.starts_with("RESULT ")).collect();
        let writes: Vec<_> = text.lines().filter(|l| l.starts_with("WRITE ")).collect();
        if masked {
            assert_eq!(
                results,
                [
                    "RESULT 101 960 1 1",
                    "RESULT 102 960 1 1",
                    "RESULT 106 832 2 0",
                    "RESULT 107 832 2 0"
                ]
            );
            assert_eq!(writes, ["WRITE 1 24 960 0", "WRITE 1 24 832 1"]);
        } else {
            assert_eq!(results, ["RESULT 106 832 1 0", "RESULT 107 832 1 0"]);
            assert_eq!(writes, ["WRITE 1 24 832 1"]);
        }
    }
}

#[test]
fn syscall_charge_preserves_owed_work_across_throttle_heal_and_multiple_bounds() {
    let page = section(
        "kernel-arm64/seedbox.c",
        "struct seedbox_page {",
        "static struct seedbox_page *page;",
    );
    let helpers = section(
        "kernel-arm64/seedbox.c",
        "u64 notrace seedbox_read(void)",
        "static u64 cs_read",
    );
    let source = format!(
        r#"
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
typedef uint64_t u64;
#define notrace
#define unlikely(x) (x)
#define panic(...) abort()
{page}
static struct seedbox_page storage, *page = &storage;
static u64 early_last, next_cost, window;
static unsigned long irq_flags;
static unsigned int writes;
static char registers[64];
static void *event = registers;
#define local_irq_save(saved) do {{ (saved) = irq_flags; irq_flags |= 128; }} while (0)
#define local_irq_enable() do {{ irq_flags &= ~128UL; }} while (0)
#define local_irq_disable() do {{ irq_flags |= 128; }} while (0)
#define local_irq_restore(saved) do {{ irq_flags = (saved); }} while (0)
#define irqs_disabled_flags(saved) ((saved) & 128)
#define isb() ((void)0)
#define READ_ONCE(x) (printf("ACCESS %d %lu\n", &(x) == &page->syscall_ns, irq_flags), (x))
#define WRITE_ONCE(x, v) do {{ printf("ACCESS 0 %lu\n", irq_flags); (x) = (v); }} while (0)
static void writeq(unsigned long value, void *address) {{
    printf("WRITE %lu %ld %lu %d\n", value, (char *)address - registers, irq_flags, page->last == page->bound);
    ++writes; page->now = page->last; page->bound += window;
    if (next_cost) page->syscall_ns = next_cost;
}}
{helpers}
int main(int argc, char **argv) {{
    if (argc != 8) return 3;
    storage.syscall_ns = strtoull(argv[1], NULL, 10); storage.bound = strtoull(argv[2], NULL, 10);
    window = strtoull(argv[3], NULL, 10); next_cost = strtoull(argv[4], NULL, 10);
    printf("OFFSET %zu\n", offsetof(struct seedbox_page, syscall_ns));
    if (atoi(argv[6])) {{ u64 value = seedbox_read(); printf("READ %llu\n", (unsigned long long)value); }}
    if (atoi(argv[7])) page = NULL;
    for (int i = 0; i < atoi(argv[5]); ++i) {{
        seedbox_syscall_progress();
        if (page) printf("RESULT %llu %u %llu %lu\n", (unsigned long long)page->last, writes, (unsigned long long)page->syscall_ns, irq_flags);
        else printf("EARLY %llu %lu\n", (unsigned long long)early_last, irq_flags);
    }}
}}
"#
    );
    let (_directory, binary) = compile(&source);
    for (args, expected) in [
        (
            ["1000", "18446744073709551615", "0", "0", "1", "1", "0"],
            vec!["RESULT 1001 0 1000 0"],
        ),
        (
            ["4000", "18446744073709551615", "0", "0", "1", "1", "0"],
            vec!["RESULT 4001 0 4000 0"],
        ),
        (
            ["1000000", "18446744073709551615", "0", "0", "1", "0", "0"],
            vec!["RESULT 1000000 0 1000000 0"],
        ),
        (
            ["1000", "500", "500", "4000", "2", "0", "0"],
            vec!["RESULT 1000 1 4000 0", "RESULT 5000 9 4000 0"],
        ),
        (
            ["4000", "2000", "2000", "1000", "2", "0", "0"],
            vec!["RESULT 4000 1 1000 0", "RESULT 5000 2 1000 0"],
        ),
        (
            ["1000", "250", "250", "0", "1", "0", "0"],
            vec!["RESULT 1000 3 1000 0"],
        ),
        (["1000", "0", "0", "0", "1", "0", "1"], vec!["EARLY 1000 0"]),
    ] {
        let output = support::success(std::process::Command::new(&binary).args(args));
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("OFFSET 48\n"));
        for line in text.lines().filter_map(|l| l.strip_prefix("ACCESS 0 ")) {
            assert_ne!(line.parse::<u64>().unwrap() & 128, 0);
        }
        for line in text.lines().filter(|l| l.starts_with("WRITE ")) {
            assert_eq!(line, "WRITE 1 24 0 1");
        }
        let results: Vec<_> = text
            .lines()
            .filter(|l| l.starts_with("RESULT ") || l.starts_with("EARLY "))
            .collect();
        assert_eq!(results, expected);
        if args[5] == "1" {
            assert!(text.contains("READ 1\n"));
        }
    }
}
