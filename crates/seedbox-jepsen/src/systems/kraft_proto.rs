//! A minimal Kafka wire-protocol client for the KRaft workloads: just the
//! admin and quorum APIs they use, each at one fixed flexible version
//! (Kafka 4.x supports them all). A raw client lets every request go to a
//! chosen node — a broker, which forwards controller writes, or a controller
//! directly, which serves reads from its own metadata image — and makes every
//! outcome explicit: a reply with an error code, or no reply in time.
//!
//! Schemas are Kafka's `clients/src/main/resources/common/message/*.json`.
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

pub const INIT_PRODUCER_ID: i16 = 22;
pub const CREATE_TOPICS: i16 = 19;
pub const DELETE_TOPICS: i16 = 20;
pub const DESCRIBE_CONFIGS: i16 = 32;
pub const INCREMENTAL_ALTER_CONFIGS: i16 = 44;
pub const DESCRIBE_QUORUM: i16 = 55;
pub const DESCRIBE_CLUSTER: i16 = 60;
pub const ADD_RAFT_VOTER: i16 = 80;
pub const REMOVE_RAFT_VOTER: i16 = 81;

/// `ConfigResource.Type.CLIENT_METRICS`: a subscription needs no other
/// metadata to exist, and its `metrics` config is a free-form list.
pub const CLIENT_METRICS: i8 = 16;
pub const TOPIC: i8 = 2;
/// `AlterConfigOp.OpType.APPEND`.
pub const OP_APPEND: i8 = 2;

pub const NONE: i16 = 0;
pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
pub const REQUEST_TIMED_OUT: i16 = 7;
pub const TOPIC_ALREADY_EXISTS: i16 = 36;
pub const INVALID_REPLICATION_FACTOR: i16 = 38;
pub const NOT_CONTROLLER: i16 = 41;
pub const UNKNOWN_TOPIC_ID: i16 = 100;

/// The protocol name of an error code, for evidence.
pub fn error_name(code: i16) -> String {
    let name = match code {
        -1 => "UNKNOWN_SERVER_ERROR",
        0 => "NONE",
        3 => "UNKNOWN_TOPIC_OR_PARTITION",
        6 => "NOT_LEADER_OR_FOLLOWER",
        7 => "REQUEST_TIMED_OUT",
        13 => "NETWORK_EXCEPTION",
        15 => "COORDINATOR_NOT_AVAILABLE",
        35 => "UNSUPPORTED_VERSION",
        36 => "TOPIC_ALREADY_EXISTS",
        37 => "INVALID_PARTITIONS",
        38 => "INVALID_REPLICATION_FACTOR",
        40 => "INVALID_CONFIG",
        41 => "NOT_CONTROLLER",
        42 => "INVALID_REQUEST",
        44 => "POLICY_VIOLATION",
        56 => "KAFKA_STORAGE_ERROR",
        89 => "THROTTLING_QUOTA_EXCEEDED",
        96 => "INCONSISTENT_VOTER_SET",
        100 => "UNKNOWN_TOPIC_ID",
        101 => "UNKNOWN_LEADER_EPOCH",
        110 => "VOTER_NOT_FOUND",
        111 => "DUPLICATE_VOTER",
        _ => "",
    };
    if name.is_empty() {
        format!("ERROR_{code}")
    } else {
        name.into()
    }
}

/// A transport failure: no usable reply, so the request's effect is unknown.
#[derive(Debug, Clone)]
pub struct NoReply(pub String);

impl NoReply {
    /// The connection was never established, so no request was sent: a
    /// definite failure rather than an unknown outcome.
    pub fn unsent(&self) -> bool {
        ["connect ", "resolve ", "socket "]
            .iter()
            .any(|p| self.0.starts_with(p))
    }
}

impl std::fmt::Display for NoReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------- encoding

