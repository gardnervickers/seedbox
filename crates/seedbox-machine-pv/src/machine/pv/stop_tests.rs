//! Rule C3 on a native PV machine: a stop that delivers nothing leaves no
//! trace. The PV Linux kernel runs a C guest that charges syscalls, reads the
//! clock and sleeps in two processes. The shared scenario
//! (`seedbox_machine::test_support::stop_scenario`) drives it with the
//! machine's own stops and host barriers every half millisecond, and again
//! with the same barriers and runs that end early at seeded times, a few
//! microseconds apart, and nanoseconds apart from boot and after each wake,
//! where the guest's clock starts and where it goes idle. At every barrier
//! the native witness, the console and the full state evidence (registers,
//! devices, RAM and the shared page) must agree.
use super::*;
use seedbox_machine::{execution::ExecutionConfig, test_support::stop_scenario};

fn machine() -> PVMachine {
    let mut machine = PVMachine::from_boot_inputs(
        &guest::input("SEEDBOX_PV_WORKER"),
        fs::read(guest::input("SEEDBOX_PV_KERNEL")).unwrap(),
        guest::initrd_source(
            fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../seedbox-machine/tests/fixtures/pv-stop-invariance.c"),
            )
            .unwrap(),
        ),
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
#[ignore = "requires a signed native PV worker, the PV Linux kernel and SEEDBOX_PV_CLANG/SEEDBOX_PV_LD"]
fn native_stops_that_deliver_nothing_leave_no_trace() {
    stop_scenario::stops_leave_no_trace(
        &stop_scenario::Backend {
            name: "PV",
            hang: std::time::Duration::from_secs(1200),
            machine: &machine,
            console: &|machine: &mut PVMachine| machine.console.clone(),
            state_every: 1,
            execution: &|_| ExecutionConfig::default(),
        },
        &[1, 2, 0x5eed],
    );
}
