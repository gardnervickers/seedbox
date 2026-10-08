//! Lazy guest-kernel downloads. The CLI supplies HTTP configuration, credentials,
//! cache location, cancellation and progress; replay never calls this module.
use crate::{execution::CancellationToken, store::ContentDigest, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const RELEASE_ID: &str = env!("SEEDBOX_KERNEL_RELEASE");
const REPOSITORY: &str = "gardnervickers/seedbox";
const MAX_KERNEL: u64 = 128 << 20;
const MAX_METADATA: u64 = 1 << 20;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    id: u64,
    name: String,
    size: u64,
    digest: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedKernel {
    release: String,
    architecture: String,
    size: u64,
    sha256: ContentDigest,
}

pub struct KernelDownload {
    agent: ureq::Agent,
    authorization: Option<ureq::http::HeaderValue>,
    api: String,
}
impl KernelDownload {
    pub fn new(agent: ureq::Agent, token: Option<&str>) -> Result<Self> {
        let authorization = token.map(|token| {
            let mut value = ureq::http::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| Error::Setup("GH_TOKEN or GITHUB_TOKEN is not a valid HTTP credential; replace it with a GitHub token that can read gardnervickers/seedbox".into()))?;
            value.set_sensitive(true);
            Ok::<_, Error>(value)
        }).transpose()?;
        Ok(Self {
            agent,
            authorization,
            api: "https://api.github.com".into(),
        })
    }

    /// Lookup only: no HTTP client, credentials, cache state or lock needed.
    pub fn lookup(cache: &Path, architecture: &str) -> Result<Option<PathBuf>> {
        let root = cache.join(RELEASE_ID).join(architecture).join("ready");
        lookup_ready(&root, architecture)
    }

    pub fn cached(cache: &Path, architecture: &str) -> Result<PathBuf> {
        Self::lookup(cache, architecture)?.ok_or_else(|| Error::Setup(format!(
            "guest kernel is not cached at {}; run seedbox run to download it, or pass --kernel PATH",
            cache.join(RELEASE_ID).join(architecture).join("ready").display()
        )))
    }

    pub fn ensure(
        &self,
        cache: &Path,
        architecture: &str,
        cancellation: &CancellationToken,
        progress: impl Fn(&str),
    ) -> Result<PathBuf> {
        if !matches!(architecture, "aarch64" | "x86_64") {
            return Err(Error::Setup(format!(
                "no downloadable guest kernel for {architecture}; pass --kernel PATH"
            )));
        }
        let parent = cache.join(RELEASE_ID).join(architecture);
        self.ensure_in(&parent, architecture, cancellation, &progress).map_err(|error| match error {
            Error::Io(error) => Error::Setup(format!("kernel cache {}: {error}; check directory permissions or set XDG_CACHE_HOME to a writable directory", parent.display())),
            other => other,
        })
    }

    fn ensure_in(
        &self,
        parent: &Path,
        architecture: &str,
        cancellation: &CancellationToken,
        progress: &impl Fn(&str),
    ) -> Result<PathBuf> {
        use std::os::unix::fs::OpenOptionsExt;
        fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(parent.join("lock"))?;
        let mut waiting = false;
        loop {
            cancelled(cancellation)?;
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    if !waiting {
                        progress("waiting for another invocation to finish the kernel download");
                        waiting = true;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let ready = parent.join("ready");
        if let Some(path) = lookup_ready(&ready, architecture)? {
            progress(&format!("using cached guest kernel {}", path.display()));
            return Ok(path);
        }
        let tag = format!("kernel-{RELEASE_ID}");
        progress(&format!(
            "downloading {architecture} guest kernel from github.com/{REPOSITORY} ({tag})"
        ));
        let mut response = self.get(
            &format!("/repos/{REPOSITORY}/releases/tags/{tag}"),
            "application/vnd.github+json",
        )?;
        let metadata = read_limited(response.body_mut().as_reader(), MAX_METADATA, cancellation)
            .map_err(download_error)?;
        let release: Release = serde_json::from_slice(&metadata).map_err(|error| Error::Setup(format!("invalid kernel release metadata from GitHub: {error}; check the Guest kernels workflow")))?;
        if release.tag_name != tag || release.draft {
            return Err(Error::Setup(format!("kernel release {tag} is not published; check the Guest kernels workflow or pass --kernel PATH")));
        }
        let name = format!("seedbox-kernel-{architecture}");
        let mut assets = release.assets.iter().filter(|asset| asset.name == name);
        let asset = assets.next().ok_or_else(|| Error::Setup(format!("kernel release {tag} is missing {name}; check the Guest kernels workflow or pass --kernel PATH")))?;
        if assets.next().is_some() || asset.size == 0 || asset.size > MAX_KERNEL {
            return Err(Error::Integrity(
                "invalid kernel release asset size or duplicate name".into(),
            ));
        }
        let digest = asset.digest.as_deref().and_then(|s| s.strip_prefix("sha256:"))
            .ok_or_else(|| Error::Setup(format!("kernel release {tag} has no SHA-256 digest for {name}; rerun the Guest kernels workflow")))?;
        let expected = ContentDigest::try_from(digest.to_owned())?;
        let stage = tempfile::Builder::new()
            .prefix("download-")
            .tempdir_in(parent)?;
        let path = stage.path().join("kernel");
        let mut output = File::create(&path)?;
        cancelled(cancellation)?;
        let mut response = self.get(
            &format!("/repos/{REPOSITORY}/releases/assets/{}", asset.id),
            "application/octet-stream",
        )?;
        let (size, actual) =
            copy_kernel(response.body_mut().as_reader(), &mut output, cancellation)
                .map_err(download_error)?;
        if size != asset.size || actual != expected {
            return Err(Error::Integrity("downloaded guest kernel size or SHA-256 differs from the GitHub release asset; retry the download".into()));
        }
        output.sync_all()?;
        fs::write(
            stage.path().join("kernel.json"),
            serde_json::to_vec(&CachedKernel {
                release: RELEASE_ID.into(),
                architecture: architecture.into(),
                size,
                sha256: actual,
            })?,
        )?;
        cancelled(cancellation)?;
        fs::rename(stage.path(), &ready)?;
        let path = ready.join("kernel");
        progress(&format!("cached guest kernel at {}", path.display()));
        Ok(path)
    }

    fn get(&self, path: &str, accept: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let mut request = self
            .agent
            .get(format!("{}{path}", self.api))
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(value) = &self.authorization {
            request = request.header("Authorization", value.clone());
        }
        request.call().map_err(|error| match error {
            ureq::Error::StatusCode(code @ (401 | 403 | 404)) => Error::Setup(format!(
                "kernel download from github.com/{REPOSITORY} failed (HTTP {code}); this repository is private: set GH_TOKEN or GITHUB_TOKEN with read access, check that kernel-{RELEASE_ID} has been published by the Guest kernels workflow, or pass --kernel PATH"
            )),
            other => Error::Setup(format!("kernel download from GitHub failed: {other}; check your network connection and retry, or pass --kernel PATH")),
        })
    }
}

fn download_error(error: Error) -> Error {
    match error {
        Error::Io(error) => Error::Setup(format!("guest kernel transfer failed: {error}; check your network connection and available cache disk space, then retry")),
        other => other,
    }
}
fn cancelled(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(Error::PreparationCancelled)
    } else {
        Ok(())
    }
}
fn copy_kernel(
    mut input: impl Read,
    mut output: impl Write,
    token: &CancellationToken,
) -> Result<(u64, ContentDigest)> {
    let mut size = 0u64;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        cancelled(token)?;
        let n = input.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > MAX_KERNEL {
            return Err(Error::Integrity("guest kernel exceeds 128 MiB".into()));
        }
        output.write_all(&bytes[..n])?;
        hash.update(&bytes[..n]);
    }
    Ok((
        size,
        ContentDigest::try_from(format!("{:x}", hash.finalize()))?,
    ))
}
fn read_limited(mut input: impl Read, limit: u64, token: &CancellationToken) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    cancelled(token)?;
    input.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    cancelled(token)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Integrity(
            "kernel metadata exceeds size limit".into(),
        ));
    }
    Ok(bytes)
}
fn lookup_ready(root: &Path, architecture: &str) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Setup(format!("kernel cache {}: {error}; check its permissions or set XDG_CACHE_HOME to a writable directory", root.display()))),
        Ok(_) => (),
    }
    cached(root, architecture).map(Some).map_err(|error| Error::Setup(format!(
        "kernel cache {} is invalid: {error}; remove that directory and retry a run, or pass --kernel PATH", root.display()
    )))
}
fn cached(root: &Path, architecture: &str) -> Result<PathBuf> {
    for path in [
        root.to_owned(),
        root.join("kernel"),
        root.join("kernel.json"),
    ] {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink()
            || (path == root && !metadata.is_dir())
            || (path != root && !metadata.is_file())
        {
            return Err(Error::Integrity(
                "kernel cache contains a symlink or unexpected file type".into(),
            ));
        }
    }
    let metadata = read_limited(
        File::open(root.join("kernel.json"))?,
        MAX_METADATA,
        &CancellationToken::default(),
    )?;
    let record: CachedKernel = serde_json::from_slice(&metadata)?;
    let (size, sha256) = copy_kernel(
        File::open(root.join("kernel"))?,
        std::io::sink(),
        &CancellationToken::default(),
    )?;
    if record.release != RELEASE_ID
        || record.architecture != architecture
        || size == 0
        || size != record.size
        || sha256 != record.sha256
    {
        return Err(Error::Integrity(
            "cached kernel digest or identity differs".into(),
        ));
    }
    Ok(root.join("kernel"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
    };

    // Serve the GitHub release protocol locally; no test credentials or network.
    fn server(
        kernel: &[u8],
        expected: &[u8],
        status: u16,
    ) -> (KernelDownload, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let kernel = kernel.to_vec();
        let metadata = serde_json::json!({
            "tag_name": format!("kernel-{RELEASE_ID}"), "draft": false,
            "assets": [{"id": 7, "name": "seedbox-kernel-aarch64", "size": expected.len(),
                "digest": format!("sha256:{}", ContentDigest::of(expected).hex())}]
        })
        .to_string()
        .into_bytes();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..if status == 200 { 2 } else { 1 } {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    connection.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = if index == 0 { &metadata } else { &kernel };
                write!(
                    connection,
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                connection.write_all(body).unwrap();
            }
            requests
        });
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .new_agent();
        let mut downloader = KernelDownload::new(agent, Some("test-credential")).unwrap();
        downloader.api = format!("http://{address}");
        (downloader, worker)
    }

    #[test]
    fn first_download_is_verified_then_reused_without_http() {
        let directory = tempfile::tempdir().unwrap();
        let (downloader, server) = server(b"guest kernel", b"guest kernel", 200);
        let messages = Mutex::new(Vec::new());
        let progress = |s: &str| messages.lock().unwrap().push(s.to_owned());
        let path = downloader
            .ensure(directory.path(), "aarch64", &Default::default(), progress)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"guest kernel");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(&format!(
            "GET /repos/{REPOSITORY}/releases/tags/kernel-{RELEASE_ID} "
        )));
        assert!(requests[1].starts_with(&format!("GET /repos/{REPOSITORY}/releases/assets/7 ")));
        assert!(requests.iter().all(|s| s
            .to_ascii_lowercase()
            .contains("authorization: bearer test-credential\r\n")));
        // The listener is gone. A second call and doctor's lookup are offline.
        assert_eq!(
            downloader
                .ensure(directory.path(), "aarch64", &Default::default(), progress)
                .unwrap(),
            path
        );
        assert_eq!(
            KernelDownload::cached(directory.path(), "aarch64").unwrap(),
            path
        );
        let messages = messages.into_inner().unwrap();
        assert!(messages[0].starts_with("downloading aarch64 guest kernel"));
        assert!(messages
            .last()
            .unwrap()
            .starts_with("using cached guest kernel"));
        assert!(
            !fs::read_to_string(path.parent().unwrap().join("kernel.json"))
                .unwrap()
                .contains("test-credential")
        );
    }

    #[test]
    fn concurrent_invocations_share_one_download() {
        let directory = tempfile::tempdir().unwrap();
        let (downloader, server) = server(b"guest kernel", b"guest kernel", 200);
        let downloader = Arc::new(downloader);
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let downloader = downloader.clone();
                let cache = directory.path().to_owned();
                thread::spawn(move || {
                    downloader
                        .ensure(&cache, "aarch64", &Default::default(), |_| {})
                        .unwrap()
                })
            })
            .collect();
        let paths: Vec<_> = workers.into_iter().map(|s| s.join().unwrap()).collect();
        assert_eq!(paths[0], paths[1]);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn corrupt_download_never_publishes_a_cache_entry() {
        let directory = tempfile::tempdir().unwrap();
        let (downloader, server) = server(b"different", b"expected!", 200);
        let error = downloader
            .ensure(directory.path(), "aarch64", &Default::default(), |_| {})
            .unwrap_err();
        assert!(error.to_string().contains("SHA-256 differs"));
        server.join().unwrap();
        let parent = directory.path().join(RELEASE_ID).join("aarch64");
        assert!(!parent.join("ready").exists());
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1); // only the lock
    }

    #[test]
    fn private_release_error_has_an_authentication_and_local_kernel_remedy() {
        let directory = tempfile::tempdir().unwrap();
        let (downloader, server) = server(b"", b"", 404);
        let error = downloader
            .ensure(directory.path(), "aarch64", &Default::default(), |_| {})
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        for text in [
            "HTTP 404",
            "GH_TOKEN",
            "read access",
            "Guest kernels workflow",
            "--kernel PATH",
        ] {
            assert!(error.contains(text));
        }
        assert!(!error.contains("test-credential"));
    }

    #[test]
    fn changed_cached_bytes_are_rejected_without_downloading() {
        let directory = tempfile::tempdir().unwrap();
        let (downloader, server) = server(b"guest kernel", b"guest kernel", 200);
        let path = downloader
            .ensure(directory.path(), "aarch64", &Default::default(), |_| {})
            .unwrap();
        server.join().unwrap();
        fs::write(path, b"replacement").unwrap();
        let error = downloader
            .ensure(directory.path(), "aarch64", &Default::default(), |_| {})
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("remove that directory and retry"));
    }

    #[test]
    fn lookup_and_pre_cancelled_download_create_no_ready_entry() {
        let directory = tempfile::tempdir().unwrap();
        assert!(KernelDownload::cached(directory.path(), "aarch64")
            .unwrap_err()
            .to_string()
            .contains("run seedbox run"));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        let token = CancellationToken::default();
        token.cancel();
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .build()
            .new_agent();
        let downloader = KernelDownload::new(agent, None).unwrap();
        assert!(matches!(
            downloader.ensure(directory.path(), "aarch64", &token, |_| {}),
            Err(Error::PreparationCancelled)
        ));
    }

    #[test]
    fn cargo_and_actions_identify_exactly_the_same_kernel_inputs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = std::process::Command::new("python3")
            .arg(root.join("kernel/release.py"))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), RELEASE_ID);
    }
}
