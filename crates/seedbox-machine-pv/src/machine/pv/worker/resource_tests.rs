use super::*;
fn architecture() -> Architecture {
    // Pure shape-valid placeholder; never restored or executed.
    snapshot::fixture_architecture()
}
fn header(memory: u64) -> Capture {
    let layout = boot::RamLayout::native(memory).unwrap();
    Capture {
        memory_bytes: memory,
        layout_version: boot::LAYOUT_VERSION,
        page_bytes: PAGE_BYTES as u64,
        segments: layout
            .segments
            .iter()
            .map(|r| (r.start, r.end - r.start))
            .collect(),
        state: State {
            witness: seedbox_machine::machine::execution_evidence::chain::initial(),
            witness_origin: seedbox_machine::machine::execution_evidence::chain::initial(),
            ..State::default()
        },
        architecture: architecture(),
        block: None,
    }
}
fn inputs(memory: u64) -> BootInputs {
    BootInputs {
        witness: protocol::BootWitness {
            mark: None,
            boot: 0,
            initial: seedbox_machine::machine::execution_evidence::chain::initial(),
        },
        memory_bytes: memory,
        kernel: vec![],
        initrd: vec![],
        seed: 1,
        command_line: String::new(),
        policy: None,
        disk: None,
        network: None,
    }
}
fn file(header: &Capture, delta: i64) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    protocol::send(&mut file, header).unwrap();
    let length = file.stream_position().unwrap() + header.page_bytes + header.memory_bytes;
    file.as_file()
        .set_len(length.checked_add_signed(delta).unwrap())
        .unwrap();
    file
}
#[test]
fn large_native_header_is_checked_without_allocating_or_reading_guest_ram() {
    let memory = 2048 << 20;
    let capture = header(memory);
    let file = file(&capture, 0);
    let saved = read_snapshot(file.path(), &inputs(memory)).unwrap();
    assert_eq!(saved.capture.memory_bytes, memory);
    assert!(saved.overlays.is_empty());
    assert_eq!(
        saved.file.metadata().unwrap().len(),
        saved.data_start + 16384 + memory
    );
    assert!(read_snapshot(file.path(), &inputs(128 << 20)).is_err());
}
#[test]
fn native_size_layout_version_and_trailing_mismatch_fail_before_target_allocation() {
    let memory = 128 << 20;
    for kind in 0..7 {
        let mut capture = header(memory);
        match kind {
            0 => capture.page_bytes = 4096,
            1 => capture.layout_version = 0,
            2 => capture.segments[0].1 -= 16384,
            5 => capture.state.witness_boot = 1,
            6 => capture.state.witness_origin[0] ^= 1,
            _ => (),
        }
        let file = file(
            &capture,
            if kind == 3 {
                -1
            } else if kind == 4 {
                1
            } else {
                0
            },
        );
        assert!(
            read_snapshot(file.path(), &inputs(memory)).is_err(),
            "kind={kind}"
        );
    }
}
