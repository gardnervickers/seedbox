//! A single-node append log that deduplicates client retries by request ID.
//! Every append is durable before it is acknowledged. The planted bug keeps
//! the table of applied request IDs only in memory, so after a restart a
//! retry of an append that was applied but never acknowledged applies again.
pub const PORT: u16 = 7004;
pub const LOG: &str = "/data/log";
