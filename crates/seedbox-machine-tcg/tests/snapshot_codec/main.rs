//! Public-API, host-only structural persistence tests. Linux header/firmware
//! data and opaque migration strings are never booted, translated or executed.
#![cfg(unix)]
mod policy_tests;
mod scratch_tests;
use seedbox_machine::{
    definition::*,
    machine::{disk::Store, Machine},
    pv::bridge::GuestBridge,
    snapshot::{CodecError, DecodeLimits, Writer},
};
use seedbox_machine_tcg::machine::tcg::{Factory, Snapshot, TcgMachine};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    path::Path,
};

fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 8_000_000,
        allocation_bytes: 16_000_000,
        collection_items: 100_000,
        total_items: 300_000,
        string_bytes: 4096,
        blob_bytes: 4_000_000,
    }
}
fn encode(s: &Snapshot) -> Vec<u8> {
    let mut bytes = Vec::new();
    s.write_to(&mut bytes).unwrap();
    bytes
}
fn decode(bytes: &[u8]) -> Snapshot {
    Snapshot::read_from(&mut &bytes[..], &limits()).unwrap()
}
fn reject(bytes: &[u8], expected: &str) {
    let error = Snapshot::read_from(&mut &bytes[..], &limits())
        .err()
        .expect("rejected");
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?}, got {error}"
    );
}
fn word(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn blob(out: &mut Vec<u8>, b: &[u8]) {
    word(out, b.len() as u64);
    out.extend_from_slice(b);
}
fn opt(out: &mut Vec<u8>, n: Option<u64>) {
    out.push(u8::from(n.is_some()));
    if let Some(n) = n {
        word(out, n);
    }
}
fn patch64(out: &mut [u8], at: usize, n: u64) {
    out[at..at + 8].copy_from_slice(&n.to_le_bytes());
}
fn overlays(sectors: u64, filled: bool) -> Vec<u8> {
    let mut out = b"SBXSTOR1".to_vec();
    word(&mut out, sectors);
    for value in [0x31, 0x72] {
        word(&mut out, u64::from(filled));
        if filled {
            word(&mut out, 1);
            out.extend_from_slice(&[value; 512]);
        }
    }
    out
}
#[derive(Default)]
struct Bridge {
    partial: Vec<u8>,
    pending: Vec<(u32, String)>,
    used: Vec<u32>,
    last: Option<u64>,
    failed: bool,
    prepared: u64,
}
impl Bridge {
    fn wire(&self) -> (Vec<u8>, BTreeMap<&'static str, usize>) {
        let mut b = Vec::new();
        let mut marks = BTreeMap::new();
        blob(&mut b, &self.partial);
        marks.insert("pending_count", b.len());
        word(&mut b, self.pending.len() as u64);
        marks.insert("pending_body", b.len());
        for (id, action) in &self.pending {
            b.extend_from_slice(&id.to_le_bytes());
            blob(&mut b, action.as_bytes());
        }
        marks.insert("used_count", b.len());
        word(&mut b, self.used.len() as u64);
        for id in &self.used {
            b.extend_from_slice(&id.to_le_bytes());
        }
        opt(&mut b, self.last);
        b.extend_from_slice(&[0, 0, u8::from(self.failed)]); // ready, outcome, failed
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(&[0; 4]); // oracle relay/session/public, failed/time/pending/prepared
        b.extend_from_slice(&0u32.to_le_bytes());
        b.push(0); // highwater and session map
        marks.insert("prepared_count", b.len());
        word(&mut b, self.prepared);
        (b, marks)
    }
}
struct Fixture {
    command: String,
    policy: Option<(String, BackendPolicy)>,
    logical_boot: u64,
    memory: u32,
    network: Option<[u8; 6]>,
    epoch: u32,
    reached: u64,
    state: u8,
    deadline: Option<u64>,
    seed: u64,
    pending: u64,
    pending_at: Option<u64>,
    shutdown: bool,
    writes: u64,
    input: Vec<u8>,
    bridge: Bridge,
    bridge_wire: Option<Vec<u8>>,
    base_tag: u8,
    capacity: u64,
    client: bool,
    kernel: Vec<u8>,
    initrd: Vec<u8>,
    firmware: Vec<u8>,
    image: Option<Vec<u8>>,
    overlay: Vec<u8>,
    base: Vec<u8>,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            command: " exact=args\n".into(),
            policy: None,
            logical_boot: 0,
            memory: 8,
            network: None,
            epoch: 1_234_567,
            reached: 0,
            state: 0,
            deadline: None,
            seed: 0,
            pending: 1,
            pending_at: None,
            shutdown: false,
            writes: 0,
            input: Vec::new(),
            bridge: Bridge::default(),
            bridge_wire: None,
            base_tag: 0,
            capacity: 0,
            client: false,
            kernel: seedbox_machine::test_support::assets::bzimage_header(),
            initrd: b"opaque initramfs data".to_vec(),
            firmware: vec![0xa5; 65536],
            image: None,
            overlay: Vec::new(),
            base: Vec::new(),
        }
    }
}
impl Fixture {
    fn live() -> Self {
        Self {
            image: Some(b"synthetic opaque migration; not QEMU-validated".to_vec()),
            reached: 123,
            pending: 0,
            seed: 0xfedcba9876543210,
            ..Self::default()
        }
    }
    fn ended() -> Self {
        Self {
            state: 3,
            reached: 99,
            pending: 0,
            seed: 77,
            ..Self::default()
        }
    }
    fn disk(mut self, image_base: bool) -> Self {
        self.base_tag = if image_base { 2 } else { 1 };
        self.capacity = 2048;
        if image_base {
            self.base = vec![0x9a; 2048];
        }
        self.client = self.image.is_none();
        if self.client {
            self.overlay = overlays(4, true);
        }
        self
    }
    fn wire(&self) -> (Vec<u8>, BTreeMap<&'static str, usize>) {
        let mut b = vec![0; 24];
        let mut marks = BTreeMap::new();
        macro_rules! mark {
            ($name:expr) => {
                marks.insert($name, b.len());
            };
        }
        mark!("boot");
        b.push(1);
        mark!("memory");
        b.extend_from_slice(&self.memory.to_le_bytes());
        mark!("network");
        b.push(u8::from(self.network.is_some()));
        if let Some(mac) = self.network {
            b.extend_from_slice(&mac);
        }
        mark!("command");
        blob(&mut b, self.command.as_bytes());
        b.extend_from_slice(&self.epoch.to_le_bytes());
        mark!("reached");
        word(&mut b, self.reached);
        mark!("state");
        b.push(self.state);
        if self.state == 1 {
            opt(&mut b, self.deadline);
        }
        word(&mut b, self.seed);
        word(&mut b, self.pending);
        opt(&mut b, self.pending_at);
        mark!("shutdown");
        b.push(u8::from(self.shutdown));
        mark!("writes");
        word(&mut b, self.writes);
        mark!("input");
        blob(&mut b, &self.input);
        let (bridge, bmarks) = self.bridge.wire();
        let start = b.len();
        for (key, offset) in bmarks {
            marks.insert(key, start + offset);
        }
        b.extend_from_slice(self.bridge_wire.as_deref().unwrap_or(&bridge));
        mark!("logical_boot");
        word(&mut b, self.logical_boot);
        mark!("active_boot");
        opt(&mut b, self.image.as_ref().map(|_| self.logical_boot));
        mark!("policy");
        b.push(u8::from(self.policy.is_some()));
        if let Some((name, c)) = &self.policy {
            blob(&mut b, name.as_bytes());
            word(&mut b, c.seed);
            word(&mut b, c.timer_jitter_ns);
            word(&mut b, c.stop_tolerance_ns);
            b.extend_from_slice(&[
                u8::from(c.torn_writes),
                u8::from(c.full_state),
                u8::from(c.seeded_pv_speed),
                u8::from(c.boot_end_evidence),
            ]);
            b.push(u8::from(c.controller.is_some()));
            if let Some(s) = &c.controller {
                blob(&mut b, s.as_bytes());
            }
        }
        b.extend_from_slice(&[0, 0]); // no last-ended identity or cached RAM yet
        mark!("native_witness");
        b.extend_from_slice(&[0, 0]); // no ended native chain or predecessor
        word(&mut b, 0); // no pending boot-end reports
        mark!("disk_tag");
        b.push(self.base_tag);
        if self.base_tag != 0 {
            word(&mut b, self.capacity);
        }
        mark!("client");
        b.push(u8::from(self.client));
        let bodies = [
            &self.kernel[..],
            &self.initrd,
            &self.firmware,
            self.image.as_deref().unwrap_or(&[]),
            &self.overlay,
            &self.base,
        ];
        for (key, body) in [
            "kernel_len",
            "initrd_len",
            "firmware_len",
            "image_len",
            "overlay_len",
            "base_len",
        ]
        .into_iter()
        .zip(bodies)
        {
            mark!(key);
            word(&mut b, body.len() as u64);
        }
        mark!("raw");
        for (key, body) in [
            "kernel_body",
            "initrd_body",
            "firmware_body",
            "image_body",
            "overlay_body",
            "base_body",
        ]
        .into_iter()
        .zip(bodies)
        {
            mark!(key);
            b.extend_from_slice(body);
        }
        b[..8].copy_from_slice(b"SBXTCG01");
        b[8..12].copy_from_slice(&3u32.to_le_bytes());
        b[12] = 1;
        b[13] = u8::from(self.image.is_some());
        let len = b.len() as u64 - 24;
        patch64(&mut b, 16, len);
        (b, marks)
    }
}

