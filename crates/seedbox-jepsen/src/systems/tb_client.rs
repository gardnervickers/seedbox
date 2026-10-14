//! TigerBeetle's C client (`tb_client`, release 0.16.11, and on Linux also
//! 0.16.17 for the eviction control) from Rust.
//!
//! The library is the static `libtb_client.a` TigerBeetle ships in its Go
//! client module for each release (see `tools::tb_client_lib`); `build.rs`
//! links it and sets `cfg(tb_client)`. Without it, [`Client::connect`] fails,
//! so the checkers and host tools build anywhere.
//!
//! The data types mirror `tb_client.h` of 0.16.11 exactly (`#[repr(C)]`, with
//! Rust's `u128` aligned to 16 bytes like C's `__uint128_t`). A 0.16.11 client
//! may talk to any server release from 0.16.11 on; a newer client is evicted
//! by an older server.
//!
//! [`Client::submit`] hands a packet to the client's own thread and returns a
//! channel for its completion. TigerBeetle retries a request until it
//! completes, so a caller that stops waiting leaves it in flight: the packet
//! and its data stay allocated until the completion arrives, if ever.
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    sync::mpsc::{self, Receiver, Sender},
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: u128,
    pub debits_pending: u128,
    pub debits_posted: u128,
    pub credits_pending: u128,
    pub credits_posted: u128,
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub reserved: u32,
    pub ledger: u32,
    pub code: u16,
    pub flags: u16,
    pub timestamp: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: u128,
    pub debit_account_id: u128,
    pub credit_account_id: u128,
    pub amount: u128,
    pub pending_id: u128,
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub timeout: u32,
    pub ledger: u32,
    pub code: u16,
    pub flags: u16,
    pub timestamp: u64,
}

/// One failed event of a create batch: its index and result code.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResult {
    pub index: u32,
    pub result: u32,
}

/// `get_account_transfers` / `get_account_balances`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountFilter {
    pub account_id: u128,
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub code: u16,
    pub reserved: [u8; 58],
    pub timestamp_min: u64,
    pub timestamp_max: u64,
    pub limit: u32,
    pub flags: u32,
}

impl Default for AccountFilter {
    fn default() -> Self {
        Self {
            account_id: 0,
            user_data_128: 0,
            user_data_64: 0,
            user_data_32: 0,
            code: 0,
            reserved: [0; 58],
            timestamp_min: 0,
            timestamp_max: 0,
            limit: 0,
            flags: 0,
        }
    }
}

/// `query_accounts` / `query_transfers`. A zero field matches anything.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryFilter {
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub ledger: u32,
    pub code: u16,
    pub reserved: [u8; 6],
    pub timestamp_min: u64,
    pub timestamp_max: u64,
    pub limit: u32,
    pub flags: u32,
}

pub const ACCOUNT_FILTER_DEBITS: u32 = 1;
pub const ACCOUNT_FILTER_CREDITS: u32 = 2;
pub const FILTER_REVERSED_ACCOUNT: u32 = 4;
pub const QUERY_FILTER_REVERSED: u32 = 1;

pub const TRANSFER_LINKED: u16 = 1;

#[repr(C)]
struct Packet {
    next: *mut Packet,
    user_data: *mut c_void,
    operation: u8,
    status: u8,
    data_size: u32,
    data: *mut c_void,
    batch_next: *mut Packet,
    batch_tail: *mut Packet,
    batch_size: u32,
    batch_allowed: u8,
    reserved: [u8; 7],
}

const _: () = {
    assert!(std::mem::size_of::<Account>() == 128);
    assert!(std::mem::size_of::<Transfer>() == 128);
    assert!(std::mem::size_of::<AccountFilter>() == 128);
    assert!(std::mem::size_of::<QueryFilter>() == 64);
    assert!(std::mem::size_of::<CreateResult>() == 8);
};

/// `TB_OPERATION`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    CreateAccounts = 129,
    CreateTransfers = 130,
    LookupAccounts = 131,
    LookupTransfers = 132,
    GetAccountTransfers = 133,
    GetAccountBalances = 134,
    QueryAccounts = 135,
    QueryTransfers = 136,
}

/// A client library release this build can link.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Release {
    /// 0.16.11, the release under test: it panics when its session is evicted.
    #[value(name = "0.16.11")]
    V0_16_11,
    /// 0.16.17: an evicted session's requests fail with `client_evicted`
    /// (tigerbeetle#2484, fixed in 0.16.13). Linux builds only.
    #[value(name = "0.16.17")]
    V0_16_17,
}

