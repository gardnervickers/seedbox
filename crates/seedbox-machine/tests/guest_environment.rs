//! Configured boot archive and production C-header probes. No guest, mount,
//! chroot, sysctl, formatter or network setup is executed on the host.
use seedbox_machine::{
    application::{
        self, DerivedApplication, GuestBootstrap, GuestEnvironment, GuestLimits, Kernel,
        NamedVolume,
    },
    definition::{FilesystemArchive, Launch, PreparedApplication},
};
use seedbox_test::process::Guarded;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};
#[derive(Debug)]
struct Entry {
    mode: u32,
    uid: u32,
    gid: u32,
    bytes: Vec<u8>,
}
fn entries(bytes: &[u8]) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut at = 0;
    while at < bytes.len() {
        assert_eq!(&bytes[at..at + 6], b"070701");
        let number = |field: usize| {
            u32::from_str_radix(
                std::str::from_utf8(&bytes[at + 6 + field * 8..at + 14 + field * 8]).unwrap(),
                16,
            )
            .unwrap()
        };
        let name_end = at + 110 + number(11) as usize;
        assert_eq!(bytes[name_end - 1], 0);
        let name = std::str::from_utf8(&bytes[at + 110..name_end - 1])
            .unwrap()
            .to_owned();
        let data = (name_end + 3) & !3;
        let size = number(6) as usize;
        let end = data.checked_add(size).unwrap();
        assert!(end <= bytes.len());
        assert!(out
            .insert(
                name.clone(),
                Entry {
                    mode: number(1),
                    uid: number(2),
                    gid: number(3),
                    bytes: bytes[data..end].to_vec()
                }
            )
            .is_none());
        at = (end + 3) & !3;
        if name == "TRAILER!!!" {
            assert_eq!(at, bytes.len());
            break;
        }
    }
    out
}
fn elf(machine: u16) -> Vec<u8> {
    // Structural ELF metadata only, never executed or translated.
    let mut b = vec![0; 128];
    b[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    b[16..18].copy_from_slice(&2u16.to_le_bytes());
    b[18..20].copy_from_slice(&machine.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[52..54].copy_from_slice(&64u16.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&1u16.to_le_bytes());
    b[64..68].copy_from_slice(&1u32.to_le_bytes());
    b[104..112].copy_from_slice(&1u64.to_le_bytes());
    b
}
fn application() -> PreparedApplication {
    let mut tar = tar::Builder::new(vec![]);
    for (path, data, mode, uid, gid) in [
        ("application", &b"not executed"[..], 0o755, 0, 0),
        (
            "data/image-content",
            &b"must not copy up"[..],
            0o640,
            123,
            456,
        ),
        (
            "seedbox/progress/runtime",
            &b"private-runtime-data"[..],
            0o755,
            0,
            0,
        ),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_mode(mode);
        h.set_uid(uid);
        h.set_gid(gid);
        h.set_size(data.len() as u64);
        tar.append_data(&mut h, path, data).unwrap();
    }
    PreparedApplication {
        filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
        launch: Launch {
            argv: vec!["/application".into()],
            environment: BTreeMap::new(),
            working_directory: "/".into(),
            user: "123:456".into(),
        },
    }
}
fn bootstrap() -> GuestBootstrap {
    GuestBootstrap {
        kernel: Kernel {
            image: vec![1],
            command_line: "explicit".into(),
        },
        init: elf(62),
        base_initramfs: vec![],
        epoch_seconds: 123456,
    }
}
fn environment() -> GuestEnvironment {
    let mut e = GuestEnvironment::new("db-α", "127.0.0.1 localhost\n10.0.0.1 db-α\n", false);
    e.anonymous_volumes = vec!["/cache".into(), "/data".into()];
    e.named_volumes = vec![NamedVolume {
        name: "data".into(),
        target: "/data".into(),
    }];
    e.configs
        .insert("/etc/demo".into(), vec![0, 255, 10, 0, 128]);
    e
}
fn archive(e: &GuestEnvironment, runtime: bool) -> Vec<u8> {
    let a = application();
    let b = bootstrap();
    let formatter = elf(62);
    if runtime {
        application::boot_derived_configured(
            &DerivedApplication {
                application: a,
                execution_prefix: vec!["/seedbox/progress/bin64/drrun".into()],
            },
            &b,
            e,
            Some(&formatter),
        )
        .unwrap()
    } else {
        application::boot_archive_configured(&a, &b, e, Some(&formatter)).unwrap()
    }
}
fn record(e: &GuestEnvironment, runtime: bool) -> Vec<u8> {
    entries(&archive(e, runtime))
        .remove("seedbox/environment.bin")
        .unwrap()
        .bytes
}
fn u32_field(bytes: &[u8], at: &mut usize) -> u32 {
    let n = u32::from_le_bytes(bytes[*at..*at + 4].try_into().unwrap());
    *at += 4;
    n
}
fn field(bytes: &[u8], at: &mut usize) -> Vec<u8> {
    let n = u32_field(bytes, at) as usize;
    let b = bytes[*at..*at + n].to_vec();
    *at += n;
    b
}
#[test]
fn configured_archive_exact_ownership_binary_closure_and_unchanged_default() {
    let app = application();
    let boot = bootstrap();
    let env = environment();
    let fmt = elf(62);
    let before = (app.clone(), boot.clone(), env.clone(), fmt.clone());
    let plain = application::boot_archive(&app, &boot).unwrap();
    let derived = DerivedApplication {
        application: app.clone(),
        execution_prefix: vec![],
    };
    assert_eq!(plain, application::boot_derived(&derived, &boot).unwrap());
    let p = entries(&plain);
    assert!(!p.contains_key("seedbox/environment.bin"));
    assert!(!p.contains_key("seedbox/mke2fs"));
    let packed = application::boot_archive_configured(&app, &boot, &env, Some(&fmt)).unwrap();
    assert_eq!((app, boot, env, fmt), before);
    drop(before);
    drop(derived);
    drop(plain);
    drop(p);
    let e = entries(&packed);
    for path in [
        "seedbox/environment.bin",
        "seedbox/mke2fs",
        "seedbox/application.bin",
    ] {
        assert_eq!((e[path].uid, e[path].gid), (0, 0));
    }
    assert_eq!(e["seedbox/environment.bin"].mode, 0o100400);
    assert_eq!(e["seedbox/mke2fs"].mode, 0o100755);
    assert_eq!(e["rootfs/data/image-content"].bytes, b"must not copy up");
    assert!(!e.contains_key("rootfs/etc/demo"));
    let bytes = &e["seedbox/environment.bin"].bytes;
    assert_eq!(&bytes[..8], b"SBXENV1\0");
    assert_eq!(
        u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize,
        bytes.len() - 16
    );
    let mut at = 16;
    assert_eq!(field(bytes, &mut at), "db-α".as_bytes());
    assert_eq!(
        field(bytes, &mut at),
        b"127.0.0.1 localhost\n10.0.0.1 db-\xce\xb1\n"
    );
    assert_eq!(u32_field(bytes, &mut at), 262144);
    assert_eq!(u32_field(bytes, &mut at), 1048576);
    assert_eq!(&bytes[at..at + 2], &[0, 0]);
    at += 2;
    assert_eq!(u32_field(bytes, &mut at), 2);
    assert_eq!(field(bytes, &mut at), b"/cache");
    assert_eq!(field(bytes, &mut at), b"/data");
    assert_eq!(u32_field(bytes, &mut at), 1);
    assert_eq!(field(bytes, &mut at), b"data");
    assert_eq!(field(bytes, &mut at), b"/data");
    assert_eq!(u32_field(bytes, &mut at), 1);
    assert_eq!(field(bytes, &mut at), b"/etc/demo");
    assert_eq!(field(bytes, &mut at), [0, 255, 10, 0, 128]);
    assert_eq!(at, bytes.len());
}
#[test]
fn configured_runtime_is_bootstrap_owned_not_duplicated_and_fd_reserve_is_backend_specific() {
    let env = environment();
    let packed = archive(&env, true);
    let e = entries(&packed);
    assert_eq!(e["seedbox/progress/runtime"].bytes, b"private-runtime-data");
    assert!(!e.contains_key("rootfs/seedbox/progress/runtime"));
    let bytes = &e["seedbox/environment.bin"].bytes;
    let mut at = 16;
    field(bytes, &mut at);
    field(bytes, &mut at);
    assert_eq!(u32_field(bytes, &mut at), 262144);
    assert_eq!(u32_field(bytes, &mut at), 1048576 + 96);
    assert_eq!(&bytes[at..at + 2], &[0, 1]);
    let mut invalid = env.clone();
    invalid.limits.nofile = NonZeroU32::new(u32::MAX).unwrap();
    let a = DerivedApplication {
        application: application(),
        execution_prefix: vec!["/seedbox/progress/bin64/drrun".into()],
    };
    assert!(
        application::boot_derived_configured(&a, &bootstrap(), &invalid, Some(&elf(62))).is_err()
    );
    let mut no_volume = GuestEnvironment::new("app", "", true);
    no_volume.limits = GuestLimits::default();
    let p = entries(
        &application::boot_archive_configured(&application(), &bootstrap(), &no_volume, None)
            .unwrap(),
    );
    assert!(!p.contains_key("seedbox/mke2fs"));
}
#[test]
fn invalid_paths_collisions_nul_limits_and_required_formatter_are_rejected_without_source_mutation()
{
    let app = application();
    let boot = bootstrap();
    let good = environment();
    for target in [
        "relative",
        "//d",
        "/d/",
        "/d//x",
        "/d/../x",
        "/d/./x",
        "/",
        "/dev/null",
        "/proc/x",
        "/sys/x",
        "/etc/hosts",
        "/nul\0x",
    ] {
        let mut e = good.clone();
        e.configs.clear();
        e.configs.insert(target.into(), vec![]);
        assert!(
            application::boot_archive_configured(&app, &boot, &e, Some(&elf(62))).is_err(),
            "{target:?}"
        );
    }
    for kind in 0..8 {
        let mut e = good.clone();
        match kind {
            0 => e.hostname = "x\n".into(),
            1 => e.hosts.push('\0'),
            2 => e.anonymous_volumes.push("/cache".into()),
            3 => e.named_volumes.push(e.named_volumes[0].clone()),
            4 => {
                e.configs.insert("/data/file".into(), vec![]);
            }
            5 => {
                e.configs.insert("/cache".into(), vec![]);
            }
            6 => {
                e.configs.insert("/etc".into(), vec![]);
            }
            7 => e.named_volumes[0].name = "..".into(),
            _ => unreachable!(),
        };
        let before = e.clone();
        assert!(
            application::boot_archive_configured(&app, &boot, &e, Some(&elf(62))).is_err(),
            "kind {kind}"
        );
        assert_eq!(e, before);
    }
    assert!(application::boot_archive_configured(&app, &boot, &good, None).is_err());
    assert!(application::boot_archive_configured(&app, &boot, &good, Some(&elf(183))).is_err());
    let mut dynamic = elf(62);
    dynamic.resize(176, 0);
    dynamic[56..58].copy_from_slice(&2u16.to_le_bytes());
    dynamic[120..124].copy_from_slice(&3u32.to_le_bytes());
    seedbox_machine::machine::assets::elf(&dynamic, 62, false).unwrap();
    let error = application::boot_archive_configured(&app, &boot, &good, Some(&dynamic))
        .err()
        .unwrap();
    assert!(error.to_string().contains("static ELF"));
    let mut huge = GuestEnvironment::new("app", "", false);
    huge.anonymous_volumes = vec!["/a".into(); 65537];
    assert!(application::boot_archive_configured(&app, &boot, &huge, None).is_err());
    huge.anonymous_volumes.clear();
    huge.configs
        .insert("/x".into(), vec![0; 16 * 1024 * 1024 + 1]);
    assert!(application::boot_archive_configured(&app, &boot, &huge, None).is_err());
    assert_eq!(good, environment());
}
struct Probe {
    _dir: tempfile::TempDir,
    path: PathBuf,
}
fn probe() -> &'static Probe {
    static PROBE: OnceLock<Result<Probe, String>> = OnceLock::new();
    match PROBE.get_or_init(|| {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = dir.path().join("environment-probe");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
        let build = Command::new(compiler)
            .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
            .arg("-I")
            .arg(root.join("src/pv"))
            .arg(root.join("tests/support/guest_environment_probe.c"))
            .arg("-o")
            .arg(&path)
            .guarded_output()
            .map_err(|e| e.to_string())?;
        if !build.status.success() {
            return Err(String::from_utf8_lossy(&build.stderr).into_owned());
        }
        Ok(Probe { _dir: dir, path })
    }) {
        Ok(probe) => probe,
        Err(error) => panic!("C probe compilation failed (cached, no fallback/retry): {error}"),
    }
}

fn run(bytes: &[u8], mode: &str, flags: &str) -> (bool, String) {
    let mut c = Command::new(&probe().path)
        .args([mode, flags])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .guarded_spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(bytes).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let storage = text
        .lines()
        .find(|line| line.starts_with("record-storage|"))
        .unwrap();
    assert!(
        storage == "record-storage|0|0" || storage == "record-storage|1|1",
        "owned record must release exactly once; borrowed input never freed: {text}"
    );
    let resources = text.lines().find(|s| s.starts_with("resources|")).unwrap();
    assert!(
        resources.starts_with("resources|0|0|"),
        "leaked parser allocation/fd: {text}"
    );
    (out.status.success(), text)
}
fn position(text: &str, line: &str) -> usize {
    text.find(line)
        .unwrap_or_else(|| panic!("missing {line:?}: {text}"))
}
#[test]
fn pure_c_setup_preserves_volume_metadata_order_binary_configs_devices_limits_and_process_restarts()
{
    let wire = record(&environment(), true);
    let (ok, text) = run(&wire, "setup", "");
    assert!(ok, "{text}");
    for expected in [
        "formatter|mke2fs|-q|-F|-t|ext4|-U|5eedb0c5-0000-4000-8000-000000000001|-E|hash_seed=5eedb0c5-0000-4000-8000-000000000002,lazy_itable_init=1,lazy_journal_init=1,nodiscard|/dev/vda",
        "pread|/dev/vda|2|1080",
        "mount|/dev/vda|/seedbox/disk|ext4|0|noinit_itable",
        "mkdir|/seedbox/disk/data|2770",
        "chown|/seedbox/disk/data|123|456",
        "chmod|/seedbox/disk/data|2770",
        "fsync|/seedbox/disk",
        "mount|/seedbox/disk/data|/rootfs/data|-|4096|-",
        "mount|/seedbox/progress|/rootfs/seedbox/progress|-|4096|-",
        "mount|-|/rootfs/seedbox/progress|-|4135|-",
        "mount|tmpfs|/dev|tmpfs|10|mode=0755,size=65536",
        "mount|tmpfs|/dev/shm|tmpfs|14|mode=1777,size=67108864",
        "mount|devpts|/dev/pts|devpts|10|newinstance,ptmxmode=0666,mode=0620,gid=5",
        "mknod|/dev/tty|020666|5|0",
        "symlink|/proc/self/fd/1|/dev/stdout",
        "fchmod|console|0666",
        "hostname|db-α",
        "write|/etc/demo|00ff0a0080",
        "fchmod|/etc/demo|0444",
        "fchown|/etc/demo|0|0",
        "mount|-|/etc/demo|-|4129|-",
        "write|/proc/sys/vm/max_map_count|3236323134340a",
        "write|/proc/sys/fs/nr_open|313034383637320a",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(position(&text, "fchmod|console|0666") < position(&text, "formatter|"));
    assert!(
        position(&text, "mount|/seedbox/disk/data") < position(&text, "mount|/seedbox/progress")
    );
    assert!(position(&text, "fsync|/seedbox/disk") < position(&text, "root-switch-stub"));
    assert!(position(&text, "root-switch-stub") < position(&text, "mount|tmpfs|/dev|"));
    assert!(position(&text, "mount|-|/etc/demo") < position(&text, "start-one"));
    assert_eq!(text.matches("formatter|").count(), 1);
    assert_eq!(text.matches("rlimit|1048672|1048672").count(), 2);
    assert!(
        !text.contains("open|/rootfs/data"),
        "image target contents must not be copied"
    );
    assert!(!text.contains("mount|tmpfs|/cache"));
}
#[test]
fn pure_c_existing_ext4_and_volume_directory_are_not_reformatted_or_reowned() {
    let (ok, text) = run(&record(&environment(), false), "setup", "existing");
    assert!(ok, "{text}");
    assert!(!text.contains("formatter|"));
    assert!(!text.contains("chown|/seedbox/disk/data"));
    assert!(!text.contains("chmod|/seedbox/disk/data"));
    assert!(!text.contains("fsync|"));
    assert_eq!(text.matches("rlimit|1048576|1048576").count(), 2);
}
#[test]
fn pure_c_setup_failures_stop_in_order_close_fds_and_do_not_retry_format_or_publish_start() {
    let wire = record(&environment(), true);
    for (flag, phase) in [
        ("preadfail", "format retained disk"),
        ("formatfail", "format retained disk"),
        ("mountfail", "mount retained disk"),
        ("mkdirfail", "create volume directory"),
        ("symlink", "plain volume target"),
        ("fsyncfail", "sync volume directories"),
        ("sysctlfail", "application sysctl"),
        ("configfail", "read-only config"),
        ("rofail", "read-only config"),
        ("nomanagement", "retain management"),
    ] {
        let (ok, text) = run(&wire, "setup", flag);
        assert!(!ok, "{flag}: {text}");
        assert!(text.contains(phase), "{flag}: {text}");
        assert!(!text.contains("start-one"), "{flag}: {text}");
        assert!(
            text.matches("formatter|").count() <= 1,
            "no formatter retry: {text}"
        );
        if flag == "symlink" {
            assert!(!text.contains("mount|/seedbox/disk/data"));
        }
    }
}
#[test]
fn c_closed_parser_rejects_all_small_prefixes_trailing_utf8_nul_tags_and_oversized_counts() {
    let wire = record(&environment(), false);
    assert!(run(&wire, "parse", "").0);
    for end in 0..wire.len() {
        assert!(!run(&wire[..end], "parse", "").0, "prefix {end}");
    }
    let mut trailing = wire.clone();
    trailing.push(0);
    assert!(!run(&trailing, "parse", "").0);
    for byte in [0, 0xff] {
        let mut bad = wire.clone();
        bad[20] = byte;
        assert!(!run(&bad, "parse", "").0);
    }
    let mut at = 16;
    field(&wire, &mut at);
    field(&wire, &mut at);
    at += 8;
    for which in [at, at + 1] {
        let mut bad = wire.clone();
        bad[which] = 2;
        assert!(!run(&bad, "parse", "").0);
    }
    at += 2;
    let mut bad = wire.clone();
    bad[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let (ok, text) = run(&bad, "parse", "");
    assert!(!ok);
    let max: usize = text
        .lines()
        .find(|s| s.starts_with("resources|"))
        .unwrap()
        .split('|')
        .next_back()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        max < 1000,
        "count must be rejected before array allocation: {text}"
    );
    let mut oversized = b"SBXENV1\0".to_vec();
    oversized.extend((u64::from(u32::MAX) - 16 + 1).to_le_bytes());
    let (ok, text) = run(&oversized, "load", "");
    assert!(!ok);
    assert!(text.contains("read|/seedbox/environment.bin|16"));
    assert!(
        text.contains("resources|0|0|0|0"),
        "oversized body must not be allocated/read: {text}"
    );
}
#[test]
fn c_parser_allocation_failures_cleanup_and_absent_recipe_keep_generic_setup_unconfigured() {
    let wire = record(&environment(), false);
    let (_, text) = run(&wire, "parse", "");
    let n: usize = text
        .lines()
        .find(|s| s.starts_with("resources|"))
        .unwrap()
        .split('|')
        .nth(3)
        .unwrap()
        .parse()
        .unwrap();
    for at in 1..=n {
        let (ok, text) = run(&wire, "parse", &format!("allocfail={at}"));
        assert!(!ok, "allocation {at}: {text}");
        assert!(
            text.contains(&format!("error|{}|", libc::ENOMEM)),
            "allocation errno must survive cleanup: {text}"
        );
    }
    let (ok, text) = run(&[], "load", "absent");
    assert!(ok, "{text}");
    assert!(text.contains("parsed|0|0|0|0|0|0|0"));
    assert!(!text.contains("mount|"));
    assert!(!text.contains("formatter|"));
    assert!(!text.contains("rlimit|"));
}
#[test]
fn c_init_integration_retains_management_before_setup_and_applies_setup_before_start() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv/guest_service_init.c"),
    )
    .unwrap();
    assert!(
        position(&source, "while ((management = port())")
            < position(&source, "sbx_env_before_root(&environment,management)")
    );
    assert!(
        position(&source, "sbx_env_after_root(&environment)")
            < position(&source, "sbx_service_start(&service)")
    );
    assert!(source.contains("int control_enabled = sbx_env_control_enabled(&environment);"));
    assert!(source.contains("if (control_enabled)"));
    assert!(
        position(&source, "sbx_env_load(&environment,failure_path)")
            < position(
                &source,
                "sbx_control_mode(command, control_enabled, &control_enabled)"
            )
    );
    assert!(
        position(
            &source,
            "sbx_control_mode(command, control_enabled, &control_enabled)"
        ) < position(&source, "sbx_env_before_root(&environment,management)")
    );
    assert!(
        position(&source, "sbx_oracle_init(&oracle, write_all, &management)")
            < position(&source, "if (control_enabled)")
    );
    let launch = &source[position(&source, "static int launch(")
        ..position(&source, "static void configure_network(")];
    assert!(
        position(launch, "sbx_env_application_limits(&environment)")
            < position(launch, "sbx_application_exec(application)")
    );
}

