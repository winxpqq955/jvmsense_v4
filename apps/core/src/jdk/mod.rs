//! Provisioning a JDK to launch applications with.
//!
//! The launcher ships no JVM of its own and borrows nothing from the machine's
//! installed toolchain: it downloads **IBM Semeru 25** once into a cache
//! directory and reuses it from then on. That keeps the runtime the application
//! sees under our control — the version, the vendor, and the exact `jvm.dll`
//! whose exports the hooks were written against.
//!
//! Two properties matter and are enforced here:
//!
//! - **Verified.** The archive's SHA-256 is checked against the value published
//!   beside it before anything is extracted. A launcher that runs untrusted
//!   application code should not also be trusting an unverified download.
//! - **Atomic.** Extraction happens into a staging directory that is renamed
//!   into place only on success, so an interrupted download cannot leave a
//!   half-populated JDK that later runs would treat as valid.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The Semeru release this build targets.
pub const SEMERU_RELEASE: &str = "jdk-25.0.4.10";
/// The archive file name within the release.
pub const SEMERU_ARCHIVE: &str = "ibm-semeru-open-jdk_x64_windows_25.0.4.10.zip";
/// The published SHA-256 of that archive.
pub const SEMERU_SHA256: &str = "e998ad47a3096a02534faca5c13738abb458a45c857d5061e32b5a8c5410f164";

/// The release download base.
const RELEASE_BASE: &str = "https://github.com/ibmruntimes/semeru25-binaries/releases/download";

/// A provisioned JDK, ready to launch with.
#[derive(Debug, Clone)]
pub struct Jdk {
    home: PathBuf,
}

impl Jdk {
    /// Wrap an existing JDK home without provisioning.
    ///
    /// Used by the tests to honour an explicit `JVMSENSE_TEST_JRE`, and by
    /// callers that manage their own runtime.
    #[must_use]
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// The JDK's home directory (`.../bin/java.exe` lives below it).
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The `jvm.dll` to load explicitly, so the JVM comes from this JDK rather
    /// than from `JAVA_HOME` or the registry.
    #[must_use]
    pub fn jvm_dll(&self) -> PathBuf {
        self.home.join("bin").join("server").join("jvm.dll")
    }

    /// The `java.dll` whose exports the hooks are written against.
    #[must_use]
    pub fn java_dll(&self) -> PathBuf {
        self.home.join("bin").join("java.dll")
    }

    /// The `java.exe`, for running tooling in a subprocess.
    #[must_use]
    pub fn java_exe(&self) -> PathBuf {
        self.home.join("bin").join("java.exe")
    }

    /// True when the layout looks like a usable JDK.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.jvm_dll().is_file() && self.java_dll().is_file()
    }

    /// Read the `release` file's `JAVA_VERSION`, if present.
    ///
    /// Used to confirm a cached JDK is the version expected, rather than
    /// trusting the directory name.
    #[must_use]
    pub fn version(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.home.join("release")).ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("JAVA_VERSION=") {
                return Some(rest.trim().trim_matches('"').to_string());
            }
        }
        None
    }
}

/// Where provisioned JDKs live, and how to obtain one.
#[derive(Debug, Clone)]
pub struct JdkProvisioner {
    cache_root: PathBuf,
    /// When set, no network access happens and a missing JDK is an error.
    offline: bool,
}

