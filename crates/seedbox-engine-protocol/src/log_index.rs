//! Retained log index wire format.
pub const MAGIC: &[u8; 8] = b"SBLOG001";
/// Records one stream's index may hold.
pub const MAX_CHUNKS: usize = 65_536;
/// Bytes of one encoded `ChunkRecord`.
pub const RECORD_BYTES: usize = 40;

/// Index filename shared by every UART sink and the retained-log reader.
pub fn index_name(raw: &str) -> String {
    format!("{raw}.chunks")
}

/// One burst of raw stream bytes. Encoded as five little-endian u64s in field order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChunkRecord {
    /// Virtual time of the burst's first byte.
    pub time_ns: u64,
    /// Machine incarnation; a valid record fits in u32.
    pub incarnation: u64,
    /// Machine-wide burst order across both streams.
    pub order: u64,
    /// Byte offset in the raw log.
    pub offset: u64,
    /// Byte length in the raw log.
    pub length: u64,
}

impl ChunkRecord {
    pub fn to_bytes(&self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0u8; RECORD_BYTES];
        let fields = [
            self.time_ns,
            self.incarnation,
            self.order,
            self.offset,
            self.length,
        ];
        for (chunk, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(fields) {
            *chunk = value.to_le_bytes();
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8; RECORD_BYTES]) -> Self {
        let field = |i: usize| {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            u64::from_le_bytes(word)
        };
        Self {
            time_ns: field(0),
            incarnation: field(1),
            order: field(2),
            offset: field(3),
            length: field(4),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_encode_as_five_little_endian_words_in_field_order() {
        let record = ChunkRecord {
            time_ns: 1,
            incarnation: 2,
            order: 3,
            offset: 4,
            length: 0x0102_0304_0506_0708,
        };
        let bytes = record.to_bytes();
        let mut expected = Vec::new();
        for value in [1u64, 2, 3, 4, 0x0102_0304_0506_0708] {
            expected.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(ChunkRecord::from_bytes(&bytes), record);
    }
}
