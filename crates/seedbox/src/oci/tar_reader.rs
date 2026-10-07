//! Bound tar metadata before tar-rs can allocate extension payloads.
//! PAX size overrides are tracked across GNU name/link headers so this reader
//! and the ordinary tar iterator agree on every next-header offset.

use crate::Result;
use std::io::{self, Read};

const BLOCK: u64 = 512;
const METADATA_LIMIT: u64 = 1024 * 1024;

pub(crate) struct TarReader<'a, R> {
    input: R,
    check: &'a dyn Fn() -> Result<()>,
    remaining: u64,
    buffer: Vec<u8>,
    offset: usize,
    payload: u64,
    pax: Option<Option<u64>>,
    tail: bool,
}

impl<'a, R: Read> TarReader<'a, R> {
    pub fn new(input: R, limit: u64, check: &'a dyn Fn() -> Result<()>) -> Self {
        Self {
            input,
            check,
            remaining: limit,
            buffer: Vec::new(),
            offset: 0,
            payload: 0,
            pax: None,
            tail: false,
        }
    }

    fn underlying(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (self.check)().map_err(io::Error::other)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0];
            return if self.input.read(&mut probe)? == 0 {
                Ok(0)
            } else {
                Err(io::Error::other("tar input exceeds import limit"))
            };
        }
        let length = bytes.len().min(64 * 1024).min(self.remaining as usize);
        let result = self.input.read(&mut bytes[..length]);
        (self.check)().map_err(io::Error::other)?;
        let count = result?;
        self.remaining -= count as u64;
        Ok(count)
    }

    fn exact(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset != bytes.len() {
            let count = self.underlying(&mut bytes[offset..])?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated tar payload or padding",
                ));
            }
            offset += count;
        }
        Ok(())
    }

    fn header(&mut self) -> io::Result<bool> {
        let mut bytes = [0; BLOCK as usize];
        let mut offset = 0;
        while offset != bytes.len() {
            let count = self.underlying(&mut bytes[offset..])?;
            if count == 0 {
                return if offset == 0 {
                    Ok(false)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated tar header",
                    ))
                };
            }
            offset += count;
        }
        self.buffer.clear();
        self.offset = 0;
        if bytes.iter().all(|byte| *byte == 0) {
            self.buffer.extend_from_slice(&bytes);
            self.tail = true;
            return Ok(true);
        }
        let header = tar::Header::from_byte_slice(&bytes);
        let sum = bytes[..148]
            .iter()
            .chain(&bytes[156..])
            .map(|byte| u32::from(*byte))
            .sum::<u32>()
            + 8 * 32;
        if sum != header.cksum()? {
            return Err(io::Error::other("tar header checksum mismatch"));
        }
        let kind = header.entry_type();
        if kind.is_gnu_sparse() || kind.is_pax_global_extensions() {
            return Err(io::Error::other(
                "unsupported sparse or global PAX image metadata",
            ));
        }
        let size = header.entry_size()?;
        let extension =
            kind.is_gnu_longname() || kind.is_gnu_longlink() || kind.is_pax_local_extensions();
        if extension {
            if size > METADATA_LIMIT {
                return Err(io::Error::other("tar metadata exceeds 1 MiB"));
            }
            let aligned = aligned(size)?;
            let mut payload = vec![0; aligned as usize];
            self.exact(&mut payload)?;
            if kind.is_pax_local_extensions() {
                if self.pax.is_some() {
                    return Err(io::Error::other("duplicate local PAX header"));
                }
                let mut pax_size = None;
                for extension in tar::PaxExtensions::new(&payload[..size as usize]) {
                    let extension = extension?;
                    if extension.key_bytes() == b"size" {
                        if pax_size.is_some() {
                            return Err(io::Error::other("duplicate PAX size"));
                        }
                        let value = extension.value().map_err(io::Error::other)?;
                        let value = value
                            .parse::<u64>()
                            .map_err(|_| io::Error::other("invalid PAX size"))?;
                        pax_size = Some(value);
                    }
                }
                self.pax = Some(pax_size);
            }
            self.buffer.extend_from_slice(&bytes);
            self.buffer.extend_from_slice(&payload);
        } else {
            let size = self.pax.take().flatten().unwrap_or(size);
            self.payload = aligned(size)?;
            if self.payload > self.remaining {
                return Err(io::Error::other("tar payload exceeds import limit"));
            }
            self.buffer.extend_from_slice(&bytes);
        }
        Ok(true)
    }
}

impl<R: Read> Read for TarReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        (self.check)().map_err(io::Error::other)?;
        if self.offset < self.buffer.len() {
            let count = output.len().min(self.buffer.len() - self.offset);
            output[..count].copy_from_slice(&self.buffer[self.offset..self.offset + count]);
            self.offset += count;
            return Ok(count);
        }
        if self.tail {
            return self.underlying(output);
        }
        if self.payload > 0 {
            let length = output.len().min(self.payload as usize);
            let count = self.underlying(&mut output[..length])?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated tar payload or padding",
                ));
            }
            self.payload -= count as u64;
            return Ok(count);
        }
        if !self.header()? {
            return Ok(0);
        }
        self.read(output)
    }
}