fn replace_field(wire: &[u8], offset: usize, value: &[u8]) -> Vec<u8> {
    let old = u32::from_le_bytes(wire[offset..offset + 4].try_into().unwrap()) as usize;
    let mut bytes = wire[..offset].to_vec();
    bytes.extend((value.len() as u32).to_le_bytes());
    bytes.extend(value);
    bytes.extend_from_slice(&wire[offset + 4 + old..]);
    let size = bytes.len() - 16;
    bytes[8..16].copy_from_slice(&(size as u64).to_le_bytes());
    bytes
}
#[test]
fn c_parser_checks_normalized_paths_duplicate_targets_and_config_volume_ancestry_independently() {
    let wire = record(&environment(), false);
    let mut at = 16;
    field(&wire, &mut at);
    field(&wire, &mut at);
    let limits_at = at;
    at += 10;
    assert_eq!(u32_field(&wire, &mut at), 2);
    field(&wire, &mut at);
    let anon_second = at;
    field(&wire, &mut at);
    assert_eq!(u32_field(&wire, &mut at), 1);
    let name_at = at;
    field(&wire, &mut at);
    field(&wire, &mut at);
    assert_eq!(u32_field(&wire, &mut at), 1);
    let config_at = at;
    for bytes in [
        replace_field(&wire, anon_second, b"/cache"),
        replace_field(&wire, name_at, b".."),
        replace_field(&wire, config_at, b"/data/file"),
        replace_field(&wire, config_at, b"/dev/config"),
        replace_field(&wire, config_at, b"/relative/../x"),
    ] {
        assert!(!run(&bytes, "parse", "").0);
    }
    for offset in [limits_at, limits_at + 4] {
        let mut bytes = wire.clone();
        bytes[offset..offset + 4].fill(0);
        assert!(!run(&bytes, "parse", "").0);
    }
    let mut inner_trailing = wire.clone();
    inner_trailing.push(0);
    let size = inner_trailing.len() - 16;
    inner_trailing[8..16].copy_from_slice(&(size as u64).to_le_bytes());
    assert!(!run(&inner_trailing, "parse", "").0);
    let mut surrogate = wire;
    surrogate[20..23].copy_from_slice(&[0xed, 0xa0, 0x80]);
    assert!(!run(&surrogate, "parse", "").0);
}
#[test]
fn bounded_large_setup_lists_and_two_maximum_config_fields_remain_functional() {
    let mut env = GuestEnvironment::new("app", "", true);
    env.anonymous_volumes = (0..5000).rev().map(|i| format!("/a/entry-{i}")).collect();
    let wire = record(&env, false);
    let (ok, text) = run(&wire, "parse", "");
    assert!(ok, "{text}");
    let app = application();
    let boot = bootstrap();
    env.anonymous_volumes.clear();
    env.configs.insert(
        "/x".into(),
        (0..16 * 1024 * 1024).map(|i| (i % 256) as u8).collect(),
    );
    env.configs.insert(
        "/y".into(),
        (0..16 * 1024 * 1024)
            .map(|i| 255 - (i % 256) as u8)
            .collect(),
    );
    let expected: Vec<_> = env
        .configs
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.len(), digest(bytes)))
        .collect();
    let packed = application::boot_archive_configured(&app, &boot, &env, None).unwrap();
    let mut parsed = entries(&packed);
    let record = parsed.remove("seedbox/environment.bin").unwrap();
    assert_eq!((record.mode, record.uid, record.gid), (0o100400, 0, 0));
    assert!(record.bytes.len() > 32 * 1024 * 1024);
    drop(env);
    drop(app);
    drop(boot);
    drop(packed);
    drop(parsed);
    let (ok, text) = run(&record.bytes, "load-setup", "compact,drop-source");
    assert!(ok, "{text}");
    assert!(text.len() < 16384, "large observation must remain bounded");
    println!("large_config_observation:\n{text}");
    assert!(text.contains("slices|1|owned|1"));
    assert!(text.contains("source-dropped"));
    assert!(text.contains("released|0|0"));
    for (path, size, hash) in expected {
        assert!(
            text.contains(&format!("content|{path}|{size}|{hash}")),
            "{text}"
        );
        assert!(text.contains(&format!("fchmod|{path}|0444")));
        assert!(text.contains(&format!("fchown|{path}|0|0")));
        assert!(text.contains(&format!("mount|-|{path}|-|4129|-")));
    }
    assert_eq!(
        resource_value(&text, 4),
        record.bytes.len(),
        "one owned record, no payload copies"
    );
    let (ok, borrowed) = run(&record.bytes, "parse", "compact");
    assert!(ok, "{borrowed}");
    assert!(borrowed.contains("slices|1|owned|0"));
    assert!(
        resource_value(&borrowed, 4) < 1024,
        "borrowed data must not allocate payload buffers: {borrowed}"
    );
    let (ok, failure) = run(
        &record.bytes,
        "load-setup",
        "compact,drop-source,fail-large",
    );
    assert!(!ok);
    assert!(failure.contains(&format!("error|{}|read-only config|/x", libc::ENOSPC)));
    assert!(failure.len() < 16384);
}

