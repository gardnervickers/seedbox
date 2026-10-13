use super::{Error, MAX_PACKET};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

// Minimal client for QEMU's GDB remote serial protocol (RSP).
// https://sourceware.org/gdb/current/onlinedocs/gdb.html/Remote-Protocol.html
pub(super) struct Gdb(pub(super) UnixStream);

impl Gdb {
    pub(super) fn attach(socket: UnixStream) -> Result<Self, Error> {
        let mut gdb = Self(socket);
        // UnixStream::connect only queues a connection. QEMU's acceptance
        // callback calls vm_stop(PAUSED), even if seedbox-run has meanwhile
        // started the CPU. A read-only RSP reply proves that callback finished
        // before setup returns; no guest execution or timing retry is involved.
        let attached = gdb.request("qAttached")?;
        if attached != "1" {
            return Err(Error(format!(
                "unexpected GDB attachment response: {attached}"
            )));
        }
        Ok(gdb)
    }

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

#[test]
fn attachment_requires_a_read_only_protocol_acknowledgment() {
    for attached in *b"10" {
        let (client, mut server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(super::TIMEOUT)).unwrap();
        server.set_read_timeout(Some(super::TIMEOUT)).unwrap();
        let peer = std::thread::spawn(move || {
            let request = "qAttached";
            let checksum = request.bytes().fold(0u8, u8::wrapping_add);
            let expected = format!("${request}#{checksum:02x}");
            let mut packet = vec![0; expected.len()];
            server.read_exact(&mut packet).unwrap();
            assert_eq!(packet, expected.as_bytes());
            write!(server, "+${}#{attached:02x}", char::from(attached)).unwrap();
            let mut ack = [0];
            server.read_exact(&mut ack).unwrap();
            assert_eq!(ack, [b'+']);
        });
        let result = Gdb::attach(client);
        if attached == b'1' {
            assert!(result.is_ok());
        } else {
            assert_eq!(
                result.err().unwrap().to_string(),
                "unexpected GDB attachment response: 0"
            );
        }
        peer.join().unwrap();
    }
}
