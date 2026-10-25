//! Public clock faults checked against real clock-faults.c timer/clock output.
//! Shares the real service, public admission and replay helpers.
use super::*;
use seedbox_api::{ControlCommand, FaultAction, InputSource, PlaybackRate};
const SEED: u64 = 123;
const TARGET: &str = "target";

const CLOCK_HANDLE: &str = "public-clock";
const SECOND: i128 = 1_000_000_000;
// Native clock-fault acceptance uses the same 1 ms timestamp allowance as the
// existing Driver clock-fault tests; this is simulated, not host elapsed time.
const CLOCK_LATENCY: i128 = 1_000_000;

#[derive(Clone, Copy)]
enum ClockCase {
    Skew,
    Drift,
}
impl ClockCase {
    fn name(self) -> &'static str {
        match self {
            Self::Skew => "skew",
            Self::Drift => "drift",
        }
    }
    fn action(self) -> FaultAction {
        match self {
            Self::Skew => FaultAction::ClockSkew {
                handle: CLOCK_HANDLE.into(),
                machine: TARGET.into(),
                offset_ns: Some(2_000_000_000),
                max_offset_ns: None,
            },
            Self::Drift => FaultAction::ClockDrift {
                handle: CLOCK_HANDLE.into(),
                machine: TARGET.into(),
                rate_ppb: Some(100_000_000),
                max_rate_ppb: None,
            },
        }
    }
}

