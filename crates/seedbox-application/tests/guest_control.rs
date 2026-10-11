//! Rust owns guest-component expectations; the private C adapter calls production code.
#[path = "support/guest.rs"]
mod guest;
use guest::Guest;
use std::ffi::{CStr, CString};

#[test]
fn actual_guest_command_parser_rejects_ambiguous_and_uncorrelated_ids() {
    let guest = Guest::new();
    let abi: unsafe extern "C" fn(u32) -> i64 = unsafe { guest.symbol(c"sb_abi") };
    let fields: Vec<_> = (0..5).map(|field| unsafe { abi(field) }).collect();
    assert_eq!(&fields[..3], [0, 0xf6, 0xf7]);
    assert!(fields[1] + fields[4] <= fields[1] + fields[3]);
    assert!(fields[2] + fields[4] <= fields[1] + fields[3]);
    type Parse =
        unsafe extern "C" fn(*const libc::c_char, *mut libc::c_char, *mut u32, *mut i64) -> i32;
    let parse: Parse = unsafe { guest.symbol(c"sb_parse") };
    let parse = |text: &str| {
        let text = CString::new(text).unwrap();
        let mut action = [0; 64];
        let (mut id, mut value) = (0, 0);
        let result = unsafe { parse(text.as_ptr(), action.as_mut_ptr(), &mut id, &mut value) };
        (
            result,
            unsafe { CStr::from_ptr(action.as_ptr()) }
                .to_str()
                .unwrap()
                .to_owned(),
            id,
            value,
        )
    };
    for (text, action, id) in [
        ("stop 1", "stop", 1),
        ("start 4294967295", "start", u32::MAX),
        ("stop", "stop", 0),
        ("start", "start", 0),
        ("freeze 7", "freeze", 7),
        ("thaw 8", "thaw", 8),
    ] {
        assert_eq!(parse(text), (0, action.into(), id, 0));
    }
    for (index, value) in [1_500_000_000, -1, 0, i64::MAX, i64::MIN]
        .into_iter()
        .enumerate()
    {
        let id = index as u32 + 7;
        assert_eq!(
            parse(&format!("clock_step {id} {value}")),
            (0, "clock_step".into(), id, value)
        );
    }
    for (index, value) in [-100_000_000, 0, 25].into_iter().enumerate() {
        let id = index as u32 + 3;
        assert_eq!(
            parse(&format!("clock_rate {id} {value}")),
            (0, "clock_rate".into(), id, value)
        );
    }
    for text in [
        "stop 0",
        "stop 01",
        "start -1",
        "start 4294967296",
        "stop 1 extra",
        "stop  1",
        "go 1",
        "start ",
        "start 184467440737095516160",
        "clock_step",
        "clock_step 1",
        "clock_step 1 ",
        "clock_step 1 -0",
        "clock_step 1 +5",
        "clock_step 1 05",
        "clock_step 1 1x",
        "clock_step 1 9223372036854775808",
        "clock_step 1 -9223372036854775809",
        "clock_step 0 5",
        "clock_step 1 5 6",
        "stop 1 5",
        "clock_rate",
        "clock_rate 1",
        "clock_rate 1 -0",
        "clock_rate 1 +5",
        "clock_rate 0 5",
        "clock_rate 1 5 6",
        "freez 1",
        "thaw 0",
        "freeze 1 5",
    ] {
        assert_ne!(parse(text).0, 0, "accepted {text:?}");
    }
}

#[test]
fn guest_application_output_preserves_stream_identity_and_every_byte() {
    use seedbox_engine_protocol::application_log::{decode, Stream};
    let guest = Guest::new();
    assert_eq!(guest.get("output_open"), 0);
    let reader = guest.get("output_read") as i32;
    let writer = guest.get("output_write") as i32;
    assert_ne!(
        unsafe { libc::fcntl(reader, libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    assert_eq!(
        unsafe { libc::fcntl(writer, libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    let frame: unsafe extern "C" fn(u32, usize, *mut libc::c_char) -> isize =
        unsafe { guest.symbol(c"sb_frame") };
    let mut buffer = [0; 4112];
    assert_eq!(unsafe { frame(0, 2048, buffer.as_mut_ptr().cast()) }, 0);
    let bytes: Vec<u8> = (0..=255).collect();
    for (index, stream) in [Stream::Stdout, Stream::Stderr].into_iter().enumerate() {
        assert_eq!(
            unsafe { libc::write(writer, bytes.as_ptr().cast(), bytes.len()) },
            bytes.len() as isize
        );
        let count = unsafe { frame(index as u32, 2048, buffer.as_mut_ptr().cast()) };
        assert!(count > 0);
        let chunk = decode(&buffer[..count as usize - 1]).unwrap().unwrap();
        assert_eq!(chunk.stream, stream);
        assert_eq!(chunk.bytes, bytes);
    }
    assert_eq!(guest.get("output_finish"), 0);
    assert_eq!(unsafe { frame(0, 2048, buffer.as_mut_ptr().cast()) }, 0);
    assert_eq!(guest.get("output_read"), -1);
}

#[cfg(target_os = "linux")]
#[test]
fn retained_descriptors_are_closed_by_exec() {
    let Some(descriptors) = std::env::var_os("SEEDBOX_TEST_IO_DESCRIPTORS") else {
        return;
    };
    for descriptor in descriptors.to_str().unwrap().split(',') {
        assert_eq!(
            unsafe { libc::fcntl(descriptor.parse().unwrap(), libc::F_GETFD) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn supervisor_io_survives_hidden_device_nodes_without_leaking_through_exec() {
    use std::{fs, process::Command};
    let guest = Guest::new();
    let path = guest.root.path().join("io-device");
    fs::write(&path, []).unwrap();
    assert_eq!(guest.text("io_retain", path.to_str().unwrap()), 0);
    let boot = guest.get("io_boot") as i32;
    assert_ne!(
        unsafe { libc::fcntl(boot, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    fs::remove_file(path).unwrap();
    assert_eq!(guest.set("io_enable", 0xf8), 0);
    let active = guest.get("io_active") as i32;
    assert_ne!(
        unsafe { libc::fcntl(active, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert_eq!(guest.set("io_disable", 0xf8), 0);
    assert_eq!(guest.get("io_active"), -1);
    assert_eq!(unsafe { libc::fcntl(active, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    assert_eq!(guest.set("io_enable", 0xf6), 0);
    let active = guest.get("io_active") as i32;
    assert_eq!(
        unsafe { libc::write(active, b"retained".as_ptr().cast(), 8) },
        8
    );
    let mut read = [0; 8];
    assert_eq!(
        unsafe { libc::pread(boot, read.as_mut_ptr().cast(), read.len(), 0) },
        8
    );
    assert_eq!(&read, b"retained");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "retained_descriptors_are_closed_by_exec"])
        .env("SEEDBOX_TEST_IO_DESCRIPTORS", format!("{boot},{active}"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert_eq!(guest.set("io_disable", 0xf6), 0);
}
