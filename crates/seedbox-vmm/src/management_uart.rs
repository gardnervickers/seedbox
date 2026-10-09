//! Legacy UART admission belongs to the PC adapter, not the shared RPC policy.
pub fn write(
    framer: &mut seedbox_engine_core::control::ManagementFramer,
    uart: &mut crate::devices::Uart,
    off: u16,
    byte: u8,
) -> Result<Option<Vec<u8>>, String> {
    let before = uart.out.len();
    uart.write(off, byte)?;
    if uart.out.len() == before {
        return Ok(None);
    }
    framer.transmitted_byte(byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_core::control;

    #[test]
    fn actual_uart_divisor_bytes_do_not_contaminate_committed_request_frames() {
        let mut uart = crate::devices::Uart::default();
        let mut framer = control::ManagementFramer::default();
        write(&mut framer, &mut uart, 3, 0x80).unwrap();
        // Base-register writes while DLAB is set never reach the RPC wire.
        for byte in b"SBX_RPC1 99 1 bad\n" {
            assert!(write(&mut framer, &mut uart, 0, *byte).unwrap().is_none());
        }
        assert!(uart.out.is_empty());
        write(&mut framer, &mut uart, 3, 3).unwrap();
        for byte in b"SEEDBOX_EFFECT id=1\n" {
            assert!(write(&mut framer, &mut uart, 0, *byte).unwrap().is_none());
        }
        let line = b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}";
        for byte in line {
            assert!(write(&mut framer, &mut uart, 0, *byte).unwrap().is_none());
        }
        assert_eq!(
            write(&mut framer, &mut uart, 0, b'\n').unwrap(),
            Some(line.to_vec())
        );
    }
}
