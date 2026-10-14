//! Jepsen-style reproductions under Seedbox.
//!
//! * [`kit`]: what every oracle needs: evidence and verdict lines, the
//!   simulated-time clock, seeded randomness, the World connection (or its
//!   native engine shim), a serve-while-waiting loop and wire helpers.
//! * [`nemesis`]: seeded fault schedules: pauses, partitions, kill/restart,
//!   power loss and console-triggered partitions.
//! * [`checker`]: list-append (Elle-style), queue, lock-set and Kafka checks;
//!   linearizability through `wgl` models; EDN export for Jepsen's Elle.
//! * [`systems`]: the NATS, etcd, PostgreSQL and Kafka oracles and clients.
//! * [`scenario`]: writes the committed scenario projects.
//! * [`campaign`] and [`tools`]: host-side research tooling: seed campaigns,
//!   the oracle build and the Elle cross-check. Not acceptance tests.
pub mod campaign;
pub mod checker;
pub mod kit;
pub mod nemesis;
pub mod scenario;
pub mod systems;
pub mod tools;
