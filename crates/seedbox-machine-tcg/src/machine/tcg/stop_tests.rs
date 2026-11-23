//! Rule C3 on a native TCG machine: a stop that delivers nothing leaves no
//! trace. The TCG Linux fixture boots and runs two shell loops that fork,
//! read the clock and sleep. The shared scenario
//! (`seedbox_machine::test_support::stop_scenario`, which the PV crate's
//! `pv::stop_tests` drives too) runs it with the machine's own stops and host
//! barriers every half millisecond, and again with the same barriers and
//! runs that end early at seeded times, a few microseconds apart, and
//! nanoseconds (a few instructions) apart from boot and after each wake. At
//! every barrier the native witness and the console must agree, and so must
//! the full state evidence (registers, devices, timers, RAM) at every
//! `STATE`th barrier and the last.
//!
//! What it covers is this one guest: one CPU, no frames, disk or host input.
use super::tests::lifecycle_initramfs;
use super::*;
use seedbox_machine::{execution::ExecutionConfig, test_support::stop_scenario};

/// Full state evidence is taken at every this many barriers (and the last):
/// hashing guest RAM at every barrier would dominate the test.
const STATE: usize = 40;

/// Replaces the fixture init's "booted" line: two loops that fold the clock
/// into a value they print, with sleeps of different lengths between reads.
/// A leading 1 keeps the uptime digits out of octal.
const WORKLOAD: &str = r#"work() {
    h=$2
    i=0
    while [ $i -lt 120 ]; do
        read up idle </proc/uptime
        h=$(( (h * 131 + 1${up%.*}${up#*.} + i) % 1000000007 ))
        /bin/busybox sleep 0.00$3
        i=$((i + 1))
        if [ $((i % 40)) -eq 0 ]; then echo "C3_$1 $i $h"; fi
    done
    echo "C3_$1_DONE $h"
}
work CHILD 7 7 &
work PARENT 3 9 &
wait
echo 'seedbox: Linux userspace booted'"#;

fn input(key: &str) -> PathBuf {
    std::env::var_os(key)
        .unwrap_or_else(|| panic!("{key}"))
        .into()
}

fn machine() -> TcgMachine {
    let linux = input("SEEDBOX_TCG_LINUX");
    let mut machine = TcgMachine::new_raw(
        &input("SEEDBOX_TCG_QEMU"),
        Manifest {
            memory_mib: 128,
            disk: None,
            network: None,
            boot: Boot::Linux {
                kernel: fs::read(linux.join("kernel")).unwrap().into(),
                initramfs: lifecycle_initramfs(
                    &fs::read(linux.join("initramfs")).unwrap(),
                    WORKLOAD,
                )
                .into(),
                firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap().into(),
                command_line: "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr"
                    .into(),
                epoch_seconds: 0,
            },
        },
    )
    .unwrap();
    machine
        .configure_policy(
            "m",
            &BackendPolicy {
                seed: 42,
                timer_jitter_ns: 17,
                stop_tolerance_ns: 0,
                torn_writes: false,
                full_state: true,
                seeded_pv_speed: false,
                boot_end_evidence: false,
                controller: None,
            },
        )
        .unwrap();
    machine
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU), SEEDBOX_TCG_LINUX and SEEDBOX_TCG_FIRMWARE"]
fn native_stops_that_deliver_nothing_leave_no_trace() {
    stop_scenario::stops_leave_no_trace(
        &stop_scenario::Backend {
            name: "TCG",
            hang: std::time::Duration::from_secs(3600),
            machine: &machine,
            console: &|machine: &mut TcgMachine| machine.console().unwrap(),
            state_every: STATE,
            execution: &|_| ExecutionConfig::default(),
        },
        &[1, 2, 0x5eed],
    );
}
