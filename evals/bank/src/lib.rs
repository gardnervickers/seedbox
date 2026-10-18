//! A small bank: accounts, transfers and a total. The server's transfer has a
//! planted lost-update bug that only shows when two transfers interleave.
pub const ACCOUNTS: usize = 8;
pub const INITIAL_BALANCE: i64 = 100;
pub const PORT: u16 = 7000;