fn aligned(size: u64) -> io::Result<u64> {
    size.checked_add(BLOCK - 1)
        .map(|size| size & !(BLOCK - 1))
        .ok_or_else(|| io::Error::other("tar size overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{execution::CancellationToken, Error};

    fn header(kind: u8, name: &str, size: u64) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_entry_type(tar::EntryType::new(kind));
        header.set_size(size);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        header.as_bytes().to_vec()
    }

    fn append(bytes: &mut Vec<u8>, kind: u8, name: &str, payload: &[u8], declared: u64) {
        bytes.extend(header(kind, name, declared));
        bytes.extend(payload);
        bytes.resize(bytes.len() + ((512 - payload.len() % 512) % 512), 0);
    }

    fn pax(key: &str, value: &str) -> Vec<u8> {
        let rest = format!(" {key}={value}\n");
        let mut length = rest.len() + 1;
        loop {
            let text = format!("{length}{rest}");
            if text.len() == length {
                return text.into_bytes();
            }
            length = text.len();
        }
    }

    fn entries(bytes: &[u8]) -> io::Result<Vec<(String, Vec<u8>)>> {
        let check = || Ok(());
        let reader = TarReader::new(bytes, 8 * 1024 * 1024, &check);
        let mut archive = tar::Archive::new(reader);
        let mut result = Vec::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let name = entry.path()?.display().to_string();
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            result.push((name, data));
        }
        Ok(result)
    }

    #[test]
    fn extension_caps_apply_before_missing_payload_is_read() {
        for kind in *b"LKx" {
            let bytes = header(kind, "metadata", 1024 * 1024 * 1024);
            let error = entries(&bytes).unwrap_err();
            assert!(
                error.to_string().contains("metadata exceeds 1 MiB"),
                "{error}"
            );
        }
        for kind in *b"gS" {
            let bytes = header(kind, "unsupported", 1024 * 1024 * 1024);
            let error = entries(&bytes).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("unsupported sparse or global PAX"),
                "{error}"
            );
        }
    }

    #[test]
    fn pax_size_and_gnu_name_keep_following_headers_aligned() {
        for long_name in [false, true] {
            let mut bytes = Vec::new();
            let pax = pax("size", "5");
            append(&mut bytes, b'x', "metadata", &pax, pax.len() as u64);
            if long_name {
                append(&mut bytes, b'L', "long-name", b"long-file-name\0", 15);
            }
            append(&mut bytes, b'0', "first", b"hello", 0);
            append(&mut bytes, b'0', "second", b"tail", 4);
            bytes.extend([0; 1024]);
            let actual = entries(&bytes).unwrap();
            assert_eq!(
                actual,
                vec![
                    (
                        (if long_name { "long-file-name" } else { "first" }).into(),
                        b"hello".to_vec()
                    ),
                    ("second".into(), b"tail".to_vec()),
                ]
            );
        }
    }

    #[test]
    fn invalid_and_duplicate_pax_sizes_fail_explicitly() {
        for payload in [
            pax("size", "invalid"),
            [pax("size", "1"), pax("size", "2")].concat(),
        ] {
            let mut bytes = Vec::new();
            append(&mut bytes, b'x', "metadata", &payload, payload.len() as u64);
            let error = entries(&bytes).unwrap_err();
            assert!(error.to_string().contains("PAX size"), "{error}");
        }
    }

    #[test]
    fn directory_payloads_are_counted_and_truncation_is_rejected() {
        let check = || Ok(());
        let bytes = header(b'5', "directory", 4096);
        let reader = TarReader::new(bytes.as_slice(), 1024, &check);
        let mut archive = tar::Archive::new(reader);
        let error = archive
            .entries()
            .unwrap()
            .next()
            .unwrap()
            .err()
            .expect("oversized directory must fail");
        assert!(
            error.to_string().contains("payload exceeds import limit"),
            "{error}"
        );
        for bytes in [
            header(b'0', "file", 10),
            [header(b'0', "file", 10), b"hello".to_vec()].concat(),
        ] {
            let error = entries(&bytes).unwrap_err();
            assert!(error.to_string().contains("truncated"), "{error}");
        }
    }

    #[test]
    fn cancellation_interrupts_metadata_and_payload_padding() {
        struct CancelRead<'a> {
            input: &'a [u8],
            token: CancellationToken,
            remaining: usize,
        }
        impl Read for CancelRead<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = self.input.read(output)?;
                self.remaining = self.remaining.saturating_sub(count);
                if self.remaining == 0 {
                    self.token.cancel();
                }
                Ok(count)
            }
        }
        for (kind, size, cancel_after) in [(b'x', 128 * 1024, 65 * 1024), (b'0', 1, 514)] {
            let mut bytes = header(kind, "input", size);
            bytes.resize(bytes.len() + size as usize + 511, 0);
            let token = CancellationToken::default();
            let input = CancelRead {
                input: &bytes,
                token: token.clone(),
                remaining: cancel_after,
            };
            let check = || {
                if token.is_cancelled() {
                    Err(Error::PreparationCancelled)
                } else {
                    Ok(())
                }
            };
            let mut reader = TarReader::new(input, 1024 * 1024, &check);
            let error = io::copy(&mut reader, &mut io::sink()).unwrap_err();
            assert!(
                error.to_string().contains("preparation cancelled"),
                "{error}"
            );
        }
    }
}
