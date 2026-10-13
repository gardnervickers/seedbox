//! Native common preparation: image metadata, retained launch and private wrapper.
use super::machine_crate;
use machine_crate::machine::{ExitReason, Machine, RunContext, StopReason, Timestamp};
use machine_crate::{
    clock::Clock,
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    pv::{ApplicationCommand, CommandResult, Inbox, Input, Outbox, Request, Response, Stream},
    rng::RngStream,
};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Debug,
    num::NonZeroU32,
    path::PathBuf,
    time::{Duration, Instant},
};
struct SimulationClock(std::sync::atomic::AtomicU64);
impl SimulationClock {
    fn new(now: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(now))
    }
    fn set(&self, now: u64) {
        self.0.store(now, std::sync::atomic::Ordering::Relaxed);
    }
}
impl Clock for SimulationClock {
    fn now(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}
struct FixtureRng(u8);
impl RngStream for FixtureRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
    }
}
pub(crate) fn input(name: &str) -> PathBuf {
    std::env::var_os(name).expect(name).into()
}
pub(crate) fn archive(binary: &[u8], runtime: bool) -> FilesystemArchive {
    archive_mode(binary, runtime, 0o111)
}
pub(crate) fn archive_mode(binary: &[u8], runtime: bool, program_mode: u32) -> FilesystemArchive {
    archive_with_modes(binary, runtime, program_mode, 0o755)
}
pub(crate) fn archive_with_modes(
    binary: &[u8],
    runtime: bool,
    program_mode: u32,
    directory_mode: u32,
) -> FilesystemArchive {
    let mut tar = tar::Builder::new(Vec::new());
    let mut entry = |name: &str,
                     kind: tar::EntryType,
                     mode: u32,
                     uid: u64,
                     gid: u64,
                     data: &[u8],
                     link: Option<&str>| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mtime(0);
        header.set_size(data.len() as u64);
        header.set_cksum();
        if let Some(link) = link {
            tar.append_link(&mut header, name, link).unwrap();
        } else {
            tar.append_data(&mut header, name, data).unwrap();
        }
    };
    use tar::EntryType as T;
    if runtime {
        entry(
            "seedbox/progress/wrapper",
            T::Regular,
            0o755,
            0,
            0,
            binary,
            None,
        );
        entry(
            "seedbox/progress/lib/runtime-data",
            T::Regular,
            0o644,
            0,
            0,
            b"runtime",
            None,
        );
    } else {
        entry("opt/bin", T::Directory, directory_mode, 0, 0, b"", None);
        entry("shadow/program", T::Directory, 0o755, 0, 0, b"", None);
        entry("shadow/script", T::Directory, 0o755, 0, 0, b"", None);
        entry(
            "opt/bin/program",
            T::Regular,
            program_mode,
            0,
            0,
            binary,
            None,
        );
        entry("usr/bin/interpreter", T::Regular, 0o755, 0, 0, binary, None);
        entry(
            "opt/bin/script",
            T::Regular,
            0o755,
            0,
            0,
            b"#!/usr/bin/interpreter --script-option\nfixture-script\n",
            None,
        );
        entry(
            "etc/passwd",
            T::Regular,
            0o644,
            0,
            0,
            b"app:x:1234:2345::/work:/bin/sh\n",
            None,
        );
        entry(
            "etc/group",
            T::Regular,
            0o644,
            0,
            0,
            b"team:x:2345:\n",
            None,
        );
        entry("work", T::Directory, 0o750, 1234, 2345, b"", None);
        entry("data", T::Directory, 0o750, 1234, 2345, b"", None);
        entry(
            "data/file",
            T::Regular,
            0o640,
            1234,
            2345,
            b"retained payload\n",
            None,
        );
        entry("data/hard", T::Link, 0, 0, 0, b"", Some("data/file"));
        entry(
            "data/link",
            T::Symlink,
            0o777,
            1234,
            2345,
            b"",
            Some("file"),
        );
    }
    FilesystemArchive::new(tar.into_inner().unwrap()).unwrap()
}
// Used by integration admission tests, not the private snapshot fixture.
#[allow(dead_code)]
pub(crate) fn check_denied<M: Machine<Context = ()>>(mut machine: M, diagnostic: &str)
where
    M::Error: Debug + std::fmt::Display,
{
    let clock = SimulationClock::new(0);
    let mut rng = FixtureRng(7);
    let mut inbox = VecDeque::new();
    let mut events = Vec::new();
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "denied launch timed out"
        );
        let now = Timestamp::from_nanos(clock.now());
        let upto = Timestamp::from_nanos(clock.now() + 5_000_000);
        let mut staged = Vec::new();
        let result = machine.run(
            &mut RunContext {
                execution: Default::default(),
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                inbox: Inbox::new(&mut inbox, now),
                outbox: Outbox::new(&mut staged, now, upto),
            },
            upto,
        );
        if let Err(error) = result {
            let error = error.to_string();
            assert!(error.contains("guest supervisor failed"), "{error}");
            assert!(error.ends_with(diagnostic), "{error}");
            break;
        }
        // Failed runs do not commit their staged outputs.
        events.extend(staged);
        clock.set(upto.as_nanos());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.payload, Response::ApplicationOutcome { .. })),
            "denied launch must fail supervisor admission before a program outcome"
        );
    }
    let stream = |selected| {
        events
            .iter()
            .filter_map(|e| match &e.payload {
                Response::ApplicationOutput { stream, bytes } if *stream == selected => {
                    Some(bytes.as_slice())
                }
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>()
    };
    assert!(stream(Stream::Stdout).is_empty(), "denied program executed");
    let stderr = String::from_utf8(stream(Stream::Stderr)).unwrap();
    assert!(
        !stderr.contains("Failed to create process"),
        "authorization must precede DR: {stderr}"
    );
}
pub(crate) fn definition(binary: &[u8], script: bool) -> MachineDefinition {
    MachineDefinition {
        name: "prepared".into(),
        resources: Resources {
            memory_bytes: 128 * 1024 * 1024,
        },
        application: PreparedApplication {
            filesystem: archive(binary, false),
            launch: Launch {
                argv: vec![
                    if script { "script" } else { "program" }.into(),
                    "one two".into(),
                    "".into(),
                ],
                environment: BTreeMap::from([
                    ("PATH".into(), "/shadow:/opt/bin".into()),
                    ("VALUE".into(), "a=b c".into()),
                ]),
                working_directory: "/work".into(),
                user: if script { "app:team" } else { "1234:2345" }.into(),
            },
        },
    }
}
pub(crate) fn check<M: Machine<Context = ()>>(mut machine: M, script: bool, wrapped: bool)
where
    M::Error: Debug,
{
    let clock = SimulationClock::new(0);
    let mut rng = FixtureRng(7);
    let mut inbox = VecDeque::new();
    let mut events = Vec::new();
    let started = Instant::now();
    let id = NonZeroU32::new(87).unwrap();
    let mut runs = 0;
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "native prepared fixture timed out: {events:?}"
        );
        let now = Timestamp::from_nanos(clock.now());
        let upto = Timestamp::from_nanos(clock.now() + 5_000_000);
        let stop = machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock: &clock,
                    rng: &mut rng,
                    inbox: Inbox::new(&mut inbox, now),
                    outbox: Outbox::new(&mut events, now, upto),
                },
                upto,
            )
            .unwrap();
        assert!(
            !matches!(stop, StopReason::Exited { .. }),
            "application exit must preserve its guest: {events:?}"
        );
        clock.set(upto.as_nanos());
        let outcomes = events
            .iter()
            .filter(|e| matches!(e.payload, Response::ApplicationOutcome { .. }))
            .count();
        if outcomes > runs {
            assert_eq!(outcomes, runs + 1);
            assert!(
                events
                    .iter()
                    .filter_map(
                        |e| if let Response::ApplicationOutcome { code } = e.payload {
                            Some(code)
                        } else {
                            None
                        }
                    )
                    .all(|c| c == 0),
                "{events:?}"
            );
            runs += 1;
            if runs == 2 {
                break;
            }
            inbox.push_back(Input {
                at: upto,
                payload: Request::Application {
                    id,
                    command: ApplicationCommand::Start,
                },
            });
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, Response::Ready))
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|e| matches!(e.payload, Response::CommandAcknowledged {id: actual, result: CommandResult::Completed} if actual==id)).count(), 1);
    let stdout: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.payload {
            Response::ApplicationOutput {
                stream: Stream::Stdout,
                bytes,
            } => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let stderr: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.payload {
            Response::ApplicationOutput {
                stream: Stream::Stderr,
                bytes,
            } => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
    let expected = format!(
        "{}MODE={}\nARGV0={}\nUID=1234 GID=2345\nCWD=/work\nVALUE=a=b c\nFILE=retained payload\nLINKS=ok\nCONTROL=ok\n",
        if wrapped { "WRAPPER\n" } else { "" },
        if script { "SCRIPT" } else { "ELF" },
        if script {
            "/usr/bin/interpreter"
        } else {
            "program"
        }
    );
    assert_eq!(stdout, expected.repeat(2).as_bytes());
    inbox.push_back(Input {
        at: Timestamp::from_nanos(clock.now()),
        payload: Request::Shutdown,
    });
    loop {
        let now = Timestamp::from_nanos(clock.now());
        let upto = Timestamp::from_nanos(clock.now() + 5_000_000);
        let stop = machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock: &clock,
                    rng: &mut rng,
                    inbox: Inbox::new(&mut inbox, now),
                    outbox: Outbox::new(&mut events, now, upto),
                },
                upto,
            )
            .unwrap();
        if let StopReason::Exited { reason, .. } = stop {
            assert_eq!(reason, ExitReason::Shutdown);
            break;
        }
        clock.set(upto.as_nanos());
        assert!(started.elapsed() < Duration::from_secs(120));
    }
}