impl JdkProvisioner {
    /// Provision into `cache_root`, downloading if the JDK is not already there.
    #[must_use]
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
            offline: false,
        }
    }

    /// Provision into the conventional per-user cache location.
    ///
    /// # Errors
    ///
    /// Returns [`JdkError::NoCacheLocation`] when neither `LOCALAPPDATA` nor a
    /// home directory is available.
    pub fn with_default_cache() -> Result<Self, JdkError> {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .ok_or(JdkError::NoCacheLocation)?;
        Ok(Self::new(base.join("jvmsense").join("jdks")))
    }

    /// Forbid downloads. A missing JDK becomes an error rather than a fetch.
    #[must_use]
    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }

    /// The directory a given release extracts to.
    #[must_use]
    pub fn install_dir(&self) -> PathBuf {
        self.cache_root.join(SEMERU_RELEASE)
    }

    /// Return the provisioned JDK, downloading it if necessary.
    ///
    /// # Errors
    ///
    /// See [`JdkError`]. A cached JDK that fails its layout check is removed
    /// and re-fetched, since a partial extraction is the likely cause.
    pub fn ensure(&self) -> Result<Jdk, JdkError> {
        let install = self.install_dir();

        if let Some(jdk) = self.try_cached(&install) {
            return Ok(jdk);
        }

        if self.offline {
            return Err(JdkError::NotCached { path: install });
        }

        self.download_and_extract(&install)?;

        self.try_cached(&install)
            .ok_or(JdkError::IncompleteInstall { path: install })
    }

    /// A cached JDK, if the directory holds a complete one.
    fn try_cached(&self, install: &Path) -> Option<Jdk> {
        let jdk = Jdk {
            home: install.to_path_buf(),
        };
        if jdk.is_complete() {
            Some(jdk)
        } else {
            None
        }
    }

    /// Fetch the archive, verify it, and extract it into place atomically.
    fn download_and_extract(&self, install: &Path) -> Result<(), JdkError> {
        std::fs::create_dir_all(&self.cache_root).map_err(|source| JdkError::Io {
            path: self.cache_root.clone(),
            source,
        })?;

        let archive_path = self.cache_root.join(SEMERU_ARCHIVE);

        // Reuse a previously downloaded archive only if it still hashes
        // correctly; otherwise fetch it again.
        let needs_download = match std::fs::read(&archive_path) {
            Ok(bytes) => hex_lower(&Sha256::digest(&bytes)) != SEMERU_SHA256,
            Err(_) => true,
        };

        if needs_download {
            let url = format!("{RELEASE_BASE}/{SEMERU_RELEASE}/{SEMERU_ARCHIVE}");
            let bytes = http_get(&url)?;
            let digest = hex_lower(&Sha256::digest(&bytes));
            if digest != SEMERU_SHA256 {
                return Err(JdkError::ChecksumMismatch {
                    expected: SEMERU_SHA256.to_string(),
                    actual: digest,
                });
            }
            std::fs::write(&archive_path, &bytes).map_err(|source| JdkError::Io {
                path: archive_path.clone(),
                source,
            })?;
        }

        // Extract into a staging directory, then rename. A rename within one
        // volume is atomic, so a concurrent or interrupted run can never see a
        // half-populated JDK.
        let staging = self.cache_root.join(format!("{SEMERU_RELEASE}.staging"));
        if staging.exists() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        std::fs::create_dir_all(&staging).map_err(|source| JdkError::Io {
            path: staging.clone(),
            source,
        })?;

        extract_zip(&archive_path, &staging)?;

        // The archive contains a single top-level directory; descend into it
        // so `install` is the JDK home rather than a directory containing one.
        let root = single_child_dir(&staging).unwrap_or_else(|| staging.clone());

        if install.exists() {
            std::fs::remove_dir_all(install).map_err(|source| JdkError::Io {
                path: install.to_path_buf(),
                source,
            })?;
        }
        std::fs::rename(&root, install).map_err(|source| JdkError::Io {
            path: install.to_path_buf(),
            source,
        })?;
        let _ = std::fs::remove_dir_all(&staging);

        Ok(())
    }
}

/// The single directory inside `dir`, when there is exactly one entry and it is
/// a directory. Release archives conventionally wrap their payload this way.
fn single_child_dir(dir: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(dir).ok()?;
    let first = entries.next()?.ok()?;
    if entries.next().is_some() {
        return None;
    }
    let path = first.path();
    path.is_dir().then_some(path)
}

/// Extract every entry of a zip archive under `dest`, rejecting paths that
/// would escape it.
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), JdkError> {
    let file = std::fs::File::open(archive).map_err(|source| JdkError::Io {
        path: archive.to_path_buf(),
        source,
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|source| JdkError::Archive {
        path: archive.to_path_buf(),
        message: source.to_string(),
    })?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|source| JdkError::Archive {
            path: archive.to_path_buf(),
            message: source.to_string(),
        })?;

        // `enclosed_name` returns None for absolute paths and `..` escapes.
        // Honouring it is what stops a malicious archive from writing outside
        // the cache directory.
        let Some(relative) = entry.enclosed_name() else {
            return Err(JdkError::UnsafeEntry {
                name: entry.name().to_string(),
            });
        };
        let target = dest.join(relative);

        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|source| JdkError::Io {
                path: target,
                source,
            })?;
            continue;
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| JdkError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut out = std::fs::File::create(&target).map_err(|source| JdkError::Io {
            path: target.clone(),
            source,
        })?;
        std::io::copy(&mut entry, &mut out).map_err(|source| JdkError::Io {
            path: target,
            source,
        })?;
    }

    Ok(())
}