#[derive(Default)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn i8(&mut self, v: i8) -> &mut Self {
        self.0.push(v as u8);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.i8(v as i8)
    }
    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn uvarint(&mut self, mut v: u64) -> &mut Self {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
        self
    }
    /// A classic nullable string (int16 length), as in the request header.
    pub fn nullable_string(&mut self, v: Option<&str>) -> &mut Self {
        match v {
            None => self.i16(-1),
            Some(s) => {
                self.i16(s.len() as i16);
                self.0.extend_from_slice(s.as_bytes());
                self
            }
        }
    }
    pub fn compact_string(&mut self, v: &str) -> &mut Self {
        self.uvarint(v.len() as u64 + 1);
        self.0.extend_from_slice(v.as_bytes());
        self
    }
    pub fn compact_nullable_string(&mut self, v: Option<&str>) -> &mut Self {
        match v {
            None => self.uvarint(0),
            Some(s) => self.compact_string(s),
        }
    }
    /// A compact array's length prefix.
    pub fn array(&mut self, len: usize) -> &mut Self {
        self.uvarint(len as u64 + 1)
    }
    pub fn uuid(&mut self, v: [u8; 16]) -> &mut Self {
        self.0.extend_from_slice(&v);
        self
    }
    /// An empty tagged-field section.
    pub fn tags(&mut self) -> &mut Self {
        self.uvarint(0)
    }
}

pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

type DecResult<T> = Result<T, String>;

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> DecResult<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(format!(
                "truncated response: need {n} bytes at {} of {}",
                self.pos,
                self.buf.len()
            ));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
    pub fn i8(&mut self) -> DecResult<i8> {
        Ok(self.take(1)?[0] as i8)
    }
    pub fn bool(&mut self) -> DecResult<bool> {
        Ok(self.i8()? != 0)
    }
    pub fn i16(&mut self) -> DecResult<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u16(&mut self) -> DecResult<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn i32(&mut self) -> DecResult<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> DecResult<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn uuid(&mut self) -> DecResult<[u8; 16]> {
        Ok(self.take(16)?.try_into().unwrap())
    }
    pub fn uvarint(&mut self) -> DecResult<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)?[0];
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint too long".into())
    }
    pub fn compact_nullable_string(&mut self) -> DecResult<Option<String>> {
        let len = self.uvarint()?;
        if len == 0 {
            return Ok(None);
        }
        let bytes = self.take(len as usize - 1)?;
        Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
    }
    pub fn compact_string(&mut self) -> DecResult<String> {
        Ok(self.compact_nullable_string()?.unwrap_or_default())
    }
    /// A compact array's length; `None` for a null array.
    pub fn array(&mut self) -> DecResult<Option<usize>> {
        let len = self.uvarint()?;
        Ok((len > 0).then(|| len as usize - 1))
    }
    /// Skip a tagged-field section.
    pub fn tags(&mut self) -> DecResult<()> {
        let count = self.uvarint()?;
        for _ in 0..count {
            let _tag = self.uvarint()?;
            let size = self.uvarint()?;
            self.take(size as usize)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- transport

/// One connection to one node. Requests are strictly sequential.
pub struct Conn {
    pub node: String,
    stream: TcpStream,
    correlation: i32,
    client_id: String,
}

impl Conn {
    pub fn connect(
        node: &str,
        port: u16,
        timeout: Duration,
        client_id: &str,
    ) -> Result<Self, NoReply> {
        let address = (node, port)
            .to_socket_addrs()
            .map_err(|e| NoReply(format!("resolve {node}: {e}")))?
            .next()
            .ok_or_else(|| NoReply(format!("resolve {node}: no address")))?;
        let stream = TcpStream::connect_timeout(&address, timeout)
            .map_err(|e| NoReply(format!("connect {node}:{port}: {e}")))?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|e| NoReply(format!("socket {node}: {e}")))?;
        Ok(Self {
            node: node.into(),
            stream,
            correlation: 0,
            client_id: client_id.into(),
        })
    }

    /// Send one request (header v2) and return the response body after its
    /// header (v1). Any failure leaves the connection unusable.
    pub fn call(&mut self, api_key: i16, version: i16, body: &[u8]) -> Result<Vec<u8>, NoReply> {
        self.correlation = self.correlation.wrapping_add(1);
        let mut header = Enc::default();
        header
            .i16(api_key)
            .i16(version)
            .i32(self.correlation)
            .nullable_string(Some(&self.client_id))
            .tags();
        let size = (header.0.len() + body.len()) as i32;
        let mut frame = Vec::with_capacity(4 + size as usize);
        frame.extend_from_slice(&size.to_be_bytes());
        frame.extend_from_slice(&header.0);
        frame.extend_from_slice(body);
        let node = self.node.clone();
        let io = |e: std::io::Error| NoReply(format!("{node}: {e}"));
        self.stream.write_all(&frame).map_err(io)?;
        let mut size = [0u8; 4];
        self.stream.read_exact(&mut size).map_err(io)?;
        let size = i32::from_be_bytes(size);
        if !(4..=64 << 20).contains(&size) {
            return Err(NoReply(format!("{}: bad response size {size}", self.node)));
        }
        let mut response = vec![0u8; size as usize];
        self.stream.read_exact(&mut response).map_err(io)?;
        let mut dec = Dec::new(&response);
        let correlation = dec.i32().map_err(NoReply)?;
        if correlation != self.correlation {
            return Err(NoReply(format!(
                "{}: correlation {correlation}, expected {}",
                self.node, self.correlation
            )));
        }
        dec.tags().map_err(NoReply)?;
        Ok(response[dec.pos..].to_vec())
    }
}

fn decode<T>(
    node: &str,
    body: &[u8],
    f: impl FnOnce(&mut Dec) -> DecResult<T>,
) -> Result<T, NoReply> {
    f(&mut Dec::new(body)).map_err(|e| NoReply(format!("{node}: {e}")))
}

// ---------------------------------------------------------------- APIs

/// IncrementalAlterConfigs v1: append `value` to each resource's `config`
/// list in one request (the controller applies the request atomically).
/// Returns each resource's error code and message.
pub fn append_configs(
    conn: &mut Conn,
    resource_type: i8,
    config: &str,
    appends: &[(String, String)],
) -> Result<Vec<(String, i16, Option<String>)>, NoReply> {
    let mut e = Enc::default();
    e.array(appends.len());
    for (resource, value) in appends {
        e.i8(resource_type).compact_string(resource).array(1);
        e.compact_string(config)
            .i8(OP_APPEND)
            .compact_nullable_string(Some(value))
            .tags();
        e.tags();
    }
    e.bool(false).tags();
    let body = conn.call(INCREMENTAL_ALTER_CONFIGS, 1, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        let n = d.array()?.unwrap_or(0);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let code = d.i16()?;
            let message = d.compact_nullable_string()?;
            let _type = d.i8()?;
            let name = d.compact_string()?;
            d.tags()?;
            out.push((name, code, message));
        }
        Ok(out)
    })
}

