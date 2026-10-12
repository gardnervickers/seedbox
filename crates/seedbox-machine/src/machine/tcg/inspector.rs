use super::{Error, MAX_PACKET};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

// Minimal client for QEMU's GDB remote serial protocol (RSP).
// https://sourceware.org/gdb/current/onlinedocs/gdb.html/Remote-Protocol.html
pub(super) struct Gdb(pub(super) UnixStream);

impl Gdb {
    fn byte(&mut self) -> Result<u8, Error> {
        let mut byte = [0];
        self.0.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    pub(super) fn request(&mut self, request: &str) -> Result<String, Error> {
        // RSP frames are `$PAYLOAD#CC`; CC is the payload byte sum modulo 256,
        // written as two hexadecimal digits. `+` acknowledges a valid packet.
        let checksum = request.bytes().fold(0u8, u8::wrapping_add);
        write!(self.0, "${request}#{checksum:02x}")?;
        let mut byte = self.byte()?;
        if byte == b'+' {
            byte = self.byte()?;
        }
        if byte != b'$' {
            return Err(Error(format!("unexpected GDB packet prefix {byte}")));
        }
        let mut payload = Vec::new();
        loop {
            let byte = self.byte()?;
            if byte == b'#' {
                break;
            }
            if payload.len() == MAX_PACKET {
                return Err(Error("oversized GDB response".into()));
            }
            payload.push(byte);
        }
        let checksum_bytes = [self.byte()?, self.byte()?];
        let expected =
            hex::decode(checksum_bytes).map_err(|_| Error("invalid GDB checksum".into()))?;
        if expected != [payload.iter().copied().fold(0u8, u8::wrapping_add)] {
            return Err(Error("GDB checksum mismatch".into()));
        }
        self.0.write_all(b"+")?;
        String::from_utf8(payload).map_err(|_| Error("non-text GDB response".into()))
    }
}

pub(super) fn decode_hex(value: &str) -> Result<Vec<u8>, Error> {
    hex::decode(value).map_err(|_| Error(format!("invalid GDB hex data: {value}")))
}