#[test]
fn independent_linux_closure_roundtrips_every_ownership_shape() {
    let mut fixtures = vec![
        Fixture::default(),
        Fixture::ended(),
        Fixture::default().disk(false),
        Fixture::ended().disk(true),
        Fixture::live(),
        Fixture::live().disk(false),
        Fixture::live().disk(true),
    ];
    for deadline in [None, Some(124)] {
        fixtures.push(Fixture {
            state: 1,
            deadline,
            ..Fixture::live()
        });
    }
    for state in [2, 3] {
        fixtures.push(Fixture {
            state,
            shutdown: true,
            ..Fixture::live()
        });
    }
    fixtures.push(Fixture {
        shutdown: true,
        ..Fixture::ended()
    });
    fixtures.push(Fixture {
        network: Some([2, 1, 2, 3, 4, 5]),
        ..Fixture::default()
    }); // No invented command-line agreement.
    for f in fixtures {
        let (wire, _) = f.wire();
        let snapshot = decode(&wire);
        drop(f); // Everything now comes only from the decoded owned snapshot.
        assert_eq!(encode(&snapshot), wire);
        let second = decode(&wire);
        drop(snapshot);
        assert_eq!(encode(&second), wire);
    }
}

#[test]
fn original_both_layers_and_image_base_are_not_promoted_or_flattened() {
    let f = Fixture::ended().disk(true);
    let (wire, m) = f.wire();
    let s = decode(&wire);
    let encoded = encode(&s);
    assert_eq!(encoded, wire);
    let raw = &encoded[m["overlay_body"]..m["base_body"]];
    let mut store = Store::from_image(f.base).unwrap();
    store.replace_overlays(raw).unwrap();
    assert_eq!(store.encode_overlays(), f.overlay);
    // Restoring and recapturing a non-live disk never spawns the supplied path.
    let mut machine = TcgMachine::from_snapshot(Path::new("/does/not/exist/qemu"), &s).unwrap();
    assert_eq!(encode(&machine.snapshot().unwrap()), wire);
    drop(machine);
    assert_eq!(encode(&s), wire);
}

