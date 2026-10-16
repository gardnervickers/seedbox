//! Shared test support for tests that start processes or execute files they
//! wrote. Every helper goes through the process-wide `seedbox_exec` scope, the
//! same one production code takes, so a test's fork can never keep a writable
//! copy of a file another thread of the same test binary is about to execute.
//!
//! Keep this library's normal dependencies to `libc` and `seedbox-exec`: many
//! crates dev-depend on it.
// The package allows disallowed spawns for its developer-tool binaries; this
// library is shared test support and must spawn only through seedbox_exec.
#![deny(clippy::disallowed_methods)]
pub mod process;
