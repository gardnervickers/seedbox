//! Input-byte constructors shared by the asset-validation specifications.
pub fn image(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 120];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
    bytes[96..104].copy_from_slice(&120_u64.to_le_bytes());
    bytes[104..112].copy_from_slice(&120_u64.to_le_bytes());
    bytes
}
/// A minimal bzImage header accepted by [`crate::machine::assets::bzimage`]: four setup sectors.
pub fn bzimage_header() -> Vec<u8> {
    let mut bytes = vec![0; 6 * 512];
    bytes[0x1f1] = 4;
    bytes[0x202..0x206].copy_from_slice(b"HdrS");
    bytes[0x206..0x208].copy_from_slice(&0x020f_u16.to_le_bytes());
    bytes[0x211] = 1;
    bytes[0x236..0x238].copy_from_slice(&1_u16.to_le_bytes());
    bytes
}