#[test]
fn zero_base_remains_compact_even_above_host_memory_sizes() {
    let f = Fixture {
        capacity: u64::MAX - 511,
        overlay: overlays((u64::MAX - 511) / 512, false),
        ..Fixture::default().disk(false)
    };
    let (wire, _) = f.wire();
    assert!(wire.len() < 100_000);
    assert_eq!(encode(&decode(&wire)), wire);
}

#[test]
fn genuine_factory_unstarted_snapshot_outlives_assets_factory_and_machine() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("kernel"),
        seedbox_machine::test_support::assets::bzimage_header(),
    )
    .unwrap();
    // Existing structural ELF header data, never a new raw executable fixture.
    std::fs::write(
        root.join("init"),
        seedbox_machine::test_support::assets::image(62),
    )
    .unwrap();
    std::fs::write(root.join("firmware"), vec![0x55; 65536]).unwrap();
    std::fs::write(root.join("command-line"), " exact=unstarted\n").unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    tar.finish().unwrap();
    let definition = MachineDefinition {
        name: "m".into(),
        resources: Resources {
            memory_bytes: 8 * 1024 * 1024,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
            launch: Launch {
                argv: vec!["/bin/app".into()],
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0".into(),
            },
        },
        disk: Some(DiskImage::from_bytes(vec![0x6a; 1024]).unwrap()),
        network: None,
    };
    let mut factory = Factory::new(&std::env::current_exe().unwrap(), root, 91).unwrap();
    let mut machine = factory.create(&definition).unwrap();
    let s = machine.snapshot().unwrap();
    drop(machine);
    drop(factory);
    drop(dir);
    drop(definition);
    let wire = encode(&s);
    let recovered = decode(&wire);
    drop(s);
    let mut machine = TcgMachine::from_snapshot(Path::new("/never/spawned"), &recovered).unwrap();
    assert_eq!(encode(&machine.snapshot().unwrap()), wire);
}

#[test]
fn pending_order_reserved_ids_partial_framing_and_time_are_preserved() {
    let bridge = Bridge {
        partial: b"SBX partial".to_vec(),
        pending: vec![(9, "stop".into()), (2, "start".into())],
        used: vec![1, 2, 9],
        last: Some(123),
        ..Bridge::default()
    };
    let (wire, _) = Fixture {
        bridge,
        ..Fixture::live()
    }
    .wire();
    assert_eq!(encode(&decode(&wire)), wire);
    let mut f = Fixture::live();
    f.bridge.last = Some(124);
    reject(&f.wire().0, "bridge time ordering");
    for bridge in [
        Bridge {
            failed: true,
            ..Bridge::default()
        },
        Bridge {
            partial: b"bad\n".to_vec(),
            ..Bridge::default()
        },
        Bridge {
            partial: b"bad\0".to_vec(),
            ..Bridge::default()
        },
        Bridge {
            pending: vec![(1, "start".into())],
            ..Bridge::default()
        },
        Bridge {
            pending: vec![(1, "alien".into())],
            used: vec![1],
            ..Bridge::default()
        },
        Bridge {
            pending: vec![(1, "start".into()), (1, "stop".into())],
            used: vec![1],
            ..Bridge::default()
        },
        Bridge {
            used: vec![2, 1],
            ..Bridge::default()
        },
        Bridge {
            used: vec![1, 1],
            ..Bridge::default()
        },
    ] {
        assert!(Snapshot::read_from(
            &mut &Fixture {
                bridge,
                ..Fixture::live()
            }
            .wire()
            .0[..],
            &limits()
        )
        .is_err());
    }
}