/// One resource's DescribeConfigs result: an error code, or the value of the
/// requested config (`None` when it is unset).
#[derive(Debug, Clone)]
pub struct Described {
    pub resource: String,
    pub error: i16,
    pub message: Option<String>,
    pub value: Option<String>,
}

/// DescribeConfigs v4 of one config across resources, as the node's own
/// metadata image holds it.
pub fn describe_configs(
    conn: &mut Conn,
    resource_type: i8,
    config: &str,
    resources: &[String],
) -> Result<Vec<Described>, NoReply> {
    let mut e = Enc::default();
    e.array(resources.len());
    for resource in resources {
        e.i8(resource_type)
            .compact_string(resource)
            .array(1)
            .compact_string(config)
            .tags();
    }
    e.bool(false).bool(false).tags();
    let body = conn.call(DESCRIBE_CONFIGS, 4, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        let n = d.array()?.unwrap_or(0);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let error = d.i16()?;
            let message = d.compact_nullable_string()?;
            let _type = d.i8()?;
            let resource = d.compact_string()?;
            let mut value = None;
            for _ in 0..d.array()?.unwrap_or(0) {
                let name = d.compact_string()?;
                let v = d.compact_nullable_string()?;
                let _read_only = d.bool()?;
                let source = d.i8()?;
                let _sensitive = d.bool()?;
                for _ in 0..d.array()?.unwrap_or(0) {
                    d.compact_string()?;
                    d.compact_nullable_string()?;
                    d.i8()?;
                    d.tags()?;
                }
                let _config_type = d.i8()?;
                let _doc = d.compact_nullable_string()?;
                d.tags()?;
                // DEFAULT_CONFIG (5) is the static default, not a stored value.
                if name == config && source != 5 {
                    value = v;
                }
            }
            d.tags()?;
            out.push(Described {
                resource,
                error,
                message,
                value,
            });
        }
        Ok(out)
    })
}