impl Release {
    pub fn name(self) -> &'static str {
        match self {
            Self::V0_16_11 => "0.16.11",
            Self::V0_16_17 => "0.16.17",
        }
    }

    /// `TB_PACKET_STATUS` names.
    pub fn packet_status(self, status: u8) -> &'static str {
        match (self, status) {
            (_, 0) => "ok",
            (_, 1) => "too_much_data",
            (Self::V0_16_11, 2) => "client_shutdown",
            (Self::V0_16_11, 3) => "invalid_operation",
            (Self::V0_16_11, 4) => "invalid_data_size",
            (Self::V0_16_17, 2) => "client_evicted",
            (Self::V0_16_17, 3) => "client_shutdown",
            (Self::V0_16_17, 4) => "invalid_operation",
            (Self::V0_16_17, 5) => "invalid_data_size",
            _ => "unknown",
        }
    }
}

/// A completed packet: its status and the reply body.
pub type Completion = (u8, Vec<u8>);

struct Pending {
    packet: Packet,
    data: Vec<u8>,
    done: Sender<Completion>,
}

/// One client session to a cluster.
pub struct Client {
    raw: usize,
    pub release: Release,
}

// SAFETY: tb_client's submit is thread-safe; completions run on its own thread.
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

#[cfg(tb_client)]
mod ffi {
    use super::Packet;
    #[link(name = "tb_client", kind = "static")]
    extern "C" {
        pub fn tb_client_init(
            out_client: *mut usize,
            cluster_id: u128,
            address_ptr: *const u8,
            address_len: u32,
            on_completion_ctx: usize,
            on_completion: extern "C" fn(usize, usize, *mut Packet, *const u8, u32),
        ) -> i32;
        pub fn tb_client_submit(client: usize, packet: *mut Packet);
    }
}

/// 0.16.17's client, exports renamed (see `tools::tb_client_lib`): the
/// cluster id is passed by pointer and completions carry a timestamp.
#[cfg(tb_client_0_16_17)]
mod ffi17 {
    use super::Packet;
    #[link(name = "tb_client_0_16_17", kind = "static")]
    extern "C" {
        pub fn tb17_client_init(
            out_client: *mut usize,
            cluster_id: *const u8,
            address_ptr: *const u8,
            address_len: u32,
            on_completion_ctx: usize,
            on_completion: extern "C" fn(usize, usize, *mut Packet, u64, *const u8, u32),
        ) -> i32;
        pub fn tb17_client_submit(client: usize, packet: *mut Packet);
    }
}

#[cfg_attr(not(tb_client_0_16_17), allow(dead_code))]
extern "C" fn on_completion17(
    ctx: usize,
    client: usize,
    packet: *mut Packet,
    _timestamp: u64,
    data: *const u8,
    len: u32,
) {
    on_completion(ctx, client, packet, data, len)
}

#[cfg_attr(not(tb_client), allow(dead_code))]
extern "C" fn on_completion(
    _ctx: usize,
    _client: usize,
    packet: *mut Packet,
    data: *const u8,
    len: u32,
) {
    // SAFETY: `user_data` is the boxed Pending this packet lives in, handed to
    // the client by `submit`; the client is done with the packet now.
    unsafe {
        let pending = Box::from_raw((*packet).user_data as *mut Pending);
        let bytes = if data.is_null() || len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, len as usize).to_vec()
        };
        let _ = pending.done.send(((*packet).status, bytes));
    }
}

/// `TB_STATUS` names for a failed init.
fn init_status(status: i32) -> &'static str {
    match status {
        1 => "unexpected",
        2 => "out_of_memory",
        3 => "address_invalid",
        4 => "address_limit_exceeded",
        5 => "system_resources",
        6 => "network_subsystem",
        _ => "unknown",
    }
}

