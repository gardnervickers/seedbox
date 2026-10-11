//! Real supervisor process, clock and relay contracts, without a guest or VM.
#![cfg(target_os = "linux")]
#[path = "support/guest.rs"]
mod guest;
#[path = "support/process.rs"]
mod process;
use guest::Guest;
use std::{
    ffi::CString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    time::{Duration, Instant},
};

fn receive(channel: &mut UnixStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        channel.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() <= 4096);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

fn inherited_limit(guest: &Guest) {
    let fd = guest.set("application_read", 0) as i32;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    loop {
        let mut pending = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as i32;
        assert!(unsafe { libc::poll(&mut pending, 1, timeout) } > 0);
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) },
            1
        );
        bytes.push(byte);
        assert!(bytes.len() < 128);
        if byte == b'\n' {
            break;
        }
    }
    let mut allowed = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut allowed) },
        0
    );
    let expected = allowed.rlim_max.min(1_048_672);
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        format!("limit {expected} {expected}\n")
    );
}
fn exited(status: i64, code: i32) {
    assert!(libc::WIFEXITED(status as i32), "wait status {status}");
    assert_eq!(libc::WEXITSTATUS(status as i32), code);
}

#[test]
fn actual_supervisor_reaps_before_ack_and_checks_exec_success() {
    if !process::isolated(concat!(
        module_path!(),
        "::actual_supervisor_reaps_before_ack_and_checks_exec_success"
    )) {
        return;
    }
    let guest = Guest::new();
    let network: unsafe extern "C" fn(*const libc::c_char, *mut u32, *mut u32) -> i32 =
        unsafe { guest.symbol(c"sb_network") };
    for (cidr, expected) in [
        ("10.0.0.3/24", Some((0x0a000003, 0xffffff00))),
        ("10.0.0.3/0", Some((0x0a000003, 0))),
        ("10.0.0.3/32", Some((0x0a000003, u32::MAX))),
        ("10.0.0.3/33", None),
        ("10.0.0.3/", None),
        ("10.0.0.3/24junk", None),
        ("bad/24", None),
    ] {
        let cidr = CString::new(cidr).unwrap();
        let (mut ip, mut mask) = (0, 0);
        let status = unsafe { network(cidr.as_ptr(), &mut ip, &mut mask) };
        if let Some(expected) = expected {
            assert_eq!(status, 0);
            assert_eq!((ip, mask), expected);
        } else {
            assert_ne!(status, 0);
        }
    }
    let child = guest.child();
    let configure = |mode| guest.call("arguments", Some(child.to_str().unwrap()), Some(mode), 0);
    let (management, mut channel) = UnixStream::pair().unwrap();
    management.set_nonblocking(true).unwrap();
    channel
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    channel
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    guest.set("management", management.as_raw_fd().into());
    let (diagnostic, diagnostic_writer) = UnixStream::pair().unwrap();
    diagnostic
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    guest.set("diagnostic", diagnostic_writer.as_raw_fd().into());
    let mut diagnostic = BufReader::new(diagnostic);
    configure("child");
    let original = guest.get("start") as i32;
    assert!(original > 0);
    let mut line = String::new();
    diagnostic.read_line(&mut line).unwrap();
    let values: Vec<i64> = line
        .strip_prefix("limit ")
        .unwrap()
        .split_whitespace()
        .map(|v| v.parse().unwrap())
        .collect();
    assert_eq!(values[0], libc::RLIMIT_NOFILE as i64);
    assert_eq!(&values[1..3], [1_048_672, 1_048_672]);
    assert!(values[3] >= 0);
    line.clear();
    diagnostic.read_line(&mut line).unwrap();
    assert_eq!(line, "exec /seedbox/progress/bin64/drrun\n");
    let mut expected = vec![
        "/seedbox/progress/bin64/drrun",
        "-opt_speed",
        "-disable_traces",
    ];
    if cfg!(target_arch = "aarch64") {
        expected.push("-ldstex2cas");
    }
    expected.extend([
        "-c",
        "/seedbox/progress/libseedbox_progress.so",
        "--blocks-per-progress-call",
        "4294967295",
        "--",
        child.to_str().unwrap(),
        "child",
    ]);
    for argument in expected {
        line.clear();
        diagnostic.read_line(&mut line).unwrap();
        assert_eq!(line, format!("arg {argument}\n"));
    }
    inherited_limit(&guest);
    let class = guest.root.path().join("class");
    let devices = guest.root.path().join("dev");
    fs::create_dir_all(class.join("vport3p7")).unwrap();
    fs::create_dir(&devices).unwrap();
    let name = class.join("vport3p7/name");
    fs::write(&name, "unrelated\n").unwrap();
    assert_eq!(
        guest.call("find_management", class.to_str(), devices.to_str(), 0),
        -1
    );
    fs::write(&name, "seedbox\n").unwrap();
    fs::write(devices.join("vport3p7"), []).unwrap();
    let discovered = guest.call("open_management", class.to_str(), devices.to_str(), 0) as i32;
    assert!(discovered >= 0);
    assert_ne!(
        unsafe { libc::fcntl(discovered, libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    assert_ne!(
        unsafe { libc::fcntl(discovered, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert_eq!(unsafe { libc::close(discovered) }, 0);
    let started = Instant::now();
    guest.get("ready");
    assert!(started.elapsed() >= Duration::from_millis(10));
    let readiness: Vec<_> = (0..11).map(|index| guest.set("readiness", index)).collect();
    assert_eq!(&readiness[..3], [0xf6, 5, 1]);
    assert_ne!(readiness[3] & i64::from(libc::O_NONBLOCK), 0);
    assert_ne!(readiness[4] & i64::from(libc::FD_CLOEXEC), 0);
    assert_eq!(&readiness[5..], [0, 0, 1, 0, 0xf6, 2]);
    channel.write_all(b"stop ").unwrap();
    guest.get("relay_management");
    assert_eq!(guest.get("child"), original.into());
    assert_eq!(guest.get("command_used"), 5);
    channel.write_all(b"1\n").unwrap();
    guest.get("relay_management");
    assert_eq!(guest.get("child"), -1);
    assert_eq!(guest.get("command_used"), 0);
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(original, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert!(receive(&mut channel).contains("SEEDBOX_EFFECT id=1 action=stop guest_ns="));
    channel.write_all(b"stop 2\nstart 3\n").unwrap();
    guest.get("relay_management");
    assert!(guest.get("child") > 0);
    assert_eq!(guest.get("command_used"), 0);
    assert!(receive(&mut channel).contains("SEEDBOX_FAILURE id=2"));
    assert!(receive(&mut channel).contains("SEEDBOX_EFFECT id=3 action=start"));
    inherited_limit(&guest);
    for (command, effect, frozen) in [
        ("freeze 5\n", "SEEDBOX_EFFECT id=5 action=freeze ", true),
        ("thaw 6\n", "SEEDBOX_EFFECT id=6 action=thaw ", false),
        ("freeze 7\n", "SEEDBOX_EFFECT id=7 action=freeze ", true),
    ] {
        channel.write_all(command.as_bytes()).unwrap();
        guest.get("relay_management");
        assert!(guest.get("child") > 0);
        assert_eq!(guest.get("command_used"), 0);
        assert!(receive(&mut channel).contains(effect));
        let state = fs::read_to_string(format!("/proc/{}/stat", guest.get("child"))).unwrap();
        assert_eq!(
            state.rsplit(')').next().unwrap().as_bytes()[1] == b'T',
            frozen
        );
    }
    assert_eq!(
        unsafe { libc::waitpid(guest.get("child") as i32, &mut status, libc::WNOHANG) },
        0
    );
    assert_eq!(guest.get("stop"), 0);
    guest.call("arguments", Some("/definitely/no/such/executable"), None, 0);
    guest.text("control", "start 4");
    assert!(guest.get("child") < 0);
    assert!(receive(&mut channel).contains("SEEDBOX_FAILURE id=4"));
    assert_eq!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    for (index, (value, seconds, nanos)) in [
        (1_500_000_001i64, 1, 500_000_001),
        (-1, -1, 999_999_999),
        (-2_000_000_000, -2, 0),
        (0, 0, 0),
    ]
    .into_iter()
    .enumerate()
    {
        guest.text("control", &format!("clock_step {} {value}", index + 5));
        assert!(guest.get("child") < 0);
        assert_eq!(guest.get("adjustments"), index as i64 + 1);
        assert_eq!(
            guest.get("clock_modes"),
            i64::from(libc::ADJ_SETOFFSET | libc::ADJ_NANO)
        );
        assert_eq!(guest.get("clock_seconds"), seconds);
        assert_eq!(guest.get("clock_nanoseconds"), nanos);
        assert!(receive(&mut channel).contains(&format!(
            "SEEDBOX_EFFECT id={} action=clock_step",
            index + 5
        )));
    }
    guest.set("adjustment_error", libc::EINVAL.into());
    guest.text("control", "clock_step 9 -9000000000000000");
    assert!(receive(&mut channel).contains("SEEDBOX_FAILURE id=9 action=clock_step"));
    guest.set("adjustment_error", 0);
    assert_eq!(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }, 100);
    for (index, (value, tick, frequency)) in [
        (100_000_000i64, 11000, 0),
        (-100_000_000, 9000, 0),
        (150_000, 10002, -50 * 65536),
        (-149_999, 9999, -49_999i64 * 65536 / 1000),
        (1, 10000, 66),
        (0, 10000, 0),
    ]
    .into_iter()
    .enumerate()
    {
        let before = guest.get("adjustments");
        guest.text("control", &format!("clock_rate {} {value}", index + 10));
        assert!(guest.get("child") < 0);
        assert_eq!(guest.get("adjustments"), before + 1);
        assert_eq!(
            guest.get("clock_modes"),
            i64::from(libc::ADJ_TICK | libc::ADJ_FREQUENCY)
        );
        assert_eq!(guest.get("clock_tick"), tick);
        assert_eq!(guest.get("clock_frequency"), frequency);
        assert!(receive(&mut channel).contains(&format!(
            "SEEDBOX_EFFECT id={} action=clock_rate",
            index + 10
        )));
    }
    let before = guest.get("adjustments");
    guest.text("control", "clock_rate 16 1000000001");
    assert_eq!(guest.get("adjustments"), before);
    assert!(receive(&mut channel).contains("SEEDBOX_FAILURE id=16 action=clock_rate"));
    let cmdline = guest.root.path().join("cmdline");
    let socket = guest.root.path().join("control.sock");
    for disabled in [
        "quiet\n",
        "seedbox.control=0\n",
        "seedbox.control=11\n",
        "xseedbox.control=1\n",
        "app=seedbox.control=1\n",
    ] {
        fs::write(&cmdline, disabled).unwrap();
        guest.call("relay_socket", cmdline.to_str(), socket.to_str(), 0);
        assert_eq!(guest.get("listener"), -1);
        assert!(!socket.exists());
    }
    fs::write(&cmdline, "quiet\tseedbox.control=1 root=/dev/ram0\n").unwrap();
    guest.call("relay_socket", cmdline.to_str(), socket.to_str(), 0);
    assert!(guest.get("listener") >= 0);
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o666
    );
    let connect = || {
        let peer = UnixStream::connect(&socket).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        guest.get("channels");
        peer
    };
    let mut peer = connect();
    assert!(guest.get("client") >= 0);
    assert_eq!(guest.get("session"), 1);
    let request = r#"{"version":1,"id":1,"op":"now"}"#;
    peer.write_all(request.as_bytes()).unwrap();
    guest.get("channels");
    assert_eq!(guest.get("waiting"), 0);
    peer.write_all(b"\n").unwrap();
    guest.get("channels");
    assert_eq!(guest.get("waiting"), 1);
    assert_eq!(receive(&mut channel), format!("SBX_RPC1 1 1 {request}\n"));
    let reply = "SBX_REPLY1 1 1 {\"version\":1,\"id\":1,\"ok\":true}";
    channel.write_all(&reply.as_bytes()[..8]).unwrap();
    guest.get("channels");
    assert_eq!(guest.get("waiting"), 1);
    channel.write_all(&reply.as_bytes()[8..]).unwrap();
    channel.write_all(b"\n").unwrap();
    guest.get("channels");
    assert_eq!(guest.get("waiting"), 0);
    assert_eq!(receive(&mut peer), "{\"version\":1,\"id\":1,\"ok\":true}\n");
    let listener = guest.get("listener");
    drop(peer);
    guest.get("channels");
    assert_eq!(guest.get("client"), -1);
    assert_eq!(guest.get("listener"), listener);
    let peer = connect();
    assert_eq!(guest.get("session"), 2);
    drop(peer);
    guest.get("channels");
    assert_eq!(guest.get("client"), -1);
    assert_eq!(guest.get("relay"), 1);
    let mut peer = connect();
    assert_eq!(guest.get("session"), 3);
    peer.write_all(format!("{request}\n").as_bytes()).unwrap();
    guest.get("channels");
    assert_eq!(guest.get("waiting"), 2);
    assert_eq!(receive(&mut channel), format!("SBX_RPC1 2 3 {request}\n"));
    exited(guest.text("fork_control", "SBX_REPLY1 2 1 {}"), 125);
    guest.text(
        "control",
        "SBX_REPLY1 2 3 {\"version\":1,\"id\":1,\"ok\":true}",
    );
    assert_eq!(guest.get("waiting"), 0);
    assert_eq!(receive(&mut peer), "{\"version\":1,\"id\":1,\"ok\":true}\n");
    drop(peer);
    guest.get("channels");
    assert_eq!(guest.get("client"), -1);
    for failure in 0..3 {
        let (client, mut remote) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        guest.set("set_client", client.as_raw_fd().into());
        guest.set("set_waiting", if failure == 1 { 9 } else { 0 });
        if failure == 0 {
            remote.write_all(b"{").unwrap();
        }
        drop(remote);
        if failure == 2 {
            drop(client);
        }
        let status = guest.get("fork_channels");
        guest.set("set_client", -1);
        exited(status, 125);
    }
    assert_eq!(unsafe { libc::close(guest.get("listener") as i32) }, 0);
    guest.set("set_listener", -1);
    for (mode, expected) in [("output-exit", 9), ("exit-now", 1), ("exit-later", 7)] {
        let (mut logs, transport) = UnixStream::pair().unwrap();
        logs.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        guest.set("management", transport.as_raw_fd().into());
        configure(mode);
        exited(
            guest.set("fork_supervise", logs.as_raw_fd().into()),
            expected,
        );
        drop(transport);
        let mut output = String::new();
        logs.read_to_string(&mut output).unwrap();
        if mode == "output-exit" {
            assert!(output.contains("SBX_LOG1 1 00ff0a\n"));
            assert!(output.contains("SBX_LOG1 2 650080\n"));
        }
    }
}