#[test]
fn bridge_default_wire_is_the_actual_public_bridge_encoding() {
    let mut actual = Vec::new();
    GuestBridge::default()
        .write_snapshot(&mut Writer::new(&mut actual))
        .unwrap();
    assert_eq!(Bridge::default().wire().0, actual);
}

#[test]
fn malformed_header_version_architecture_native_and_tags_are_closed() {
    let (wire, m) = Fixture::default().wire();
    for (at, value, message) in [
        (0, 0, "format or version"),
        (8, 1, "format or version"),
        (12, 2, "architecture or native format"),
        (13, 2, "architecture or native format"),
        (14, 1, "architecture or native format"),
        (15, 1, "architecture or native format"),
        (m["boot"], 0, "boot kind"),
        (m["network"], 2, "boolean tag"),
        (m["shutdown"], 2, "boolean tag"),
        (m["state"], 4, "machine state"),
        (m["state"], 255, "machine state"),
        (m["disk_tag"], 3, "disk base tag"),
        (m["client"], 2, "boolean tag"),
    ] {
        let mut bad = wire.clone();
        bad[at] = value;
        reject(&bad, message);
    }
    let mut bad = wire.clone();
    patch64(&mut bad, 16, u64::MAX);
    reject(&bad, "encoded bytes");
    let mut bad = wire;
    bad[m["command"] + 8] = 0xff;
    reject(&bad, "UTF-8");
}

#[test]
fn capture_boundaries_lifecycle_boot_and_network_reject_source_impossible_states() {
    for f in [
        Fixture {
            state: 4,
            ..Fixture::default()
        },
        Fixture {
            memory: 0,
            ..Fixture::default()
        },
        Fixture {
            pending: 2,
            ..Fixture::default()
        },
        Fixture {
            pending_at: Some(0),
            ..Fixture::default()
        },
        Fixture {
            seed: 1,
            ..Fixture::default()
        },
        Fixture {
            reached: 1,
            ..Fixture::default()
        },
        Fixture {
            shutdown: true,
            ..Fixture::default()
        },
        Fixture {
            pending: 0,
            ..Fixture::default()
        },
        Fixture {
            state: 1,
            pending: 0,
            ..Fixture::default()
        },
        Fixture {
            pending: 1,
            ..Fixture::live()
        },
        Fixture {
            reached: i64::MAX as u64 + 1,
            ..Fixture::live()
        },
        Fixture {
            state: 1,
            deadline: Some(123),
            ..Fixture::live()
        },
        Fixture {
            state: 2,
            ..Fixture::live().disk(false)
        },
        Fixture {
            image: Some(vec![]),
            ..Fixture::live()
        },
        Fixture {
            command: "nul\0".into(),
            ..Fixture::default()
        },
        Fixture {
            command: "x".repeat(2048),
            ..Fixture::default()
        },
        Fixture {
            network: Some([0; 6]),
            ..Fixture::default()
        },
        Fixture {
            network: Some([1, 2, 3, 4, 5, 6]),
            ..Fixture::default()
        },
        Fixture {
            network: Some([255; 6]),
            ..Fixture::default()
        },
    ] {
        assert!(Snapshot::read_from(&mut &f.wire().0[..], &limits()).is_err());
    }
    let (wire, _) = Fixture {
        command: "x".repeat(2047),
        ..Fixture::default()
    }
    .wire();
    assert_eq!(encode(&decode(&wire)), wire);
    for body in [0, 1, 2] {
        let mut f = Fixture::default();
        match body {
            0 => f.kernel[0x202] = 0,
            1 => f.firmware.pop().map(|_| ()).unwrap(),
            _ => f.initrd = vec![0; 9 * 1024 * 1024],
        };
        let mut l = limits();
        l.encoded_bytes = 32_000_000;
        l.allocation_bytes = 64_000_000;
        l.blob_bytes = 16_000_000;
        assert!(Snapshot::read_from(&mut &f.wire().0[..], &l).is_err());
    }
}

#[test]
fn every_truncated_prefix_and_trailing_byte_is_rejected() {
    let wire = Fixture::default().wire().0;
    for n in 0..wire.len() {
        assert!(
            Snapshot::read_from(&mut &wire[..n], &limits()).is_err(),
            "prefix {n}"
        );
    }
    let mut bad = wire.clone();
    bad.push(0);
    reject(&bad, "trailing bytes");
    let mut bad = wire.clone();
    patch64(&mut bad, 16, wire.len() as u64 - 25);
    reject(&bad, "raw body framing");
    let mut bad = wire.clone();
    patch64(&mut bad, 16, wire.len() as u64 - 23);
    reject(&bad, "raw body framing");
}

