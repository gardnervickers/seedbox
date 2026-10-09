//! Datagrams of the host-control socket between the CLI and a running engine.
//! The engine acknowledges each request at a world-slice boundary.

/// Pause at the next slice boundary; acknowledged with `PAUSED`.
pub const PAUSE: &[u8] = b"pause";
/// Leave a pause; acknowledged with `RUNNING`.
pub const RESUME: &[u8] = b"resume";
pub const PAUSED: &[u8] = b"paused";
pub const RUNNING: &[u8] = b"running";
/// Reply to any other request.
pub const INVALID: &[u8] = b"invalid";
/// Largest datagram either side sends or reads.
pub const MAX_DATAGRAM: usize = 32;
