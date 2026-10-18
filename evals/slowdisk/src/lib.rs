//! A single-node key-value store with group commit: writes are appended to a
//! log and a flusher thread fsyncs them in batches. The planted bug bounds how
//! long a write waits for its fsync and acknowledges it anyway, assuming the
//! fsync will finish; a crash before it does loses an acknowledged write. An
//! instant disk never reaches the bound.
pub const PORT: u16 = 7005;
pub const WAL: &str = "/data/wal";