#[test]
fn c_setup_missing_image_target_uses_root_defaults_and_all_ordinary_devices_are_created() {
    let (ok, text) = run(&record(&environment(), false), "setup", "missingtarget");
    assert!(ok, "{text}");
    assert!(text.contains("mkdir|/seedbox/disk/data|0755"));
    assert!(text.contains("chown|/seedbox/disk/data|0|0"));
    assert!(text.contains("chmod|/seedbox/disk/data|0755"));
    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        assert!(text.contains(&format!("mknod|/dev/{name}|020666|{major}|{minor}")));
        assert!(text.contains(&format!("chmod|/dev/{name}|0666")));
    }
    for (target, path) in [
        ("pts/ptmx", "ptmx"),
        ("/proc/self/fd", "fd"),
        ("/proc/self/fd/0", "stdin"),
        ("/proc/self/fd/1", "stdout"),
        ("/proc/self/fd/2", "stderr"),
    ] {
        assert!(text.contains(&format!("symlink|{target}|/dev/{path}")));
    }
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("hostname|"))
            .count(),
        1
    );
    assert_eq!(text.matches("mount|tmpfs|/dev|").count(), 1);
}
#[test]
fn c_close_owner_device_hostname_and_child_limit_failures_retain_primary_errno() {
    let wire = record(&environment(), false);
    for (flag, error, phase) in [
        ("configfail,closeconfig", libc::ENOSPC, "read-only config"),
        ("closeconfig", libc::EIO, "read-only config"),
        ("configownerfail", libc::EPERM, "read-only config"),
        ("closedev", libc::EIO, "format retained disk"),
        ("devicefail", libc::EPERM, "application device"),
        ("hostnamefail", libc::EPERM, "hostname"),
        (
            "existing,badvolume",
            libc::ENOTDIR,
            "existing volume directory",
        ),
        ("shortwrite", libc::EIO, "application sysctl"),
    ] {
        let (ok, text) = run(&wire, "setup", flag);
        assert!(!ok, "{flag}: {text}");
        assert!(
            text.contains(&format!("error|{error}|{phase}|")),
            "{flag}: {text}"
        );
        assert!(!text.contains("start-one"));
    }
    let (ok, text) = run(&wire, "setup", "rlimitfail");
    assert!(!ok);
    assert!(text.contains("start-one"));
    assert!(!text.contains("start-two"));
    assert!(text.contains(&format!(
        "error|{}|application nofile limit|RLIMIT_NOFILE",
        libc::EPERM
    )));
}
#[test]
fn c_loader_failures_cleanup_after_successful_parse_and_absence_does_not_apply_setup() {
    let wire = record(&environment(), false);
    let (ok, text) = run(&wire, "load", "");
    assert!(ok, "{text}");
    let n: usize = text
        .lines()
        .find(|s| s.starts_with("resources|"))
        .unwrap()
        .split('|')
        .nth(3)
        .unwrap()
        .parse()
        .unwrap();
    for at in 1..=n {
        let (ok, text) = run(&wire, "load", &format!("allocfail={at}"));
        assert!(!ok, "allocation {at}: {text}");
        assert!(text.contains(&format!("error|{}|", libc::ENOMEM)), "{text}");
    }
    for flag in ["closeload", "readerror"] {
        let (ok, text) = run(&wire, "load", flag);
        assert!(!ok);
        assert!(text.contains(&format!("error|{}|", libc::EIO)));
    }
    for end in [0, 7, 15, 16, wire.len() - 1] {
        assert!(!run(&wire[..end], "load", "").0);
    }
    let (ok, text) = run(&[], "load-setup", "absent");
    assert!(ok, "{text}");
    assert!(text.contains("parsed|0|0|0|0|0|0|0"));
    for op in [
        "mount|",
        "formatter|",
        "rlimit|",
        "fchmod|",
        "hostname|",
        "mknod|",
        "write|",
    ] {
        assert!(
            !text.contains(op),
            "unconfigured helper must be inert: {text}"
        );
    }
}