/// Minimal HTTPS GET.
///
/// Deliberately not a general HTTP client: the launcher fetches exactly two
/// kinds of URL (a release archive and a payload), so the only features needed
/// are a redirect-following GET returning the body, and every added dependency
/// here would be a dependency an attacker could reach.
fn http_get(url: &str) -> Result<Vec<u8>, JdkError> {
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store()?)
        .with_no_client_auth();
    let server_name =
        rustls::pki_types::ServerName::try_from(host_of(url).to_string()).map_err(|_| {
            JdkError::Download {
                url: url.to_string(),
                message: "invalid host name".into(),
            }
        })?;
    // `StreamOwned` borrows the connection's inner `ConnectionCommon`, so the
    // `ClientConnection` must outlive it and be passed by `&mut *`.
    let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
        .map_err(|source| JdkError::Download {
            url: url.to_string(),
            message: source.to_string(),
        })?;

    let sock =
        std::net::TcpStream::connect((host_of(url), 443)).map_err(|source| JdkError::Download {
            url: url.to_string(),
            message: source.to_string(),
        })?;
    // `ClientConnection` derefs to `ConnectionCommon`; `StreamOwned` wants the
    // latter, so reborrow through the deref.
    let mut tls = rustls::StreamOwned::new(&mut *conn, sock);

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: jvmsense\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path_of(url),
        host_of(url)
    );
    std::io::Write::write_all(&mut tls, request.as_bytes()).map_err(|source| {
        JdkError::Download {
            url: url.to_string(),
            message: source.to_string(),
        }
    })?;

    let mut response = Vec::new();
    tls.read_to_end(&mut response)
        .map_err(|source| JdkError::Download {
            url: url.to_string(),
            message: source.to_string(),
        })?;

    let (head, body) = split_response(&response).ok_or_else(|| JdkError::Download {
        url: url.to_string(),
        message: "malformed HTTP response".into(),
    })?;

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);

    // A proxy may chunk the body even with `Connection: close`; passing the
    // chunk framing through as payload would corrupt the archive and only be
    // caught by the checksum, which is a much worse error message.
    let chunked = head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    });
    let body = if chunked {
        dechunk(body).ok_or_else(|| JdkError::Download {
            url: url.to_string(),
            message: "malformed chunked body".into(),
        })?
    } else {
        body.to_vec()
    };

    match status {
        200 => Ok(body),
        301 | 302 | 303 | 307 | 308 => {
            let location = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("location")
                        .then(|| value.trim().to_string())
                })
                .ok_or_else(|| JdkError::Download {
                    url: url.to_string(),
                    message: "redirect without Location".into(),
                })?;
            http_get(&location)
        }
        other => Err(JdkError::Download {
            url: url.to_string(),
            message: format!("HTTP {other}"),
        }),
    }
}

/// Split an HTTP response into its head text and body bytes.
fn split_response(response: &[u8]) -> Option<(String, &[u8])> {
    let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&response[..split]).ok()?.to_string();
    Some((head, &response[split + 4..]))
}

/// Handle a chunked response body.
///
/// `Connection: close` usually avoids chunking, but a proxy may chunk anyway,
/// and passing chunk framing through as if it were payload would corrupt the
/// archive and be caught only by the checksum.
fn dechunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(out);
        }
        let start = line_end + 2;
        out.extend_from_slice(rest.get(start..start + size)?);
        rest = rest.get(start + size + 2..)?;
    }
}

fn host_of(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
}

fn path_of(url: &str) -> String {
    let after_host = url.split("//").nth(1).unwrap_or("");
    match after_host.find('/') {
        Some(index) => after_host[index..].to_string(),
        None => "/".to_string(),
    }
}

fn root_store() -> Result<rustls::RootCertStore, JdkError> {
    let mut store = rustls::RootCertStore::empty();
    let result = rustls_native_certs::load_native_certs();
    for cert in result.certs {
        let _ = store.add(cert);
    }
    if store.is_empty() {
        return Err(JdkError::Download {
            url: String::new(),
            message: "no system root certificates available".into(),
        });
    }
    Ok(store)
}

/// Lowercase hex encoding of a digest.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Errors from provisioning a JDK.
#[derive(Debug, thiserror::Error)]
pub enum JdkError {
    #[error("no cache location: neither LOCALAPPDATA nor HOME is set")]
    NoCacheLocation,

    #[error("no cached JDK at {path} and downloads are disabled")]
    NotCached { path: PathBuf },