impl Client {
    /// Start a client for `cluster` at `addresses` (`ip:port,...`, replica order).
    pub fn connect(release: Release, cluster: u128, addresses: &str) -> Result<Self, String> {
        if release == Release::V0_16_17 {
            #[cfg(tb_client_0_16_17)]
            {
                let mut raw = 0usize;
                let id = cluster.to_le_bytes();
                // SAFETY: the id and address bytes outlive the call.
                let status = unsafe {
                    ffi17::tb17_client_init(
                        &mut raw,
                        id.as_ptr(),
                        addresses.as_ptr(),
                        addresses.len() as u32,
                        0,
                        on_completion17,
                    )
                };
                if status != 0 {
                    return Err(format!("tb_client_init 0.16.17: {}", init_status(status)));
                }
                return Ok(Self { raw, release });
            }
            #[cfg(not(tb_client_0_16_17))]
            return Err("this build has no 0.16.17 TigerBeetle client library".into());
        }
        #[cfg(tb_client)]
        {
            let mut raw = 0usize;
            // SAFETY: the address bytes outlive the call; `raw` receives the handle.
            let status = unsafe {
                ffi::tb_client_init(
                    &mut raw,
                    cluster,
                    addresses.as_ptr(),
                    addresses.len() as u32,
                    0,
                    on_completion,
                )
            };
            if status != 0 {
                return Err(format!("tb_client_init: {}", init_status(status)));
            }
            Ok(Self { raw, release })
        }
        #[cfg(not(tb_client))]
        {
            let _ = (cluster, addresses, init_status(0));
            Err(
                "this build has no TigerBeetle client library (run seedbox-jepsen fetch-tb-client)"
                    .into(),
            )
        }
    }

    /// Submit one request; the receiver yields its completion.
    pub fn submit(&self, operation: Operation, data: Vec<u8>) -> Receiver<Completion> {
        let (done, completion) = mpsc::channel();
        let mut pending = Box::new(Pending {
            packet: Packet {
                next: std::ptr::null_mut(),
                user_data: std::ptr::null_mut(),
                operation: operation as u8,
                status: 0,
                data_size: data.len() as u32,
                data: std::ptr::null_mut(),
                batch_next: std::ptr::null_mut(),
                batch_tail: std::ptr::null_mut(),
                batch_size: 0,
                batch_allowed: 0,
                reserved: [0; 7],
            },
            data,
            done,
        });
        pending.packet.data = pending.data.as_mut_ptr() as *mut c_void;
        let pending = Box::into_raw(pending);
        // SAFETY: the Box stays alive until on_completion reclaims it.
        unsafe {
            (*pending).packet.user_data = pending as *mut c_void;
            #[cfg(tb_client_0_16_17)]
            if self.release == Release::V0_16_17 {
                ffi17::tb17_client_submit(self.raw, &mut (*pending).packet);
                return completion;
            }
            #[cfg(tb_client)]
            ffi::tb_client_submit(self.raw, &mut (*pending).packet);
            #[cfg(not(tb_client))]
            {
                let _ = self.raw;
                on_completion(0, 0, &mut (*pending).packet, std::ptr::null(), 0);
            }
        }
        completion
    }
}

/// The bytes of a slice of plain `#[repr(C)]` values.
pub fn bytes_of<T: Copy>(items: &[T]) -> Vec<u8> {
    let len = std::mem::size_of_val(items);
    // SAFETY: T is a plain repr(C) type without padding-sensitive invariants.
    unsafe { std::slice::from_raw_parts(items.as_ptr() as *const u8, len).to_vec() }
}

/// Values of `T` read back from a reply body.
pub fn values_of<T: Copy + Default>(bytes: &[u8]) -> Vec<T> {
    let size = std::mem::size_of::<T>();
    bytes
        .chunks_exact(size)
        .map(|chunk| {
            let mut value = T::default();
            // SAFETY: chunk is exactly size_of::<T>() bytes of a repr(C) value.
            unsafe {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), &mut value as *mut T as *mut u8, size)
            };
            value
        })
        .collect()
}

/// `TB_CREATE_TRANSFER_RESULT` / `TB_CREATE_ACCOUNT_RESULT` codes this oracle names.
pub fn create_result_name(transfer: bool, code: u32) -> String {
    let name = match (transfer, code) {
        (_, 0) => "ok",
        (_, 1) => "linked_event_failed",
        (_, 2) => "linked_event_chain_open",
        (false, 21) => "exists",
        (true, 46) => "exists",
        (true, 21) => "debit_account_not_found",
        (true, 22) => "credit_account_not_found",
        (true, 54) => "exceeds_credits",
        (true, 55) => "exceeds_debits",
        (true, 68) => "id_already_failed",
        _ => return format!("code-{code}"),
    };
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_through_bytes() {
        let transfers = [
            Transfer {
                id: 7,
                amount: 3,
                ledger: 1,
                code: 2,
                ..Transfer::default()
            },
            Transfer {
                id: u128::MAX - 1,
                timestamp: 99,
                ..Transfer::default()
            },
        ];
        let bytes = bytes_of(&transfers);
        assert_eq!(bytes.len(), 256);
        assert_eq!(values_of::<Transfer>(&bytes), transfers);
        // The id is the first 16 bytes, little-endian.
        assert_eq!(bytes[0], 7);
    }
}
