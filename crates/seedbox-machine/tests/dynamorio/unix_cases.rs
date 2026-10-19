//! Source-derived Linux process and signal regressions. Inventory IDs and exact
//! provenance live beside the application sources in unix/inventory.tsv.
#[path = "unix/runner.rs"]
mod runner;
use runner::{checked, triplet, Profile};

#[test]
fn source_signal_context_and_lifecycle_properties() {
    for (case, fields, images) in [
        ("siglongjmp", vec!["caught 20000"], 1),
        ("ucontext", vec!["x28Changed 0"], 1),
        ("pthreadcancel", vec!["canceled 200", "cleaned 200"], 1),
        ("seccomp", vec!["emulated 1"], 2),
        (
            "autodisarm",
            vec!["disarmedInHandler 1000", "stillAutoDisarm 1"],
            1,
        ),
        (
            "autodisarmctx",
            vec![
                "first 2000/2000 onA 2000 disarmed 2000 savedOk 2000 resumed 2000 savedOkAfter 2000 canaryBad 0",
                "second 2000/2000 threadStack 1000 onA 0 onB 1000 flagsOk 2000",
                "restoredA 2000/2000",
            ],
            1,
        ),
        ("signalfd", vec!["sum 1279200", "inOrder 1"], 1),
        ("pselect", vec!["eintr 500", "handled 500"], 1),
        ("sigsuspend", vec!["eintr 500", "handled 500"], 1),
        ("timerfd", vec!["readsWithExpiry 20"], 1),
        ("pidfd", vec!["ok 20"], 21),
        ("clone3", vec!["ok 20"], 21),
        ("memfdexec", vec!["ok 10"], 21),
        ("wipeonfork", vec!["wiped 10"], 11),
        (
            "unwind",
            vec!["depthOk 200", "sameOutermost 200", "signalOk 50"],
            1,
        ),
        (
            "tlsdtor",
            vec![
                "return cabaaa",
                "pthreadExit cabaaa",
                "cancel cabaaa",
                "same 90/90",
            ],
            1,
        ),
        (
            "execsig",
            vec![
                "disp DDDIID alt disabled",
                "afterAck B value 77 fromParent 1 queue",
                "acked 1",
                "exit 0",
            ],
            3,
        ),
        (
            "execsigrace",
            vec![
                "raced queued 64/64 inOrder 1",
                "afterAck B value 77 fromParent 1 queue",
                "exit 0",
            ],
            3,
        ),
    ] {
        triplet("properties", &[case], images, &fields);
    }
}
#[test]
fn source_temporary_masks_and_pending_queues() {
    for mode in [
        "sigsuspend",
        "sigsuspend-badarg",
        "ppoll",
        "ppoll-badarg",
        "ppoll-ready",
        "pselect6",
        "pselect6-badarg",
        "pselect6-ready",
        "epoll_pwait",
        "epoll_pwait-badarg",
        "epoll_pwait-ready",
        "epoll_pwait2",
        "epoll_pwait2-badarg",
        "epoll_pwait2-ready",
    ] {
        triplet("tempmask_pending", &[mode], 1, &["OK"]);
    }
    triplet(
        "pending_overflow",
        &["16"],
        1,
        &["accepted 16 delivered 16 inOrder 1", "OK"],
    );
    for n in ["254", "255", "256", "257"] {
        triplet(
            "pending_fork_signalfd",
            &["fork", n],
            2,
            &["othersTags 0", "OK"],
        );
    }
    for n in ["257", "600"] {
        triplet(
            "pending_fork_signalfd",
            &["signalfd", n],
            1,
            &["inOrder 1 thenEagain 1", "OK"],
        );
    }
    triplet(
        "queues",
        &["rtfifo"],
        1,
        &[
            "delivered 640/640",
            "duplicates 0",
            "foreign 0",
            "fifoPerSignal 1",
            "blockedLowestFirst 20/20",
            "flushStale 0",
            "flushRan 1",
        ],
    );
    triplet(
        "queues",
        &["sigpressure"],
        6,
        &[
            "lost 0 duplicated 0 unaccepted 0 foreign 0 someAccepted 1",
            "thenEagain 1",
        ],
    );
    triplet("queues", &["tmpmask"], 1, &["same 280/280"]);
}
#[test]
fn source_wait_signals_and_timers() {
    for mode in ["pending", "arrive", "afterEintr"] {
        triplet("sigtimedwait_queued", &[mode], 1, &["OK"]);
    }
    for mode in ["before", "during", "flush"] {
        triplet("sigtimedwait", &[mode], 1, &["got 3/3", "OK"]);
    }
    for source in ["timer", "kill"] {
        for mode in ["self", "thread", "handler"] {
            triplet("timer_sigwait", &[source, mode], 1, &["OK"]);
        }
    }
    for mode in ["plain", "thread", "helper", "masked"] {
        triplet(
            "timer_signal_loss",
            &[mode],
            1,
            &["round 4 missing 0", "OK"],
        );
    }
    for signal in ["ILL", "STKFLT", "USR2", "USR1", "PROF"] {
        for mode in ["flush", "noflush"] {
            triplet(
                "suspend_signal_sharing",
                &[signal, "2000", mode],
                1,
                &["handled 2000/2000 inOrder 2000 unacked 0", "OK"],
            );
        }
    }
}
#[test]
fn source_non_sleep_flush_and_stop_continue_waits() {
    for mode in ["read", "ppoll", "pselect", "epoll", "nanosleep"] {
        triplet(
            "flush_eintr",
            &[mode, "20"],
            1,
            &["eintr 0", "completed 20/20", "OK"],
        );
        let out = checked("flush_eintr", &[mode, "20"], Profile::Client, 1);
        assert_eq!(
            out.stderr
                .lines()
                .filter(|s| s.starts_with("unix-client: suspended="))
                .count(),
            20
        );
    }
    for mode in ["read", "ppoll", "pselect", "epoll", "futex"] {
        triplet("waits", &[mode], 2, &["stopped=1, resumed", "payload=1"]);
        let out = checked("waits", &[mode], Profile::Client, 2);
        assert!(out.stderr.contains(" resumed=1 flushed=1"));
    }
}
#[test]
fn relative_futex_timeout_snapshot_after_kernel_entry() {
    // Additional shared-helper test. Original nanosleep alias/remainder and
    // successful deadline semantics remain unresolved; this does not replace them.
    triplet(
        "waits",
        &["snapshot"],
        2,
        &["stopped=0, resumed, completed=0, payload=1"],
    );
    let out = checked("waits", &["snapshot"], Profile::Client, 2);
    assert!(out.stderr.contains(" resumed=1 flushed=1"));
}
#[test]
fn source_exec_descriptors_raw_clone_and_failure_rollback() {
    triplet("exec_sigign", &[], 2, &["IIID (expected IIID) OK"]);
    for mode in ["range", "cloexec"] {
        triplet(
            "close_range_all",
            &[mode],
            3,
            &[
                "closed 0",
                "open 3",
                "thread ok",
                "code ok",
                "child 42",
                "OK",
            ],
        );
    }
    for mode in ["thread", "vm"] {
        triplet(
            "clone_mask",
            &[mode],
            if mode == "vm" { 2 } else { 1 },
            &["clone ok", "mask blocked", "OK"],
        );
    }
    for mode in ["vfork", "clone", "spawn"] {
        triplet("vfork_exit", &[mode], 1, &["OK"]);
    }
    triplet("sigsys_call_addr", &[], 1, &["pc ok si_call_addr ok", "OK"]);
    for mode in ["ign", "nocldwait", "nocldstop"] {
        triplet(
            "sigchld_flags",
            &[mode],
            if mode == "nocldstop" { 2 } else { 4 },
            &["OK"],
        );
    }
    for first in ["none", "vm", "thread", "vfork", "clone", "fork"] {
        for then in ["fork", "flush", "thread"] {
            triplet(
                "failed_clone_vm",
                &[first, then],
                if then == "fork" { 2 } else { 1 },
                &["OK"],
            );
        }
    }
    for mode in ["flush", "segv", "exit"] {
        for failed in [false, true] {
            let args = if failed {
                vec![mode, "fail"]
            } else {
                vec![mode]
            };
            triplet("clear_sighand_child", &args, 2, &["child exit 7", "OK"]);
        }
    }
}
#[test]
fn source_process_creation_under_jit_load() {
    for args in [
        vec!["200", "4", "spawn", "4", "20"],
        vec!["200", "4", "vfork", "4", "20"],
        vec!["200", "4", "fork", "4", "20"],
        vec!["60", "1", "fork", "1", "0"],
    ] {
        triplet(
            "spawnjit",
            &args,
            args[0].parse::<usize>().unwrap() + 1,
            &["OK"],
        );
    }
    for mode in ["blocking", "spin"] {
        let args = if mode == "spin" {
            vec!["20", "4", "1000", "spin"]
        } else {
            vec!["20", "4", "1000"]
        };
        triplet("vfork_flush", &args, 21, &["OK"]);
    }
    for args in [vec!["50", "4", "0"], vec!["50", "4", "16"]] {
        triplet("sigext", &args, 101, &["OK"]);
    }
}
#[test]
fn source_process_stress_populations() {
    // procexitcounts has nondeterministic *diagnostic* populations; validate
    // completed-work fields rather than comparing those host counts.
    for case in [
        "clone3fail",
        "execnonleader",
        "spawnattrs",
        "atfork",
        "fdhygiene",
        "condcancel",
        "procexit",
    ] {
        triplet(
            "process_stress",
            &[case, "1", "1"],
            1,
            &[&format!("OK {case}")],
        );
    }
    for profile in [Profile::Native, Profile::Plain, Profile::Client] {
        let out = checked("process_stress", &["procexitcounts", "1", "1"], profile, 1);
        assert!(out.stdout.contains("workBad 0") && out.stdout.contains("exitBad 0"));
        assert!(out.stdout.contains("reader 0 dirs ") && out.stdout.contains("scanner 4 dirs "));
    }
}
#[test]
fn real_public_attach_detach_with_live_handler_and_ordered_pipe() {
    let native = checked("lifecycle", &["native"], Profile::Native, 0);
    let attached = checked("lifecycle", &["attach"], Profile::Native, 0);
    assert_eq!(attached.stdout, native.stdout);
    assert!(attached
        .stdout
        .contains("12 transitions, 24 ordered operations, 12 live handlers"));
}
#[cfg(target_arch = "aarch64")]
#[test]
fn source_aarch64_signal_frames_vectors_rewrite_and_deep_nesting() {
    triplet(
        "signal_abi_aarch64",
        &["sigframe"],
        2,
        &["same 240/240", "fpsimd 1/528 term 1 bad 0", "nzcvOk 1"],
    );
    triplet(
        "signal_abi_aarch64",
        &["ctxrewrite"],
        1,
        &["captured 50/50 restored 50/50 maskApplied 50/50"],
    );
    triplet(
        "signal_abi_aarch64",
        &["nestsig"],
        2,
        &[
            "levels 25000/25000",
            "altLevels 2500/2500",
            "canaryBad 0",
            "pcBad 0 infoBad 0 spBad 0 maskBad 0 altBad 0 endBad 0",
            "onstackFlagBad 0 changeRefused 250/250",
        ],
    );
    for mode in ["base", "x28", "ibl"] {
        triplet("sigregs", &["4", "2000", "20000", mode], 1, &["OK"]);
    }
    triplet("srsample", &["10000", "2"], 1, &["OK"]);
}

