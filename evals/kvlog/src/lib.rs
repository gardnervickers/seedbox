//! A single-node key-value store with a write-ahead log. The planted bug
//! acknowledges a write before it is durable; a crash inside the gap between
//! acknowledgement and the next flush loses acknowledged data.
pub const PORT: u16 = 7001;
pub const WAL: &str = "/data/wal";
