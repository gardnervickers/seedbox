//! Standard base64 with padding, as etcd's JSON gateway encodes bytes.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn encode_str(text: &str) -> String {
    encode(text.as_bytes())
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|&c| c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Decode to text, replacing invalid UTF-8 as Python's lenient decoders would refuse.
pub fn decode_str(text: &str) -> Option<String> {
    String::from_utf8(decode(text)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_padding_length() {
        for (plain, coded) in [
            ("", ""),
            ("l", "bA=="),
            ("lo", "bG8="),
            ("loc", "bG9j"),
            ("lock/2778", "bG9jay8yNzc4"),
            ("\0", "AA=="),
        ] {
            assert_eq!(encode_str(plain), coded);
            assert_eq!(decode_str(coded).unwrap(), plain);
        }
        assert!(decode("b$==").is_none());
    }
}