    #[error("extraction produced an incomplete JDK at {path}")]
    IncompleteInstall { path: PathBuf },

    #[error("archive checksum mismatch: expected {expected}, computed {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("archive entry {name:?} would escape the extraction directory")]
    UnsafeEntry { name: String },

    #[error("cannot read archive {path}: {message}")]
    Archive { path: PathBuf, message: String },

    #[error("cannot fetch {url}: {message}")]
    Download { url: String, message: String },

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_path_are_extracted_from_a_url() {
        let url = "https://example.test/a/b.zip?q=1";
        assert_eq!(host_of(url), "example.test");
        assert_eq!(path_of(url), "/a/b.zip?q=1");
    }

    #[test]
    fn a_response_head_and_body_are_separated() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (head, body) = split_response(response).expect("split");
        assert!(head.starts_with("HTTP/1.1 200"));
        assert_eq!(body, b"hello");
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        // "hello" + "world" in two chunks
        let body = b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        assert_eq!(dechunk(body).expect("dechunk"), b"helloworld");
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn the_pinned_checksum_is_well_formed() {
        assert_eq!(SEMERU_SHA256.len(), 64);
        assert!(SEMERU_SHA256.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_missing_jdk_reports_not_cached_when_offline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provisioner = JdkProvisioner::new(dir.path().join("jdks")).offline();

        let err = provisioner.ensure().expect_err("must not download");
        assert!(matches!(err, JdkError::NotCached { .. }));
    }

    #[test]
    fn a_complete_layout_is_recognized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("jdk");
        // `jvm.dll` lives in `bin/server/`; `java.dll` directly in `bin/`.
        std::fs::create_dir_all(home.join("bin").join("server")).expect("mkdir");
        std::fs::write(home.join("bin").join("server").join("jvm.dll"), b"x").expect("write");
        std::fs::write(home.join("bin").join("java.dll"), b"x").expect("write");

        let jdk = Jdk { home };
        assert!(jdk.is_complete());
        assert!(jdk.jvm_dll().ends_with(r"server\jvm.dll"));
    }

    #[test]
    fn an_incomplete_layout_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("jdk");
        std::fs::create_dir_all(home.join("bin").join("server")).expect("mkdir");
        std::fs::write(home.join("bin").join("server").join("jvm.dll"), b"x").expect("write");

        assert!(!Jdk { home }.is_complete(), "java.dll is also required");
    }

    #[test]
    fn version_is_read_from_the_release_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("release"),
            "IMPLEMENTOR=\"IBM\"\nJAVA_VERSION=\"25.0.4\"\n",
        )
        .expect("write");

        let jdk = Jdk {
            home: dir.path().to_path_buf(),
        };
        assert_eq!(jdk.version().as_deref(), Some("25.0.4"));
    }

    #[test]
    fn a_single_wrapping_directory_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = dir.path().join("jdk-25.0.4.10");
        std::fs::create_dir(&inner).expect("mkdir");

        assert_eq!(single_child_dir(dir.path()), Some(inner));
    }

    #[test]
    fn multiple_entries_are_not_treated_as_a_wrapper() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("a")).expect("a");
        std::fs::create_dir(dir.path().join("b")).expect("b");

        assert_eq!(single_child_dir(dir.path()), None);
    }

    #[test]
    fn extraction_rejects_escaping_entries() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let archive = dir.path().join("evil.zip");
        {
            let file = std::fs::File::create(&archive).expect("create");
            let mut zw = zip::ZipWriter::new(file);
            zw.start_file("../escaped.txt", zip::write::FileOptions::<()>::default())
                .expect("start");
            zw.write_all(b"pwned").expect("write");
            zw.finish().expect("finish");
        }

        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).expect("mkdir");
        let err = extract_zip(&archive, &dest).expect_err("must reject");
        assert!(matches!(err, JdkError::UnsafeEntry { .. }));
    }

    #[test]
    fn extraction_writes_entries_under_the_destination() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let archive = dir.path().join("ok.zip");
        {
            let file = std::fs::File::create(&archive).expect("create");
            let mut zw = zip::ZipWriter::new(file);
            zw.start_file("sub/file.txt", zip::write::FileOptions::<()>::default())
                .expect("start");
            zw.write_all(b"payload").expect("write");
            zw.finish().expect("finish");
        }

        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).expect("mkdir");
        extract_zip(&archive, &dest).expect("extract");

        assert_eq!(
            std::fs::read(dest.join("sub").join("file.txt")).expect("read"),
            b"payload"
        );
    }
}
