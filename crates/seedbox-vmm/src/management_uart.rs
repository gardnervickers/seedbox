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
