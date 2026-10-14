//! Checkers over recorded histories. Linearizability checks use the
//! Wing–Gong–Lowe checker (`wgl`) with the models in [`models`]; transaction
//! histories use the Elle-style list-append analysis in [`elle`], whose
//! history type is `wgl`'s, and [`edn`] exports them for Jepsen's Elle.
//! [`views`] checks single-transaction reads (non-repeatable reads, MAV);
//! [`realtime`] finds list-append reads that miss an earlier-acknowledged append.
pub mod edn;
pub mod elle;
pub mod kafka;
pub mod lockset;
pub mod models;
pub mod queue;
pub mod realtime;
pub mod views;