fn clock_request(session: &NativeSession) -> SubmitRequest {
    let key = match session.backend {
        CpuBackend::Pv => "SEEDBOX_PV_CLOCK_APPLICATION",
        CpuBackend::Tcg => "SEEDBOX_TCG_CLOCK_APPLICATION",
    };
    let binary =
        fs::read(std::env::var_os(key).unwrap_or_else(|| panic!("missing {key}"))).unwrap();
    let filesystem = application_tar(&session.root.path().join("clock-application.tar"), &binary);
    let mut request = fixture_submission(
        session.backend,
        session.root.path(),
        &session.store,
        SEED,
        NativeFixture {
            name: "oracle",
            filesystem,
            launch: GuestLaunch {
                argv: vec!["/application".into(), "idle".into()],
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
            control: false,
            request_id: "clock-fault".into(),
            extra_machines: vec![TARGET],
            volumes: vec![],
        },
    );
    let InputSource::Resolved { definition } = &request.spec.inputs else {
        panic!("resolved real clock fixture")
    };
    let mut definition = definition.as_ref().clone();
    // Paused Step acknowledgements pass through the service control poll.
    // These NIC-free machines can advance in 100 ms windows while guest timer
    // deadlines still shorten runs and output keeps its actual occurrence time.
    // Keep the workload and 1 ms clock-accuracy assertions unchanged.
    definition.simulator.quantum_ns = std::num::NonZeroU64::new(100_000_000).unwrap();
    definition.machines.get_mut(TARGET).unwrap().process.argv =
        vec!["/application".into(), "clock".into()];
    request.spec = SimulationSpec::resolved(definition).unwrap();
    request.options.start_paused = true;
    request
}

#[derive(Debug)]
struct Reading {
    tag: String,
    at: u64,
    rt: i128,
    mono: i128,
    raw: i128,
    cnt: i128,
}
fn readings(output: &str, times: &[(usize, u64)]) -> Vec<Reading> {
    assert!(output.is_ascii() && output.ends_with('\n'), "{output}");
    let mut end = 0;
    output
        .split_inclusive('\n')
        .map(|line| {
            end += line.len();
            let at = times.iter().find(|(offset, _)| *offset >= end).unwrap().1;
            let words: Vec<_> = line.split_whitespace().collect();
            assert_eq!(words.len(), 5, "unexpected native clock output: {line}");
            let number =
                |index: usize, prefix| words[index].strip_prefix(prefix).unwrap().parse().unwrap();
            Reading {
                tag: words[0].into(),
                at,
                rt: number(1, "rt="),
                mono: number(2, "mono="),
                raw: number(3, "raw="),
                cnt: number(4, "cnt="),
            }
        })
        .collect()
}
fn tagged<'a>(lines: &'a [Reading], tag: &str) -> Vec<&'a Reading> {
    lines.iter().filter(|line| line.tag == tag).collect()
}
fn assert_clocks(case: ClockCase, output: &str, times: &[(usize, u64)], healed: u64) {
    let lines = readings(output, times);
    assert_eq!(tagged(&lines, "BOOT").len(), 1, "{output}");
    let boot = &lines[0];
    assert_eq!(boot.tag, "BOOT");
    for pair in lines.windows(2) {
        assert!(
            pair[1].mono >= pair[0].mono
                && pair[1].raw >= pair[0].raw
                && pair[1].cnt >= pair[0].cnt,
            "guest monotonic clocks went backwards: {pair:?}"
        );
    }
    let slept = tagged(&lines, "SLEPT");
    assert_eq!(slept.len(), 2, "{output}");
    let sleep_mono = slept[1].mono - slept[0].mono;
    let sleep_cnt = slept[1].cnt - slept[0].cnt;
    assert!(sleep_mono >= SECOND);
    assert!((sleep_cnt - sleep_mono).abs() < CLOCK_LATENCY);
    assert!((slept[1].raw - slept[0].raw - sleep_cnt).abs() < CLOCK_LATENCY);
    let fired = tagged(&lines, "FIRED");
    assert_eq!(
        fired.len(),
        1,
        "absolute realtime timer must fire once: {output}"
    );
    assert!(
        fired[0].rt >= boot.rt + 5 * SECOND,
        "timer fired early: {output}"
    );
    let canceled = tagged(&lines, "CANCELED");
    match case {
        ClockCase::Skew => {
            assert_eq!(
                canceled.len(),
                1,
                "CANCEL_ON_SET must observe realtime step: {output}"
            );
            let jump = (canceled[0].rt - canceled[0].mono) - (boot.rt - boot.mono);
            assert!(
                (jump - 2 * SECOND).abs() < CLOCK_LATENCY,
                "realtime-only +2s step: {output}"
            );
            let elapsed = fired[0].cnt - boot.cnt;
            assert!((elapsed - 3 * SECOND).abs() < CLOCK_LATENCY,
                "forward realtime step must advance the absolute timer, not monotonic time: {output}");
            let last = lines.last().unwrap();
            assert!(last.at > healed, "missing post-heal guest output");
            assert!(
                ((last.rt - last.mono) - (boot.rt - boot.mono)).abs() < CLOCK_LATENCY,
                "heal must undo the realtime offset: {output}"
            );
        }
        ClockCase::Drift => {
            assert!(
                canceled.is_empty(),
                "rate change must not cancel a realtime timer: {output}"
            );
            let simulated = i128::from(slept[1].at - slept[0].at);
            assert!(
                (simulated * 11 / 10 - sleep_cnt).abs() < CLOCK_LATENCY,
                "guest one-second sleep must use +10% native clock mapping: {output}"
            );
            let post: Vec<_> = lines
                .iter()
                .filter(|line| line.tag == "TICK" && line.at > healed + 500_000_000)
                .collect();
            assert!(
                post.len() >= 2,
                "missing post-heal native timer samples: {output}"
            );
            let simulated = i128::from(post[1].at - post[0].at);
            assert!(
                (post[1].cnt - post[0].cnt - simulated).abs() < CLOCK_LATENCY,
                "heal must restore unit rate: {output}"
            );
            // Two complete guest seconds at 1.1x already accrue at least
            // 2/11 of a simulated second. Healing must retain that advance.
            assert!(
                post[1].cnt - i128::from(post[1].at) > 2 * SECOND / 11 - CLOCK_LATENCY,
                "heal discarded accrued native clock time: {output}"
            );
            for line in &lines {
                assert!(
                    ((line.rt - line.mono) - (boot.rt - boot.mono)).abs() < CLOCK_LATENCY,
                    "drift/heal must not introduce a realtime step: {line:?}"
                );
            }
        }
    }
}

