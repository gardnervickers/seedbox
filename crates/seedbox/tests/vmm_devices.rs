// The PC-device model is portable. Exercise the actual VMM source here so these
// output-retention checks also run on Darwin without pretending to run KVM.
#[allow(dead_code)]
#[allow(clippy::manual_div_ceil)] // Existing PIT arithmetic is outside these UART tests.
#[path = "../../seedbox-vmm/src/devices.rs"]
mod devices;

#[test]
fn uart_output_is_persisted_before_completion_and_limited_without_truncation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("console");
    let mut uart = devices::Uart::default();
    uart.output_file = Some(std::fs::File::create(&path).unwrap());
    uart.output_limit = Some(2);
    uart.write(0, b'O').unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"O");
    uart.transmit(b'K').unwrap();
    assert!(uart.write(0, b'!').is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"OK");
    assert_eq!(uart.out, b"OK");
    drop(uart);
    assert_eq!(std::fs::read(&path).unwrap(), b"OK");
}

#[test]
fn uart_baud_register_writes_do_not_enter_console_output() {
    let mut uart = devices::Uart::default();
    uart.write(3, 0x80).unwrap();
    uart.write(0, 12).unwrap();
    assert!(uart.out.is_empty());
    uart.write(3, 0).unwrap();
    uart.write(0, b'A').unwrap();
    assert_eq!(uart.out, b"A");
}
