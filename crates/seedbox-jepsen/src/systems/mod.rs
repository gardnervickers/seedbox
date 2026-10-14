//! The reproduced systems: one oracle each, plus the etcd lock client.
pub mod etcd;
pub mod etcd_client;
pub mod etcd_gateway;
pub mod galera;
pub mod kafka;
pub mod mysql;
pub mod nats;
pub mod nats_proto;
pub mod pg_proto;
pub mod postgres;