async fn clock_case(backend: CpuBackend, case: ClockCase) {
    native_case(move |owner| Box::pin(async move {
        let session = owner.session(backend);
        // Existing backend boot allowance plus idle role's exact 12.5s guest
        // sleep. A clock/timer bug cannot survive until a host-time deadline.
        session.bound = simulated_bound(backend) + 12_500_000_000;
        let source = session.submit(clock_request(session), 1).await;
        session.worker(&source).await;
        session.control(&source, "clock-initial-pause", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        until_output(session, &source, match case {
            ClockCase::Skew => "SLEPT", // positively proves both timerfds were armed
            ClockCase::Drift => "BOOT",
        }).await;
        let fault = inject(session, &source, "clock-apply", case.action()).await;
        step(session, &source).await;
        applied_once(session, &source, &[fault]).await;
        until_output(session, &source, match case {
            ClockCase::Skew => "FIRED", // observe changed timer before undoing step
            ClockCase::Drift => "TICK", // two drifted sleeps and a pending periodic wake
        }).await;
        let heal = inject(session, &source, "clock-heal", FaultAction::Heal { handle: CLOCK_HANDLE.into() }).await;
        step(session, &source).await;
        applied_once(session, &source, &[fault, heal]).await;
        let healed = session.client.playback(&source).await.unwrap().state.position.simulated_time_ns;
        session.control(&source, "clock-finish", ControlCommand::SetRate { rate: PlaybackRate::Unlimited }).await;
        let witness = session.complete(&source, false, 1, SEED).await.unwrap();
        let (output, times) = session.stdout_timed(&source, TARGET, false).await;
        assert_clocks(case, &output, &times, healed);
        let resource = session.client.describe(&source).await.unwrap();
        assert_eq!(resource.spec.program.len(), 2);
        let replay = session.submit(SubmitRequest {
            fault_profile: None,
            metadata: Default::default(), options: Default::default(),
            request_id: "clock-replay".into(), spec: resource.spec,
        }, 1).await;
        session.worker(&replay).await;
        let replay_witness = session.complete(&replay, false, 1, SEED).await.unwrap();
        let (replay_output, replay_times) = session.stdout_timed(&replay, TARGET, false).await;
        assert_eq!(output, replay_output, "real clock payload changed on retained replay");
        assert_eq!(times, replay_times, "clock output simulation times changed on replay");
        assert_clocks(case, &replay_output, &replay_times, healed);
        applied_once(session, &replay, &[fault, heal]).await;
        assert_eq!(witness, replay_witness);
        let comparison = session.client.compare(&source, &replay).await.unwrap();
        assert_eq!(comparison.status, ComparisonStatus::Match);
        assert_eq!(comparison.witness_matches, Some(true));
        eprintln!("public {} clock-{}/heal native replay MATCH source={source} replay={replay} witness={witness}", label(backend), case.name());
    })).await;
}

#[cfg(feature = "kvm-tests")]
pub async fn pv_clock_skew_heal_replay() {
    clock_case(CpuBackend::Pv, ClockCase::Skew).await;
}
#[cfg(feature = "kvm-tests")]
pub async fn pv_clock_drift_heal_replay() {
    clock_case(CpuBackend::Pv, ClockCase::Drift).await;
}
#[cfg(feature = "tcg-tests")]
pub async fn tcg_clock_skew_heal_replay() {
    clock_case(CpuBackend::Tcg, ClockCase::Skew).await;
}
#[cfg(feature = "tcg-tests")]
pub async fn tcg_clock_drift_heal_replay() {
    clock_case(CpuBackend::Tcg, ClockCase::Drift).await;
}