#[test]
fn disk_base_geometry_image_length_and_layer_validation_are_exact() {
    for f in [
        Fixture {
            capacity: 0,
            ..Fixture::default().disk(false)
        },
        Fixture {
            capacity: 513,
            ..Fixture::default().disk(false)
        },
        Fixture {
            client: false,
            ..Fixture::default().disk(false)
        },
        Fixture {
            client: true,
            ..Fixture::default()
        },
        Fixture {
            client: true,
            overlay: overlays(4, false),
            ..Fixture::live().disk(false)
        },
        Fixture {
            base: vec![0; 512],
            ..Fixture::ended().disk(true)
        },
        Fixture {
            base: vec![0; 512],
            ..Fixture::ended().disk(false)
        },
    ] {
        assert!(Snapshot::read_from(&mut &f.wire().0[..], &limits()).is_err());
    }
    let mut f = Fixture::ended().disk(true);
    f.client = false;
    f.overlay.clear();
    reject(&f.wire().0, "TCG snapshot disk does not match its manifest");
    for kind in 0..7 {
        let mut f = Fixture::ended().disk(false);
        match kind {
            0 => f.overlay[0] = 0,
            1 => patch64(&mut f.overlay, 8, 3),
            2 => patch64(&mut f.overlay, 16, u64::MAX),
            3 => patch64(&mut f.overlay, 24, 4),
            4 => {
                f.overlay.pop();
            }
            5 => f.overlay.push(0),
            _ => {
                f.overlay.truncate(31);
            }
        }
        assert!(
            Snapshot::read_from(&mut &f.wire().0[..], &limits()).is_err(),
            "overlay {kind}"
        );
    }
}

struct PrefixOnly<'a>(&'a [u8]);
impl Read for PrefixOnly<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        assert!(!self.0.is_empty(), "forbidden body read");
        let n = out.len().min(self.0.len());
        out[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}
fn prefix_error(wire: &[u8], end: usize, l: &DecodeLimits, expected: &str) {
    let error = Snapshot::read_from(&mut PrefixOnly(&wire[..end]), l)
        .err()
        .expect("rejected before body");
    assert!(error.to_string().contains(expected), "{error}");
}
#[test]
fn quotas_and_forbidden_counts_fail_before_corresponding_body_reads() {
    let (wire, m) = Fixture::default().wire();
    let mut l = limits();
    l.encoded_bytes = wire.len() as u64 - 1;
    prefix_error(&wire, 24, &l, "encoded bytes");
    l = limits();
    l.encoded_bytes = 23;
    prefix_error(&wire, 0, &l, "encoded bytes");
    l = limits();
    l.string_bytes = 0;
    prefix_error(&wire, m["command"] + 8, &l, "string bytes");
    l = limits();
    l.blob_bytes = 100;
    prefix_error(&wire, m["kernel_len"] + 8, &l, "blob bytes");
    l = limits();
    l.allocation_bytes = 1;
    prefix_error(&wire, m["command"] + 8, &l, "allocation bytes");
    // All six raw bodies and ownership copies are charged before the first byte.
    let charged = Fixture::default().command.len() as u64
        + 2 * (Fixture::default().kernel.len() + Fixture::default().initrd.len() + 65536) as u64;
    l = limits();
    l.allocation_bytes = charged - 1;
    prefix_error(&wire, m["raw"], &l, "allocation bytes");
    l.allocation_bytes = charged;
    assert_eq!(
        encode(&Snapshot::read_from(&mut &wire[..], &l).unwrap()),
        wire
    );
    for name in ["writes", "input"] {
        let mut bad = wire.clone();
        patch64(&mut bad, m[name], u64::MAX);
        prefix_error(&bad, m[name] + 8, &limits(), "capture boundary ownership");
    }
    for n in [1, 50_000, u64::MAX] {
        let mut bad = wire.clone();
        patch64(&mut bad, m["prepared_count"], n);
        prefix_error(
            &bad,
            m["prepared_count"] + 8,
            &limits(),
            "prepared reply delivery ownership",
        );
    }
    let mut bad = wire.clone();
    patch64(&mut bad, m["pending_count"], u64::MAX);
    prefix_error(
        &bad,
        m["pending_count"] + 8,
        &limits(),
        "collection count exceeds body",
    );
    for (at, value) in [(12, 2), (13, 2), (14, 1)] {
        let mut bad = wire.clone();
        bad[at] = value;
        prefix_error(&bad, 24, &limits(), "architecture or native format");
    }
}

#[test]
fn pending_identity_index_is_charged_and_order_remains_unmodified() {
    let f = Fixture {
        bridge: Bridge {
            pending: vec![(9, "stop".into()), (2, "start".into())],
            used: vec![1, 2, 9],
            ..Bridge::default()
        },
        ..Fixture::default()
    };
    let (wire, m) = f.wire();
    let mut l = limits();
    l.collection_items = 1;
    prefix_error(&wire, m["pending_body"], &l, "collection items");
    l = limits();
    l.total_items = 3;
    prefix_error(&wire, m["pending_body"], &l, "aggregate items"); // two retained + two identity scratch
    l = limits();
    l.allocation_bytes =
        f.command.len() as u64 + 2 * std::mem::size_of::<(std::num::NonZeroU32, String)>() as u64;
    prefix_error(&wire, m["pending_body"], &l, "allocation bytes"); // copied identities, before element body
    assert_eq!(encode(&decode(&wire)), wire);
}