/// CreateTopics v7 of one topic with one partition; `validate_only` checks
/// existence through the controller without creating it.
pub fn create_topic(
    conn: &mut Conn,
    name: &str,
    replication: i16,
    timeout_ms: i32,
    validate_only: bool,
) -> Result<(i16, Option<String>, [u8; 16]), NoReply> {
    let mut e = Enc::default();
    e.array(1)
        .compact_string(name)
        .i32(1)
        .i16(replication)
        .array(0)
        .array(0)
        .tags();
    e.i32(timeout_ms).bool(validate_only).tags();
    let body = conn.call(CREATE_TOPICS, 7, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        let n = d.array()?.unwrap_or(0);
        if n != 1 {
            return Err(format!("{n} topic results"));
        }
        let _name = d.compact_string()?;
        let id = d.uuid()?;
        let code = d.i16()?;
        let message = d.compact_nullable_string()?;
        Ok((code, message, id))
    })
}

/// DeleteTopics v6 of one topic by name.
pub fn delete_topic(
    conn: &mut Conn,
    name: &str,
    timeout_ms: i32,
) -> Result<(i16, Option<String>), NoReply> {
    let mut e = Enc::default();
    e.array(1)
        .compact_nullable_string(Some(name))
        .uuid([0; 16])
        .tags();
    e.i32(timeout_ms).tags();
    let body = conn.call(DELETE_TOPICS, 6, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        let n = d.array()?.unwrap_or(0);
        if n != 1 {
            return Err(format!("{n} delete results"));
        }
        let _name = d.compact_nullable_string()?;
        let _id = d.uuid()?;
        let code = d.i16()?;
        let message = d.compact_nullable_string()?;
        Ok((code, message))
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Replica {
    pub id: i32,
    pub directory: String,
    pub end_offset: i64,
}

/// A node's view of the metadata quorum (DescribeQuorum v2).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Quorum {
    pub error: i16,
    pub leader: i32,
    pub epoch: i32,
    pub high_watermark: i64,
    pub voters: Vec<Replica>,
    pub observers: Vec<Replica>,
    /// node id -> (host, port) of its CONTROLLER listener
    pub endpoints: Vec<(i32, String, u16)>,
}

/// A directory ID in Kafka's URL-safe unpadded base64.
pub fn uuid_text(bytes: [u8; 16]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut bits = 0u32;
    let mut count = 0;
    for byte in bytes {
        bits = (bits << 8) | u32::from(byte);
        count += 8;
        while count >= 6 {
            count -= 6;
            out.push(ALPHABET[((bits >> count) & 63) as usize] as char);
        }
    }
    if count > 0 {
        out.push(ALPHABET[((bits << (6 - count)) & 63) as usize] as char);
    }
    out
}

pub fn uuid_bytes(text: &str) -> Option<[u8; 16]> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in text.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        bits = (bits << 6) | v;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out.try_into().ok()
}

pub fn describe_quorum(conn: &mut Conn) -> Result<Quorum, NoReply> {
    let mut e = Enc::default();
    e.array(1)
        .compact_string("__cluster_metadata")
        .array(1)
        .i32(0)
        .tags()
        .tags();
    e.tags();
    let body = conn.call(DESCRIBE_QUORUM, 2, &e.0)?;
    decode(&conn.node, &body, |d| {
        let error = d.i16()?;
        let _message = d.compact_nullable_string()?;
        let mut quorum = Quorum {
            error,
            leader: -1,
            epoch: -1,
            high_watermark: -1,
            voters: vec![],
            observers: vec![],
            endpoints: vec![],
        };
        let replicas = |d: &mut Dec| -> DecResult<Vec<Replica>> {
            let mut out = Vec::new();
            for _ in 0..d.array()?.unwrap_or(0) {
                let id = d.i32()?;
                let directory = uuid_text(d.uuid()?);
                let end_offset = d.i64()?;
                let _last_fetch = d.i64()?;
                let _caught_up = d.i64()?;
                d.tags()?;
                out.push(Replica {
                    id,
                    directory,
                    end_offset,
                });
            }
            Ok(out)
        };
        for _ in 0..d.array()?.unwrap_or(0) {
            let _topic = d.compact_string()?;
            for _ in 0..d.array()?.unwrap_or(0) {
                let _index = d.i32()?;
                let code = d.i16()?;
                let _message = d.compact_nullable_string()?;
                if code != 0 && quorum.error == 0 {
                    quorum.error = code;
                }
                quorum.leader = d.i32()?;
                quorum.epoch = d.i32()?;
                quorum.high_watermark = d.i64()?;
                quorum.voters = replicas(d)?;
                quorum.observers = replicas(d)?;
                d.tags()?;
            }
            d.tags()?;
        }
        for _ in 0..d.array()?.unwrap_or(0) {
            let id = d.i32()?;
            for _ in 0..d.array()?.unwrap_or(0) {
                let name = d.compact_string()?;
                let host = d.compact_string()?;
                let port = d.u16()?;
                d.tags()?;
                if name == "CONTROLLER" {
                    quorum.endpoints.push((id, host, port));
                }
            }
            d.tags()?;
        }
        d.tags()?;
        Ok(quorum)
    })
}