#[test]
fn c_guest_listener_flag_is_explicit_while_generic_absence_remains_enabled() {
    for enabled in [false, true] {
        let mut environment = environment();
        environment.control_socket = enabled;
        let (ok, text) = run(&record(&environment, false), "parse", "");
        assert!(ok);
        assert!(text.contains(&format!("control-enabled|{}", u8::from(enabled))));
    }
    let (ok, text) = run(&[], "load", "absent");
    assert!(ok);
    assert!(text.contains("control-enabled|1"));
}

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14695981039346656037u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(1099511628211)
    })
}
fn resource_value(text: &str, column: usize) -> usize {
    text.lines()
        .find(|line| line.starts_with("resources|"))
        .unwrap()
        .split('|')
        .nth(column)
        .unwrap()
        .parse()
        .unwrap()
}
#[test]
fn c_loader_checks_actual_regular_file_before_reserve_and_detects_changed_input() {
    let wire = record(&environment(), false);
    for (flags, error) in [
        ("fstatfail", libc::EACCES),
        ("nonregular", libc::EINVAL),
        ("negativesize", libc::EINVAL),
        ("oversizedstat", libc::E2BIG),
        ("statsmall", libc::EINVAL),
        ("statlarge", libc::EINVAL),
    ] {
        let (ok, text) = run(&wire, "load", flags);
        assert!(!ok, "{text}");
        assert!(text.contains(&format!("error|{error}|")), "{text}");
        assert_eq!(
            resource_value(&text, 3),
            0,
            "no allocation before actual size/header validation: {text}"
        );
    }
    let mut huge = b"SBXENV1\0".to_vec();
    huge.extend((u64::from(u32::MAX) - 16).to_le_bytes());
    let (ok, text) = run(&huge, "load", "");
    assert!(!ok);
    assert_eq!(resource_value(&text, 3), 0);
    assert!(text.contains(&format!("error|{}|", libc::EINVAL)));
    for flags in ["shrink", "grow", "bodyfail"] {
        let (ok, text) = run(&wire, "load", flags);
        assert!(!ok, "{text}");
        assert_eq!(
            resource_value(&text, 3),
            1,
            "only record allocated before changed/read failure: {text}"
        );
    }
    let (ok, text) = run(&wire, "load-setup", "shortread,compact,drop-source");
    assert!(ok, "{text}");
    assert!(text.contains("released|0|0"));
    assert!(text.contains(&format!(
        "content|/etc/demo|5|{}",
        digest(&[0, 255, 10, 0, 128])
    )));
}
#[test]
fn owned_record_failure_cleanup_and_borrowed_record_lifetime_are_explicit() {
    let wire = record(&environment(), false);
    let (ok, text) = run(&wire, "load-setup", "compact,drop-source");
    assert!(ok, "{text}");
    assert!(text.contains("slices|1|owned|1"));
    assert!(text.contains("released|0|0"));
    for flags in [
        "compact,drop-source,configownerfail",
        "compact,drop-source,closeconfig",
        "compact,drop-source,rofail",
    ] {
        let (ok, text) = run(&wire, "load-setup", flags);
        assert!(!ok, "{text}");
        assert!(text.contains("source-dropped"));
    }
    let mut bad_text = wire.clone();
    bad_text[20] = 0;
    let mut bad_tail = wire.clone();
    bad_tail.push(0);
    let body = bad_tail.len() - 16;
    bad_tail[8..16].copy_from_slice(&(body as u64).to_le_bytes());
    for bad in [bad_text, bad_tail] {
        let (ok, text) = run(&bad, "load", "compact");
        assert!(!ok);
        assert!(text.contains("record-storage|1|1"));
    }
    let (ok, text) = run(&wire, "setup", "");
    assert!(ok, "{text}");
    assert!(text.contains("slices|1|owned|0"));
    assert!(text.contains("released|0|0"));
}

#[test]
fn init_fatal_cleanup_releases_record_without_clearing_diagnostic_storage() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv/guest_service_init.c"),
    )
    .unwrap();
    let failure = &source
        [position(&source, "static void failed(void)")..position(&source, "static void mount_fs(")];
    assert!(
        position(failure, "int error = errno;")
            < position(failure, "sbx_env_release(&environment)")
    );
    assert!(
        position(failure, "sbx_env_release(&environment)")
            < position(failure, "dprintf(STDERR_FILENO")
    );
    assert!(position(failure, "dprintf(STDERR_FILENO") < position(failure, "poweroff()"));
    assert!(
        !failure.contains("sbx_env_destroy"),
        "release must preserve fixed diagnostic path/scalars"
    );
}
