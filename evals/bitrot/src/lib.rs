//! A single-node key-value store whose log records carry a CRC-32. Every
//! write is fsynced before its acknowledgement, so a crash loses nothing;
//! a restart replays the log. The planted bug skips the checksum on replay,
//! so a record garbled by silent disk corruption comes back as a value that
//! was never written.
pub const PORT: u16 = 7002;
pub const LOG: &str = "/data/log";

/// CRC-32 (IEEE), bitwise.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// One log line: `KEY VALUE CRC`, the checksum over `KEY VALUE`.
pub fn record(key: &str, value: &str) -> String {
    let body = format!("{key} {value}");
    format!("{body} {:08x}\n", crc32(body.as_bytes()))
}

/// A replayed line's key and value: checked against its CRC, or, unchecked,
/// whatever its first two fields hold.
pub fn parse(line: &str, checked: bool) -> Option<(&str, &str)> {
    let (body, crc) = line.rsplit_once(' ')?;
    let (key, value) = body.split_once(' ')?;
    if checked && u32::from_str_radix(crc, 16).ok()? != crc32(body.as_bytes()) {
        return None;
    }
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_garbled_record_fails_its_checksum_but_not_the_unchecked_parse() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let line = record("k1-2", "v1-2");
        assert_eq!(parse(line.trim_end(), true), Some(("k1-2", "v1-2")));
        let garbled = line.trim_end().replace("v1-2", "v1\u{7f}2");
        assert_eq!(parse(&garbled, true), None);
        assert_eq!(parse(&garbled, false), Some(("k1-2", "v1\u{7f}2")));
    }
}