#[test]
fn source_sleep_syscalls_are_interrupted_after_qualified_kernel_entry() {
    for subject in ["relative", "absolute", "alias"] {
        for mode in ["plain", "restart", "nested"] {
            triplet(
                "sleep_subject",
                &[subject, mode],
                1,
                &["EINTR, frame, mask,", "remainder/deadline-input valid"],
            );
            let out = checked("sleep_subject", &[subject, mode], Profile::Client, 1);
            assert!(out.stderr.contains(" resumed=1 flushed=1"));
        }
    }
}

#[test]
fn public_application_and_client_itimers_preserve_original_order_and_sampling() {
    let native = checked("timer_order", &["app"], Profile::Native, 0);
    let plain = checked("timer_order", &["app"], Profile::Plain, 0);
    let client = checked("timer_order", &["client"], Profile::Client, 1);
    assert_eq!(plain.stdout, native.stdout);
    assert_eq!(client.stdout, native.stdout);
}
#[test]
fn source_stop_continue_actual_sleep_completion_has_no_eintr() {
    for mode in ["relative", "absolute"] {
        triplet("stop_sleep", &[mode], 2, &["stopped, continued, result 0"]);
    }
}

#[test]
fn source_static_pie_self_relocation_and_same_address_across_exec() {
    for case in ["staticpie", "staticpieaddr"] {
        for profile in [Profile::Native, Profile::Plain, Profile::Client] {
            let out = checked("properties", &[case], profile, 10);
            for field in [
                "relocated 1 value 42 func 7 noRandomize 1 interp 0 atBase 0 etDyn 1 phdrInImage 1",
                "execs 6",
                "sameAddress 3",
                "statusOk 3",
            ] {
                assert!(
                    out.stdout.contains(field),
                    "missing {field}: {}",
                    out.stdout
                );
            }
        }
    }
}
#[test]
fn source_safespot_signals_concurrent_jit_and_mapping_lifetimes() {
    triplet(
        "safespot_storm",
        &[],
        1,
        &["2000 signals acknowledged, two writers, two executors, 1000 mappings"],
    );
}

