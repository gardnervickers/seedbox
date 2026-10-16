//! A single-node key-value store whose every write is durable before it is
//! acknowledged. On restart it compacts its log into a snapshot. The planted
//! bug truncates the log before the new snapshot is installed, so a crash
//! during that compaction loses every write the log held.
pub const PORT: u16 = 7002;
pub const WAL: &str = "/data/wal";
pub const SNAPSHOT: &str = "/data/snapshot";
pub const SNAPSHOT_TEMP: &str = "/data/snapshot.tmp";