#[test]
fn blob_ownership_copies_and_single_migration_have_exact_logical_charges() {
    for f in [
        Fixture::live().disk(true),
        Fixture::ended().disk(true),
        Fixture::live(),
        Fixture::default().disk(false),
    ] {
        let (wire, m) = f.wire();
        let exact = f.command.len() as u64
            + 2 * (f.kernel.len() + f.initrd.len() + f.firmware.len() + f.base.len()) as u64
            + f.image.as_ref().map_or(0, Vec::len) as u64
            + f.overlay.len() as u64;
        let mut l = limits();
        l.allocation_bytes = exact - 1;
        prefix_error(&wire, m["raw"], &l, "allocation bytes");
        l.allocation_bytes = exact;
        let decoded = Snapshot::read_from(&mut &wire[..], &l).unwrap();
        assert_eq!(encode(&decoded), wire);
    }
}

#[test]
fn boot_validation_precedes_opaque_migration_body_reads() {
    let mut f = Fixture::live();
    f.kernel[0x202] = 0;
    let (wire, m) = f.wire();
    prefix_error(&wire, m["initrd_body"], &limits(), "x86-64 bzImage");
    let mut f = Fixture::live();
    f.firmware.pop();
    let (wire, m) = f.wire();
    prefix_error(&wire, m["image_body"], &limits(), "x86 BIOS");
}

struct Output {
    bytes: Vec<u8>,
    max: usize,
    left: usize,
    zero: bool,
    interrupt: bool,
}
impl Write for Output {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        assert!(b.len() <= 65536, "unbounded writer chunk");
        if self.interrupt {
            self.interrupt = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.left == 0 {
            return if self.zero {
                Ok(0)
            } else {
                Err(io::Error::other("caller sink error"))
            };
        }
        let n = b.len().min(self.max).min(self.left);
        self.bytes.extend_from_slice(&b[..n]);
        self.left -= n;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("codec must not flush caller stream")
    }
}
#[test]
fn writer_bounded_borrowing_partial_short_zero_and_io_errors_leave_snapshot_reusable() {
    let f = Fixture {
        image: Some(vec![0x4a; 200_000]),
        ..Fixture::live().disk(true)
    };
    let (wire, _) = f.wire();
    let snapshot = decode(&wire);
    let mut short = Output {
        bytes: Vec::new(),
        max: 17,
        left: usize::MAX,
        zero: false,
        interrupt: true,
    };
    snapshot.write_to(&mut short).unwrap();
    assert_eq!(short.bytes, wire);
    for at in [0, 1, 23, 24, 100, wire.len() / 2, wire.len() - 1] {
        for zero in [true, false] {
            let mut sink = Output {
                bytes: Vec::new(),
                max: 65536,
                left: at,
                zero,
                interrupt: false,
            };
            let CodecError::Io(error) = snapshot.write_to(&mut sink).unwrap_err() else {
                panic!("ordinary I/O error")
            };
            assert_eq!(
                error.kind(),
                if zero {
                    io::ErrorKind::WriteZero
                } else {
                    io::ErrorKind::Other
                }
            );
            assert_eq!(sink.bytes, wire[..at]);
            assert_eq!(encode(&snapshot), wire);
        }
    }
    let mut full = Output {
        bytes: Vec::new(),
        max: usize::MAX,
        left: usize::MAX,
        zero: false,
        interrupt: false,
    };
    snapshot.write_to(&mut full).unwrap();
    assert_eq!(full.bytes, wire);
}

#[test]
fn migration_reads_target_the_retained_allocation_and_writes_borrow_it() {
    let f = Fixture {
        image: Some(vec![0x67; 200_003]),
        ..Fixture::live()
    };
    let (wire, m) = f.wire();
    let range = m["image_body"]..m["overlay_body"];
    struct WitnessRead<'a> {
        bytes: &'a [u8],
        offset: usize,
        range: std::ops::Range<usize>,
        pointers: Vec<(usize, usize)>,
    }
    impl Read for WitnessRead<'_> {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let n = b.len().min(self.bytes.len());
            if self.range.contains(&self.offset) {
                assert!(self.offset + n <= self.range.end);
                assert!(n <= 65536);
                self.pointers.push((b.as_ptr() as usize, n));
            }
            b[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            self.offset += n;
            Ok(n)
        }
    }
    let mut reader = WitnessRead {
        bytes: &wire,
        offset: 0,
        range: range.clone(),
        pointers: Vec::new(),
    };
    let s = Snapshot::read_from(&mut reader, &limits()).unwrap();
    assert_eq!(
        reader.pointers.iter().map(|(_, n)| n).sum::<usize>(),
        range.len()
    );
    for adjacent in reader.pointers.windows(2) {
        assert_eq!(adjacent[0].0 + adjacent[0].1, adjacent[1].0);
    }
    struct WitnessWrite {
        bytes: Vec<u8>,
        range: std::ops::Range<usize>,
        pointers: std::collections::VecDeque<(usize, usize)>,
    }
    impl Write for WitnessWrite {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            assert!(b.len() <= 65536);
            if self.range.contains(&self.bytes.len()) {
                assert_eq!(
                    self.pointers.pop_front().unwrap(),
                    (b.as_ptr() as usize, b.len()),
                    "migration copied or staged"
                );
            }
            self.bytes.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("no caller flush")
        }
    }
    let mut writer = WitnessWrite {
        bytes: Vec::new(),
        range,
        pointers: reader.pointers.into(),
    };
    s.write_to(&mut writer).unwrap();
    assert!(writer.pointers.is_empty());
    assert_eq!(writer.bytes, wire);
}

