//! Static library root linking Seedbox's private disk store into QEMU.
//!
//! Built by `nix/tcg.nix` with the project's rustc as
//! `rustc --edition 2021 --crate-type staticlib -C panic=abort --cfg seedbox_disk_abi`.
//! It compiles the same store, request and overlay sources as the machine
//! crate; only `abi.rs` adds the C entry points (see `seedbox-disk.h`).
// The store's machine-side operations have no caller in this library.
#![allow(dead_code)]

#[path = "../../seedbox-machine/src/machine/disk.rs"]
mod disk;