/// InitProducerId v5 for an idempotent (non-transactional) producer: a broker
/// hands out an ID from a block the active controller allocated to it.
pub fn init_producer_id(conn: &mut Conn) -> Result<(i16, i64, i16), NoReply> {
    let mut e = Enc::default();
    e.compact_nullable_string(None).i32(60_000);
    e.0.extend_from_slice(&(-1i64).to_be_bytes());
    e.i16(-1).tags();
    let body = conn.call(INIT_PRODUCER_ID, 5, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        let code = d.i16()?;
        let id = d.i64()?;
        let epoch = d.i16()?;
        Ok((code, id, epoch))
    })
}

/// DescribeCluster v1 sent to a controller (endpoint type 2): the active
/// controller as this node knows it, and the controllers' endpoints.
/// (error code, active controller ID, controllers as (ID, host, port)).
pub type ActiveController = (i16, i32, Vec<(i32, String, i32)>);

pub fn active_controller(conn: &mut Conn) -> Result<ActiveController, NoReply> {
    let mut e = Enc::default();
    e.bool(false).i8(2).tags();
    let body = conn.call(DESCRIBE_CLUSTER, 1, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        let code = d.i16()?;
        let _message = d.compact_nullable_string()?;
        let _endpoint = d.i8()?;
        let _cluster = d.compact_string()?;
        let controller = d.i32()?;
        let mut nodes = Vec::new();
        for _ in 0..d.array()?.unwrap_or(0) {
            let id = d.i32()?;
            let host = d.compact_string()?;
            let port = d.i32()?;
            let _rack = d.compact_nullable_string()?;
            d.tags()?;
            nodes.push((id, host, port));
        }
        Ok((code, controller, nodes))
    })
}

/// AddRaftVoter v0 (acknowledged when committed).
pub fn add_voter(
    conn: &mut Conn,
    id: i32,
    directory: [u8; 16],
    host: &str,
    port: u16,
    timeout_ms: i32,
) -> Result<(i16, Option<String>), NoReply> {
    let mut e = Enc::default();
    e.compact_nullable_string(None)
        .i32(timeout_ms)
        .i32(id)
        .uuid(directory);
    e.array(1)
        .compact_string("CONTROLLER")
        .compact_string(host)
        .u16(port)
        .tags();
    e.tags();
    let body = conn.call(ADD_RAFT_VOTER, 0, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        Ok((d.i16()?, d.compact_nullable_string()?))
    })
}

/// RemoveRaftVoter v0.
pub fn remove_voter(
    conn: &mut Conn,
    id: i32,
    directory: [u8; 16],
) -> Result<(i16, Option<String>), NoReply> {
    let mut e = Enc::default();
    e.compact_nullable_string(None)
        .i32(id)
        .uuid(directory)
        .tags();
    let body = conn.call(REMOVE_RAFT_VOTER, 0, &e.0)?;
    decode(&conn.node, &body, |d| {
        let _throttle = d.i32()?;
        Ok((d.i16()?, d.compact_nullable_string()?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64] {
            let mut e = Enc::default();
            e.uvarint(v);
            assert_eq!(Dec::new(&e.0).uvarint().unwrap(), v);
        }
    }

    #[test]
    fn directory_ids_round_trip_in_kafka_base64() {
        let bytes = [7u8; 16];
        let text = uuid_text(bytes);
        assert_eq!(text.len(), 22);
        assert_eq!(uuid_bytes(&text), Some(bytes));
        // Kafka's Uuid.toString of 0x0000...0001
        let mut one = [0u8; 16];
        one[15] = 1;
        assert_eq!(uuid_text(one), "AAAAAAAAAAAAAAAAAAAAAQ");
    }
}