#[test]
fn body_lengths_overflows_and_native_ownership_reject_before_raw_bytes() {
    let (wire, m) = Fixture::default().wire();
    for field in [
        "kernel_len",
        "initrd_len",
        "firmware_len",
        "image_len",
        "overlay_len",
        "base_len",
    ] {
        let mut bad = wire.clone();
        patch64(&mut bad, m[field], u64::MAX);
        prefix_error(&bad, m[field] + 8, &limits(), "blob exceeds body");
    }
    let mut bad = wire.clone();
    bad[13] = 1;
    prefix_error(&bad, m["raw"], &limits(), "raw body ownership or capacity");
    let (mut bad, m) = Fixture::live().wire();
    bad[13] = 0;
    prefix_error(&bad, m["raw"], &limits(), "raw body ownership or capacity");
    let f = Fixture {
        memory: 2048,
        ..Fixture::default()
    };
    let (mut bad, m) = f.wire();
    patch64(&mut bad, m["image_len"], 2 * 1024 * 1024 * 1024);
    // Declared body covers the image, but the caller's blob quota rejects its
    // length before any image read or reservation, without allocating 2 GiB.
    let declared = bad.len() as u64 - 24 + 2 * 1024 * 1024 * 1024;
    patch64(&mut bad, 16, declared);
    let mut l = limits();
    l.encoded_bytes = declared + 24;
    prefix_error(&bad, m["image_len"] + 8, &l, "blob bytes");
}

#[test]
fn arbitrary_reader_short_reads_interruptions_and_failures_are_ordinary_io() {
    struct Short<'a> {
        bytes: &'a [u8],
        fail: bool,
        interrupt: bool,
    }
    impl Read for Short<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.interrupt {
                self.interrupt = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.fail && self.bytes.len() < 100 {
                return Err(io::Error::other("reader failure"));
            }
            let n = 3.min(out.len()).min(self.bytes.len());
            out[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }
    let wire = Fixture::live().wire().0;
    let mut reader = Short {
        bytes: &wire,
        fail: false,
        interrupt: true,
    };
    assert_eq!(
        encode(&Snapshot::read_from(&mut reader, &limits()).unwrap()),
        wire
    );
    let mut reader = Short {
        bytes: &wire,
        fail: true,
        interrupt: false,
    };
    assert!(matches!(
        Snapshot::read_from(&mut reader, &limits()),
        Err(CodecError::Io(_))
    ));
    assert_eq!(encode(&decode(&wire)), wire);
}

#[test]
fn native_chain_codec_rejects_invalid_values_and_charges_before_body_reads() {
    let (wire, m) = Fixture::default().wire();
    let at = m["native_witness"];
    let replace = |native: &[u8]| {
        let mut b = wire[..at].to_vec();
        b.extend_from_slice(native);
        b.extend_from_slice(&wire[at + 2..]);
        let len = b.len() as u64 - 24;
        patch64(&mut b, 16, len);
        b
    };
    for (old, cause) in [
        ("not-sha256", 1),
        (&"0".repeat(64), 0),
        (&"A".repeat(64), 1),
    ] {
        let mut native = vec![0, 1];
        blob(&mut native, old.as_bytes());
        native.push(cause);
        reject(&replace(&native), "native witness");
    }
    // Strings use the existing Reader quotas, not an uncharged JSON parser.
    let mut native = vec![0, 1];
    blob(&mut native, "0".repeat(64).as_bytes());
    native.push(1);
    let bad = replace(&native);
    let mut l = limits();
    l.string_bytes = 63;
    prefix_error(&bad, at + 2 + 8, &l, "string bytes");
    // A well-formed orphan predecessor is rejected by ownership validation.
    reject(&bad, "native witness");
}

