//! Bounded binary transfers. Import is deliberately a single non-idempotent POST.
use super::{Client, Error};
use reqwest::{header, Method};
use seedbox_api::{
    BundleTransferQuery, Simulation, SimulationId, BUNDLE_CONTENT_TYPE, BUNDLE_MAX_BYTES,
};
use std::{future::Future, time::Duration};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub(crate) async fn cancellable<T>(
    cancelled: &impl Fn() -> bool,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::pin!(future);
    loop {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

fn validate_limit(max_bytes: u64) -> Result<(), Error> {
    if !(1..=BUNDLE_MAX_BYTES).contains(&max_bytes) {
        return Err(Error::Binary(
            "maxBytes must be between 1 and 68719476736".into(),
        ));
    }
    Ok(())
}

impl Client {
    /// Stream a completed simulation archive to an unpublished disk file.
    /// The caller owns cleanup and atomic destination publication.
    pub async fn export_bundle(
        &self,
        id: &SimulationId,
        output: &mut tokio::fs::File,
        max_bytes: u64,
        cancelled: impl Fn() -> bool,
    ) -> Result<u64, Error> {
        validate_limit(max_bytes)?;
        let request = self
            .request(Method::GET, &format!("/v1/simulations/{id}/bundle"))
            .query(&BundleTransferQuery { max_bytes });
        cancellable(&cancelled, async {
            let response = request.send().await?;
            self.binary(response, output, max_bytes, BUNDLE_CONTENT_TYPE, None)
                .await
        })
        .await
    }

    /// Upload one file once; cancellation/transport failure may leave an accepted
    /// import. Never retry automatically: every accepted upload creates a fresh ID.
    pub async fn import_bundle(
        &self,
        mut file: tokio::fs::File,
        max_bytes: u64,
        cancelled: impl Fn() -> bool,
    ) -> Result<Simulation, Error> {
        validate_limit(max_bytes)?;
        cancellable(&cancelled, async {
            let metadata = file.metadata().await?;
            if !metadata.is_file() {
                return Err(Error::Binary("import requires a regular file".into()));
            }
            if metadata.len() > max_bytes {
                return Err(Error::Binary("bundle exceeds wire byte limit".into()));
            }
            file.rewind().await?;
            // Spool a stable, bounded snapshot so changing input files cannot bypass
            // the limit or produce a successful truncated upload.
            let mut spool = tokio::fs::File::from_std(tempfile::tempfile()?);
            let mut total = 0u64;
            let mut bytes = vec![0; 64 * 1024];
            loop {
                let count = file.read(&mut bytes).await?;
                if count == 0 {
                    break;
                }
                total = total
                    .checked_add(count as u64)
                    .filter(|n| *n <= max_bytes)
                    .ok_or_else(|| Error::Binary("bundle exceeds wire byte limit".into()))?;
                spool.write_all(&bytes[..count]).await?;
            }
            spool.flush().await?;
            spool.rewind().await?;
            let stream = futures_util::stream::try_unfold(spool, |mut file| async move {
                let mut bytes = vec![0; 64 * 1024];
                let count = file.read(&mut bytes).await?;
                if count == 0 {
                    return Ok::<_, std::io::Error>(None);
                }
                bytes.truncate(count);
                Ok(Some((bytes, file)))
            });
            let response = self
                .request(Method::POST, "/v1/bundles")
                .query(&BundleTransferQuery { max_bytes })
                .header(header::CONTENT_TYPE, BUNDLE_CONTENT_TYPE)
                .header(header::CONTENT_LENGTH, total)
                .body(reqwest::Body::wrap_stream(stream))
                .send()
                .await
                .map_err(|error| Error::ImportUncertain(error.to_string()))?;
            self.decode(response).await.map_err(|error| match error {
                Error::Rejected { .. } => error,
                other => Error::ImportUncertain(other.to_string()),
            })
        })
        .await
    }

    pub async fn diagnostics(
        &self,
        id: &SimulationId,
        cancelled: impl Fn() -> bool,
    ) -> Result<seedbox_api::DiagnosticFiles, Error> {
        cancellable(
            &cancelled,
            self.document(self.request(Method::GET, &format!("/v1/simulations/{id}/diagnostics"))),
        )
        .await
    }

    /// Download raw retained evidence into a caller-owned unpublished file.
    pub async fn diagnostic_file(
        &self,
        id: &SimulationId,
        file: &seedbox_api::DiagnosticFile,
        output: &mut tokio::fs::File,
        max_bytes: u64,
        cancelled: impl Fn() -> bool,
    ) -> Result<u64, Error> {
        validate_limit(max_bytes)?;
        cancellable(&cancelled, async {
            let response = self
                .request(
                    Method::GET,
                    &format!("/v1/simulations/{id}/diagnostics/file"),
                )
                .query(&seedbox_api::DiagnosticFileQuery {
                    name: file.name.clone(),
                    max_bytes,
                })
                .send()
                .await?;
            if response.status().is_success() && response.content_length() != Some(file.size_bytes)
            {
                return Err(Error::Binary(
                    "diagnostic size differs from metadata".into(),
                ));
            }
            self.binary(
                response,
                output,
                max_bytes,
                "application/octet-stream",
                Some(&file.sha256),
            )
            .await
        })
        .await
    }

    async fn binary(
        &self,
        mut response: reqwest::Response,
        output: &mut tokio::fs::File,
        max_bytes: u64,
        content_type: &str,
        digest: Option<&str>,
    ) -> Result<u64, Error> {
        if !response.status().is_success() {
            let _: Simulation = self.decode(response).await?;
            return Err(Error::Binary("unexpected successful error response".into()));
        }
        if response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some(content_type)
        {
            return Err(Error::Binary(format!("expected {content_type}")));
        }
        let length = response
            .content_length()
            .ok_or_else(|| Error::Binary("missing binary Content-Length".into()))?;
        if length > max_bytes {
            return Err(Error::Binary(
                "binary response exceeds wire byte limit".into(),
            ));
        }
        use sha2::{Digest, Sha256};
        let expected = if content_type == "application/octet-stream" {
            Some(
                response
                    .headers()
                    .get("x-seedbox-sha256")
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| {
                        v.len() == 64
                            && v.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                    .ok_or_else(|| Error::Binary("missing diagnostic SHA-256".into()))?
                    .to_owned(),
            )
        } else {
            None
        };
        if expected.as_deref() != digest {
            return Err(Error::Binary(
                "diagnostic digest differs from metadata".into(),
            ));
        }
        let mut hash = Sha256::new();
        let mut total = 0;
        while let Some(bytes) = response.chunk().await? {
            total += bytes.len() as u64;
            if total > max_bytes || total > length {
                return Err(Error::Binary(
                    "binary response exceeds declared limit".into(),
                ));
            }
            if expected.is_some() {
                hash.update(&bytes);
            }
            output.write_all(&bytes).await?;
        }
        if total != length {
            return Err(Error::Binary("truncated binary response".into()));
        }
        if expected.is_some_and(|expected| expected != hex::encode(hash.finalize())) {
            return Err(Error::Binary("diagnostic checksum mismatch".into()));
        }
        output.flush().await?;
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, Read, Write},
        os::unix::net::UnixListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    fn headers(stream: &mut std::os::unix::net::UnixStream) -> usize {
        let mut reader = std::io::BufReader::new(stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap();
                }
            }
        }
        // Consume the upload before simulating an uncertain post-acceptance drop.
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).unwrap();
        length
    }

    #[tokio::test]
    async fn download_streams_limits_content_type_truncation_and_digest_are_checked() {
        for response in [
            format!("HTTP/1.1 200 OK\r\nContent-Type: {BUNDLE_CONTENT_TYPE}\r\nContent-Length: 5\r\n\r\nhello"),
            format!("HTTP/1.1 200 OK\r\nContent-Type: {BUNDLE_CONTENT_TYPE}\r\nContent-Length: 6\r\n\r\nhello!"),
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello".into(),
            format!("HTTP/1.1 200 OK\r\nContent-Type: {BUNDLE_CONTENT_TYPE}\r\nContent-Length: 5\r\n\r\nhi"),
        ] {
            let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
            let socket = root.path().join("api.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let valid = response.ends_with("hello") && response.contains(BUNDLE_CONTENT_TYPE);
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap(); headers(&mut stream);
                stream.write_all(response.as_bytes()).unwrap();
            });
            let mut output = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
            let result = Client::connect_to(&socket).unwrap().export_bundle(&"k7m2p9qx".parse().unwrap(), &mut output, 5, || false).await;
            assert_eq!(result.is_ok(), valid, "{result:?}");
            if valid { output.rewind().await.unwrap(); let mut bytes = Vec::new(); output.read_to_end(&mut bytes).await.unwrap(); assert_eq!(bytes, b"hello"); }
            thread.join().unwrap();
        }
        let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let socket = root.path().join("api.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let digest = "0".repeat(64);
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 5\r\nx-seedbox-sha256: {digest}\r\n\r\nhello");
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            headers(&mut stream);
            stream.write_all(response.as_bytes()).unwrap();
        });
        let mut output = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let descriptor = seedbox_api::DiagnosticFile {
            name: "file".into(),
            sha256: digest,
            size_bytes: 5,
        };
        let error = Client::connect_to(&socket)
            .unwrap()
            .diagnostic_file(
                &"k7m2p9qx".parse().unwrap(),
                &descriptor,
                &mut output,
                5,
                || false,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn stalled_download_and_uncertain_import_are_cancellable_without_retry() {
        for import in [false, true] {
            let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
            let socket = root.path().join("api.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let flag = cancelled.clone();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                assert_eq!(headers(&mut stream), if import { 5 } else { 0 });
                if !import {
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {BUNDLE_CONTENT_TYPE}\r\nContent-Length: 10\r\n\r\nhello").unwrap();
                }
                flag.store(true, Ordering::Release);
                // Keep the connection open until client cancellation is observed.
                std::thread::sleep(Duration::from_millis(100));
                listener.set_nonblocking(true).unwrap();
                assert!(
                    listener.accept().is_err(),
                    "uncertain import must not retry"
                );
            });
            let client = Client::connect_to(&socket).unwrap();
            let mut file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
            let result = if import {
                file.write_all(b"hello").await.unwrap();
                file.rewind().await.unwrap();
                client
                    .import_bundle(file, 10, || cancelled.load(Ordering::Acquire))
                    .await
                    .map(|_| ())
            } else {
                client
                    .export_bundle(&"k7m2p9qx".parse().unwrap(), &mut file, 10, || {
                        cancelled.load(Ordering::Acquire)
                    })
                    .await
                    .map(|_| ())
            };
            assert!(matches!(result, Err(Error::Cancelled)));
            thread.join().unwrap();
        }
    }

    #[tokio::test]
    async fn stalled_diagnostics_metadata_headers_and_body_are_cancellable() {
        for stall_headers in [false, true] {
            let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
            let socket = root.path().join("api.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let flag = cancelled.clone();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                assert_eq!(headers(&mut stream), 0);
                if !stall_headers {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").unwrap();
                }
                flag.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(100));
            });
            let client = Client::connect_to(&socket).unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                client.diagnostics(&"k7m2p9qx".parse().unwrap(), || {
                    cancelled.load(Ordering::Acquire)
                }),
            )
            .await
            .unwrap();
            assert!(matches!(result, Err(Error::Cancelled)));
            thread.join().unwrap();
        }
        let client =
            Client::connect_to(std::path::Path::new("/nonexistent-seedbox-test.sock")).unwrap();
        assert!(matches!(
            client
                .diagnostics(&"k7m2p9qx".parse().unwrap(), || true)
                .await,
            Err(Error::Cancelled)
        ));
    }

    #[tokio::test]
    async fn lost_import_response_is_uncertain_and_never_retried() {
        let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let socket = root.path().join("api.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(headers(&mut stream), 5);
            drop(stream);
            std::thread::sleep(Duration::from_millis(100));
            listener.set_nonblocking(true).unwrap();
            assert!(listener.accept().is_err());
        });
        let client = Client::connect_to(&socket).unwrap();
        let mut file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        file.write_all(b"hello").await.unwrap();
        let error = client.import_bundle(file, 10, || false).await.unwrap_err();
        assert!(matches!(error, Error::ImportUncertain(_)), "{error}");
        assert!(error.to_string().contains("inspect simulations"));
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn upload_spool_limit_and_pre_cancel_fail_before_connecting() {
        let client =
            Client::connect_to(std::path::Path::new("/nonexistent-seedbox-test.sock")).unwrap();
        let mut file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        file.write_all(b"oversized").await.unwrap();
        file.rewind().await.unwrap();
        let error = client.import_bundle(file, 1, || false).await.unwrap_err();
        assert!(error.to_string().contains("wire byte limit"));
        let file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        assert!(matches!(
            client.import_bundle(file, 1, || true).await,
            Err(Error::Cancelled)
        ));
        let mut file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        assert!(matches!(
            client
                .export_bundle(&"k7m2p9qx".parse().unwrap(), &mut file, 0, || false)
                .await,
            Err(Error::Binary(_))
        ));
        assert!(matches!(
            client
                .export_bundle(
                    &"k7m2p9qx".parse().unwrap(),
                    &mut file,
                    BUNDLE_MAX_BYTES + 1,
                    || false
                )
                .await,
            Err(Error::Binary(_))
        ));
    }
}