#[test]
fn source_posix_timer_targets_replacement_and_overrun_populations() {
    for case in ["timerstorm", "timerstormcounts"] {
        for profile in [Profile::Native, Profile::Plain, Profile::Client] {
            let out = checked("properties", &[case], profile, 1);
            for field in [
                "created 2000/2000 armed 2000/2000",
                "complete 10/10 once 2000/2000 missing 0 duplicates 0",
                "wrongThread 0 codeBad 0 readEintr 0",
                "replaced 5/5 deadTidEinval 5/5",
                "created 1 delivered 1 overrunPositive 1",
            ] {
                assert!(
                    out.stdout.contains(field),
                    "{case} missing {field}: {}",
                    out.stdout
                );
            }
            // Raw overrun/delivery counts are reported evidence, not a timing
            // gate or equality oracle across independently scheduled hosts.
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn shared_deep_signal_nesting_checks_native_x86_register_vector_and_flags_abi() {
    triplet(
        "nesting_x86",
        &[],
        2,
        &[
            "levels 25000/25000",
            "altLevels 2500/2500",
            "canaryBad 0",
            "pcBad 0 infoBad 0 spBad 0 maskBad 0 altBad 0 endBad 0",
            "onstackFlagBad 0 changeRefused 250/250",
        ],
    );
}

#[test]
fn source_masked_waiters_and_process_directed_stop_continue_signal_counts() {
    for mode in ["stop", "usr1", "both"] {
        for profile in [Profile::Native, Profile::Plain, Profile::Client] {
            let out = checked("stopcont_signals", &[mode, "100", "4", "2"], profile, 2);
            assert!(
                out.stdout.contains("(lost 0)")
                    && out.stdout.contains("EINTR 0, exit 0")
                    && out.stdout.ends_with("OK\n"),
                "{}",
                out.stdout
            );
        }
    }
}
#[test]
fn source_competing_fork_callers_and_reused_clone_stack_lifetimes() {
    triplet(
        "process_stack",
        &["forkcallers"],
        121,
        &[
            "records 30",
            "recordBad 0",
            "childJitBad 0",
            "executorBad 0",
            "writerBad 0",
        ],
    );
    triplet(
        "process_stack",
        &["clonestack"],
        327,
        &[
            "bad cookie 0 arg 0 mask 0 compute 0 stack 0 tls 0 jit 0 exit 0",
            "publisherBad 0",
        ],
    );
}

#[test]
fn source_async_io_signal_ownership_moves_and_reused_descriptors() {
    triplet(
        "properties",
        &["sigio"],
        1,
        &[
            "pipeW0 25/25 w0 fdOk 1",
            "sockW1 25/25 w1 fdOk 1",
            "pipeMovedW2 25/25 w2 fdOk 1",
            "pipeProcess 25/25 w2 fdOk 1",
            "pipeReusedW1 25/25 w1 fdOk 1",
            "sockReusedNoAsync 25/25 none",
            "owners 150/150",
            "numbersReused 25/25",
        ],
    );
}
#[cfg(target_arch = "aarch64")]
#[test]
fn source_shared_runtime_signal_numbers_interleave_with_genuine_faults() {
    triplet(
        "signal_abi_aarch64",
        &["sigshare"],
        1,
        &[
            "5000/5000 inOrder 5000",
            "unacked 0",
            "foreign 0",
            "genuineIll 200/200",
            "genuineTrap 200/200",
            "flushStale 0",
        ],
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn original_aarch64_generated_signal_abi_and_public_spill_paths() {
    for mode in ["abi", "paths"] {
        let mut kinds = vec![
            "udf", "brk", "segv", "self", "ext", "nest", "samask", "mask", "alt", "rt", "pipe",
        ];
        if mode == "paths" {
            kinds.push("ispin");
        }
        for kind in kinds {
            let native = runner::run("signal_model", &[mode, kind], Profile::Native, None);
            assert_eq!(
                native.code, 0,
                "native model {mode}/{kind}: {}",
                native.stderr
            );
            let variants: &[(&[&str], Option<&[&str]>)] = if mode == "abi" {
                &[
                    (&[], None),
                    (&["-max_bb_instrs", "3"], None),
                    (&["-disable_traces"], None),
                ]
            } else {
                &[
                    (&[], Some(&[])),
                    (&[], Some(&["all"])),
                    (&["-disable_traces"], Some(&[])),
                ]
            };
            for (options, path_args) in variants {
                let profile = if path_args.is_some() {
                    Profile::Client
                } else {
                    Profile::Plain
                };
                let client = path_args.map(|args| ("libunix-signal-paths.so", args));
                let out = runner::run_with_options(
                    "signal_model",
                    &[mode, kind],
                    profile,
                    None,
                    options,
                    client,
                );
                assert_eq!(out.code, 0, "model {mode}/{kind}: {}", out.stderr);
                assert_eq!(
                    out.stdout, native.stdout,
                    "model {mode}/{kind}: {}",
                    out.stderr
                );
                if path_args.is_some() {
                    assert!(out.stderr.contains("unix-paths: instructions="));
                }
            }
        }
    }
}

#[test]
fn original_indirect_calls_complete_250_async_signals_through_clean_calls() {
    let native = checked("cleancallsig", &[], Profile::Native, 0);
    let plain = checked("cleancallsig", &[], Profile::Plain, 0);
    let client = runner::run_with_options(
        "cleancallsig",
        &[],
        Profile::Client,
        None,
        &[],
        Some(("libunix-cleancallsig.so", &[])),
    );
    assert_eq!(client.code, 0, "{}", client.stderr);
    assert_eq!(native.stdout, plain.stdout);
    assert_eq!(native.stdout, client.stdout);
    assert!(client
        .stderr
        .contains("unix-cleancallsig: app-clean-calls="));
    assert!(native.stdout.contains("acknowledgements 250"));
}

#[test]
fn source_executable_alias_values_survive_mapping_lifetimes_and_signal_return() {
    // Default HWCC profile only. Source noHWCC is a distinct A64 cache-publication
    // profile; its debug-runtime dependency is explicitly retained in inventory.
    for case in [
        "boundary", "closed", "late", "protect", "offset", "reuse", "remap", "replace", "stable",
        "private", "mixed", "process", "signal",
    ] {
        triplet(
            "aliases",
            &[case],
            if case == "process" { 2 } else { 1 },
            &[
                "updates 5000 slots 64 stride 16 sum 87487500 stale 0",
                if case == "signal" {
                    "traps 5000"
                } else {
                    "traps 0"
                },
            ],
        );
    }
}

#[test]
fn alias_publication_during_branch_translation_preserves_post_join_values() {
    let native = runner::run("alias_build", &[], Profile::Native, None);
    let plain = runner::run("alias_build", &[], Profile::Plain, None);
    let controlled = runner::run_with_options(
        "alias_build",
        &[],
        Profile::Client,
        None,
        &[],
        Some(("libunix-alias-build.so", &[])),
    );
    for out in [&native, &plain, &controlled] {
        assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
        assert_eq!(
            out.stdout,
            "alias-build: publisher joined 1 calls 1000 expected 22 stale 0\n"
        );
    }
    assert!(controlled
        .stderr
        .contains("process publication acknowledged after decode"));
    // The first call can legally execute either snapshot. Its result is only
    // diagnostic; the1000 calls after publication AND waitpid are the oracle.
}

fn check_asynchronous_alias_context(mode: &str) {
    let native = checked("alias_async", &[mode], Profile::Native, 0);
    let plain = checked("alias_async", &[mode], Profile::Plain, 0);
    let client = checked("alias_async", &[mode], Profile::Client, 1);
    assert_eq!(native.stdout, plain.stdout);
    assert_eq!(native.stdout, client.stdout);
    assert!(native
        .stdout
        .contains("signals 64 GPR vector flags PC mask sigreturn progress preserved"));
    let receipt = match mode {
        "both" => Some("resumed=1 flushed=1 alias-pc=1"),
        "suspend" => Some("suspend-only=1 alias-pc=1"),
        "flush" => Some("flush-only=1"),
        _ => None,
    };
    if let Some(receipt) = receipt {
        assert_eq!(
            client
                .stderr
                .lines()
                .filter(|line| line.contains(receipt))
                .count(),
            64
        );
    }
}
#[test]
fn asynchronous_alias_context_survives_public_flush_and_sigreturn() {
    check_asynchronous_alias_context("both");
}
#[test]
fn asynchronous_alias_signal_control_preserves_context() {
    check_asynchronous_alias_context("signal");
}
#[test]
fn asynchronous_alias_suspend_control_preserves_context() {
    check_asynchronous_alias_context("suspend");
}
#[test]
fn asynchronous_alias_flush_control_preserves_context() {
    check_asynchronous_alias_context("flush");
}

#[cfg(target_arch = "aarch64")]
#[test]
fn original_generated_lifecycle_state_and_fault_oracle_spans_60_attach_rounds() {
    // Full original generator/state oracle; each worker admits3 blocks using
    // two identical native executions before any DR attach. Debug-only nocas /
    // steal27 remain explicitly pending a matching ARM debug SDK.
    for variant in ["native", "default", "notraces"] {
        let out = checked("lifecycle_model", &[variant], Profile::Native, 0);
        assert!(out.stdout.contains("rounds60 blocks3/worker workers"));
        assert!(out
            .stdout
            .contains("operations240 process131072 full state/fault comparisons"));
        if variant != "native" {
            assert!(out.stderr.contains("probes60"));
        }
    }
}

#[test]
fn original_detach_signal_masks_and_altstack_survive_live_handler() {
    for variant in ["native", "attach"] {
        for location in ["ordinary", "handler"] {
            let out = checked("detach_signal", &[variant, location], Profile::Native, 0);
            assert!(out
                .stdout
                .contains("masks altstack live-handler frame sigreturn preserved"));
        }
    }
}

#[test]
fn original_restart_table_completes_all_17_selectors_and_three_signal_modes() {
    // Original source table, not runtime registration. No selector is silently
    // absent: a missing/refused kernel API produces a named setup failure.
    for selector in [
        "read",
        "write",
        "nanosleep",
        "clockNanosleepAbs",
        "futexWait",
        "ppoll",
        "epollPwait2",
        "accept4",
        "connect",
        "recvmsg",
        "wait4",
        "flock",
        "sigtimedwait",
        "semop",
        "ioUringEnter",
        "futexWaitv",
        "sigsuspendPause",
    ] {
        for mode in ["0", "1", "2"] {
            triplet(
                "restart_matrix",
                &[selector, mode],
                if selector == "wait4" { 2 } else { 1 },
                &["completed payload frame nested blocked-entry"],
            );
        }
    }
}

#[path = "unix/signal_models_cases.rs"]
mod signal_models_cases;

#[cfg(target_arch = "aarch64")]
#[test]
fn original_generated_lifecycle_debug_options_use_actual_debug_core() {
    for variant in ["nocas", "steal27"] {
        let out = checked("lifecycle_model", &[variant], Profile::DebugNative, 0);
        assert!(out.stdout.contains("rounds60 blocks3/worker workers"));
        assert!(out.stderr.contains("profile debug confirmed60"));
    }
}

#[test]
fn source_process_cpu_timer_delivers_20_ticks_through_public_client() {
    triplet("properties", &["cputimer"], 1, &["ticksAtLeast 1"]);
}

#[test]
fn original_sigstop_concurrent_jit_compute_and_four_wait_records() {
    triplet(
        "sigstop_concurrent",
        &[],
        2,
        &[
            "workers8 stopped200 continued200 usr1=50/50 JITstale0 computeBad0 timed4 EINTR0 wrong0 early=PENDING",
        ],
    );
}

#[test]
fn original_detach_context_register_vector_flags_and_stack_matrix() {
    for case in [
        "gprs_cache",
        "gprs_selfmod1",
        "gprs_selfmod2",
        "flags_cache",
        "flags_selfmod",
        "sp_cache",
        "sp_selfmod",
    ] {
        for mode in ["native", "attach"] {
            let out = checked("detach_context", &[mode, case], Profile::Native, 0);
            assert!(out
                .stdout
                .contains("source register vector status stack assertions preserved"));
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn copied_x86_relative_operands_and_raw_roundtrips_keep_original_targets() {
    let native = checked("rip-api-control", &[], Profile::Native, 0);
    let plain = checked("rip-api-control", &[], Profile::Plain, 0);
    let client = runner::run_with_options(
        "rip-api-control",
        &[],
        Profile::Client,
        None,
        &[],
        Some(("libunix-rip-api.so", &[])),
    );
    assert_eq!(client.code, 0, "{}", client.stderr);
    assert_eq!(native.stdout, plain.stdout);
    assert_eq!(native.stdout, client.stdout);
    assert_eq!(native.stdout, "OK public client control app\n");
    assert!(client
        .stderr
        .contains("unix-rip-api: cases4 decoded4 raw4 roundtrips8 errors0"));
}
#[cfg(target_arch = "x86_64")]
#[test]
fn source_alias_literal_load_keeps_5000_updates_relative_to_application_pc() {
    triplet(
        "aliases",
        &["rip-literal"],
        1,
        &["updates 5000 slots 64 stride 16 sum 87487500 stale 0 traps 0"],
    );
}
#[path = "unix/fault_context_cases.rs"]
mod fault_context_cases;

#[path = "unix/fd_limits_cases.rs"]
mod fd_limits_cases;

#[cfg(target_arch = "aarch64")]
#[path = "unix/cache_publication/cases.rs"]
mod cache_publication_cases;
#[path = "unix/timer_state/cases.rs"]
mod timer_state_cases;

#[test]
fn source_rlimit_kernel_visibility_and_fork_exec() {
    triplet(
        "rlimit_nofile",
        &[],
        2,
        &[
            "exec'd limit 2048",
            "limits kernel-visible set/get/prlimit fork/exec preserved",
            "OK",
        ],
    );
}

#[path = "unix/shared_signal_handoff/cases.rs"]
mod shared_signal_handoff_cases;