// Synthetic retained records, NOT captured native execution. Build independent
// public codec3 bytes so populated private cache/predecessor state is exercised
// without exposing internals, including an older physical owner after an
// unbooted logical retirement. All boot inputs remain Fixture's owned bytes.
fn synthetic_native_retirement(
    disposition: u8,
    code: i32,
    cause: u8,
    older_owner: bool,
) -> (Vec<u8>, BTreeMap<&'static str, usize>) {
    let mut f = Fixture::ended();
    f.logical_boot = if older_owner { 8 } else { 7 };
    f.state = if disposition == 2 && !older_owner {
        2
    } else {
        3
    };
    f.policy = Some(("node".into(), BackendPolicy::default()));
    f.command.push_str(" seedbox.control=0");
    let (original, original_marks) = f.wire();
    let at = original_marks["native_witness"];
    assert_eq!(&original[at - 2..at + 2], &[0, 0, 0, 0]);
    let mut b = original[..at - 2].to_vec();
    let mut marks = BTreeMap::new();
    b.push(1); // actual retained last-ended identity present
    word(&mut b, f.logical_boot);
    // Slot identity differs from the older cached physical endpoint on purpose.
    b.push(if f.state == 2 { 1 } else { 0 });
    marks.insert("slot_native", b.len());
    b.push(u8::from(!older_owner));
    b.push(0); // optional RAM evidence is disabled
    b.push(1); // cached ended native witness present
    marks.insert("cache_boot", b.len());
    word(&mut b, 7);
    marks.insert("cache_disposition", b.len());
    b.push(disposition);
    marks.insert("cache_code", b.len());
    b.extend_from_slice(&code.to_le_bytes());
    blob(&mut b, "ab".repeat(32).as_bytes());
    b.push(1); // accepted predecessor present
    let predecessor_at = b.len();
    blob(&mut b, "ab".repeat(32).as_bytes());
    marks.insert("predecessor_hash", predecessor_at + 8);
    marks.insert("cause", b.len());
    b.push(cause);
    b.extend_from_slice(&original[at + 2..]);
    let length = b.len() as u64 - 24;
    patch64(&mut b, 16, length);
    (b, marks)
}

#[test]
fn populated_native_cache_and_predecessor_roundtrip_preserves_actual_endpoint() {
    use seedbox_machine::machine::EvidenceDisposition;
    for (disposition, code, cause, older_owner) in [
        (1, 0, 1, false),       // accepted Crash predecessor
        (1, 0, 2, false),       // live forced Reboot; do not normalize to Crash cause
        (2, 0, 3, false),       // natural shutdown, then Reboot
        (3, -17, 3, true),      // actual ended owner survives an unbooted logical crash
        (3, i32::MIN, 3, true), // exact signed exit-code retention
    ] {
        let (bytes, marks) = synthetic_native_retirement(disposition, code, cause, older_owner);
        assert_eq!(&bytes[8..12], &3u32.to_le_bytes());
        let snapshot = decode(&bytes);
        assert_eq!(encode(&snapshot), bytes);
        snapshot
            .validate_policy("node", Some(&BackendPolicy::default()))
            .unwrap();
        let expected_disposition = match disposition {
            1 => EvidenceDisposition::Crashed,
            2 => EvidenceDisposition::Shutdown,
            3 => EvidenceDisposition::Exited { code },
            _ => unreachable!(),
        };
        for _ in 0..2 {
            // No live migration image exists: this restores only owned retained
            // bytes and cannot start this nonexistent executable.
            let mut machine =
                TcgMachine::from_snapshot(Path::new("/never-started/qemu"), &snapshot).unwrap();
            for _ in 0..3 {
                let evidence = machine.execution_evidence().unwrap().unwrap();
                assert_eq!(evidence.boot, 7); // NOT logical slot8 for older_owner
                assert_eq!(evidence.disposition, expected_disposition);
                assert_eq!(evidence.witness, "ab".repeat(32));
                assert!(evidence.disk.is_none());
                assert!(machine.state_evidence().unwrap().is_none());
                let restored = encode(&machine.snapshot().unwrap());
                assert_eq!(restored[marks["cause"]], cause);
                assert_eq!(restored, bytes); // read-only cache, cause and cursor
            }
        }
    }
}

#[test]
fn populated_native_cache_rejects_mismatched_predecessor_and_retirement() {
    let (valid, marks) = synthetic_native_retirement(1, 0, 1, false);
    decode(&valid); // ensure every mutation begins from an accepted record
    for key in [
        "predecessor_hash",
        "cause",
        "cache_disposition",
        "slot_native",
    ] {
        let mut bad = valid.clone();
        bad[marks[key]] = match key {
            "predecessor_hash" => b'c', // still valid lowercase SHA, wrong root
            "cause" => 3,               // a natural predecessor cannot describe crashed cache
            "cache_disposition" => 2,   // cached Shutdown != actual ended Crash
            "slot_native" => 0,         // cached same-boot native owner != unbooted slot
            _ => unreachable!(),
        };
        reject(&bad, "native witness");
    }
    let mut future = valid.clone();
    patch64(&mut future, marks["cache_boot"], 8);
    reject(&future, "native witness");
    let mut nonexit_code = valid;
    nonexit_code[marks["cache_code"]] = 1;
    reject(&nonexit_code, "native witness");

    let (mut natural, marks) = synthetic_native_retirement(2, 0, 3, false);
    decode(&natural);
    natural[marks["cause"]] = 2; // live forced cause cannot relabel natural end
    reject(&natural, "native witness");
}
