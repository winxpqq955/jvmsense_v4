//! Content-addressed store of artifact bytes.
//!
//! Every byte an application needs lives here, in `Arc<[u8]>`, for the life of
//! the session. Nothing is written to disk except the zero-length placeholders
//! that give the bytes a path the JVM will accept (see `vfs::hollow`).
//!
//! Artifacts arrive as *payloads*: the bytes on disk are a zstd-compressed or
//! identity-encoded blob, and the manifest carries the SHA-256 and decoded
//! size. Both are verified here, before the bytes are ever handed to the
//! virtual file system, so a corrupted or tampered payload fails at load
//! rather than as a confusing parse error deep inside the JVM.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// The decoded bytes of one artifact, plus the identity it was verified under.
#[derive(Debug, Clone)]
pub struct Artifact {
    bytes: Arc<[u8]>,
    sha256: [u8; 32],
    source: PathBuf,
}

impl Artifact {
    pub(crate) fn from_memory(bytes: Arc<[u8]>, sha256: [u8; 32]) -> Self {
        Self {
            bytes,
            sha256,
            source: PathBuf::new(),
        }
    }

    /// The artifact's bytes.
    #[must_use]
    pub fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    /// The artifact's length in bytes, after decoding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when the artifact decoded to zero bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The verified SHA-256 of the decoded bytes.
    #[must_use]
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// The payload file this artifact was loaded from.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }
}

/// How an artifact's payload is encoded on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageEncoding {
    /// The payload is the artifact itself.
    Identity,
    /// The payload is zstd-compressed; the manifest's size is the *decoded* size.
    Zstd,
}

impl StorageEncoding {
    /// Parse the manifest's `storage_encoding` string.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::UnknownEncoding`] for anything else. This is
    /// deliberately strict: silently treating an unknown encoding as identity
    /// would hand the JVM garbage.
    pub fn parse(text: &str) -> Result<Self, ArtifactError> {
        match text {
            "identity" => Ok(Self::Identity),
            "zstd" => Ok(Self::Zstd),
            other => Err(ArtifactError::UnknownEncoding {
                encoding: other.to_string(),
            }),
        }
    }

    /// The manifest spelling of this encoding.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Zstd => "zstd",
        }
    }
}

/// What the manifest declares about one payload, used to verify it.
#[derive(Debug, Clone)]
pub struct ArtifactSpec {
    /// Human-readable name, used in error messages.
    pub name: String,
    /// Where the encoded payload lives.
    pub payload_path: PathBuf,
    /// How the payload is encoded.
    pub encoding: StorageEncoding,
    /// Expected SHA-256 of the *decoded* bytes, lowercase hex.
    pub sha256_hex: String,
    /// Expected decoded length.
    pub size_bytes: u64,
}

/// Loads and verifies artifacts, and hands out shared handles to their bytes.
///
/// Not `Clone`: the store owns the bytes and is shared as `Arc<ArtifactStore>`
/// so a session teardown drops every handle at once.
#[derive(Debug, Default)]
pub struct ArtifactStore {
    /// Keyed by lowercase hex SHA-256, so identical artifacts load once.
    artifacts: HashMap<String, Artifact>,
}

impl ArtifactStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct artifacts are loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.artifacts.len()
    }

    /// True when nothing is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty()
    }

    /// Insert bytes produced in this process, returning their SHA-256 hex.
    ///
    /// This is the boundary for transformed payloads such as a remapped jar:
    /// the bytes move from the producer's pipe directly into the store. They
    /// are content-addressed exactly like a verified on-disk payload, while
    /// `source` remains empty because no payload file exists.
    pub fn insert_memory(&mut self, bytes: Vec<u8>) -> String {
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let sha256_hex = hex_lower(&digest);
        self.artifacts
            .entry(sha256_hex.clone())
            .or_insert_with(|| Artifact {
                bytes: Arc::from(bytes.into_boxed_slice()),
                sha256: digest,
                source: PathBuf::new(),
            });
        sha256_hex
    }

    /// Load, decode and verify one payload, returning its SHA-256 hex.
    ///
    /// Loading the same artifact twice is a no-op beyond a re-verification, so
    /// two jars that share a library cost one copy of its bytes.
    ///
    /// # Errors
    ///
    /// - [`ArtifactError::Io`] if the payload cannot be read.
    /// - [`ArtifactError::Decode`] if zstd decompression fails.
    /// - [`ArtifactError::SizeMismatch`] if the decoded length disagrees.
    /// - [`ArtifactError::HashMismatch`] if the digest disagrees.
    pub fn load(&mut self, spec: &ArtifactSpec) -> Result<String, ArtifactError> {
        let payload = std::fs::read(&spec.payload_path).map_err(|source| ArtifactError::Io {
            path: spec.payload_path.clone(),
            source,
        })?;

        let decoded = match spec.encoding {
            StorageEncoding::Identity => payload,
            StorageEncoding::Zstd => {
                zstd::stream::decode_all(payload.as_slice()).map_err(|source| {
                    ArtifactError::Decode {
                        name: spec.name.clone(),
                        path: spec.payload_path.clone(),
                        source,
                    }
                })?
            }
        };

        // Size first: it is cheap, and a size mismatch usually means the
        // manifest and the payload are from different builds.
        if decoded.len() as u64 != spec.size_bytes {
            return Err(ArtifactError::SizeMismatch {
                name: spec.name.clone(),
                declared: spec.size_bytes,
                actual: decoded.len(),
            });
        }

        let digest: [u8; 32] = Sha256::digest(&decoded).into();
        let actual_hex = hex_lower(&digest);
        let expected_hex = spec.sha256_hex.to_ascii_lowercase();
        if actual_hex != expected_hex {
            return Err(ArtifactError::HashMismatch {
                name: spec.name.clone(),
                declared: expected_hex,
                actual: actual_hex,
            });
        }

        self.artifacts
            .entry(actual_hex.clone())
            .or_insert(Artifact {
                bytes: Arc::from(decoded.into_boxed_slice()),
                sha256: digest,
                source: spec.payload_path.clone(),
            });

        Ok(actual_hex)
    }

    /// Look up a previously loaded artifact by its SHA-256 hex.
    #[must_use]
    pub fn get(&self, sha256_hex: &str) -> Option<&Artifact> {
        self.artifacts.get(&sha256_hex.to_ascii_lowercase())
    }

    /// Iterate over every loaded artifact.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Artifact)> {
        self.artifacts.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// Lowercase hex encoding of a digest.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Errors from loading or verifying an artifact.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("cannot read payload for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot decompress zstd payload for {name} ({path}): {source}")]
    Decode {
        name: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{name}: declared {declared} bytes but decoded to {actual}")]
    SizeMismatch {
        name: String,
        declared: u64,
        actual: usize,
    },

    #[error("{name}: declared sha256 {declared} but computed {actual}")]
    HashMismatch {
        name: String,
        declared: String,
        actual: String,
    },

    #[error("unknown storage encoding {encoding:?} (expected \"identity\" or \"zstd\")")]
    UnknownEncoding { encoding: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_payload(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write payload");
        path
    }

    fn spec_for(path: PathBuf, decoded: &[u8], encoding: StorageEncoding) -> ArtifactSpec {
        ArtifactSpec {
            name: "test".into(),
            payload_path: path,
            encoding,
            sha256_hex: hex_lower(&Sha256::digest(decoded)),
            size_bytes: decoded.len() as u64,
        }
    }

    #[test]
    fn identity_payload_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = b"hello world";
        let path = write_payload(dir.path(), "a.bin", data);
        let mut store = ArtifactStore::new();

        let sha = store
            .load(&spec_for(path, data, StorageEncoding::Identity))
            .expect("load");

        assert_eq!(store.get(&sha).expect("get").bytes().as_ref(), data);
    }

    #[test]
    fn zstd_payload_is_decoded_and_sized_by_the_decoded_form() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = vec![0xabu8; 4096];
        let compressed = zstd::stream::encode_all(data.as_slice(), 3).expect("compress");
        let path = write_payload(dir.path(), "a.zst", &compressed);
        let mut store = ArtifactStore::new();

        let sha = store
            .load(&spec_for(path, &data, StorageEncoding::Zstd))
            .expect("load");

        assert_eq!(store.get(&sha).expect("get").len(), data.len());
    }

    #[test]
    fn a_corrupted_payload_is_rejected_on_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = b"original";
        // Same length as `data` so the size check passes and the hash check is
        // what actually rejects it.
        let path = write_payload(dir.path(), "a.bin", b"tampered");
        let mut store = ArtifactStore::new();

        let err = store
            .load(&spec_for(path, data, StorageEncoding::Identity))
            .expect_err("must reject");

        assert!(matches!(err, ArtifactError::HashMismatch { .. }));
    }

    #[test]
    fn a_wrong_declared_size_is_rejected_before_hashing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = b"twelve bytes";
        let path = write_payload(dir.path(), "a.bin", data);
        let mut spec = spec_for(path, data, StorageEncoding::Identity);
        spec.size_bytes = 999;

        let err = ArtifactStore::new().load(&spec).expect_err("must reject");
        assert!(matches!(err, ArtifactError::SizeMismatch { .. }));
    }

    #[test]
    fn loading_the_same_artifact_twice_stores_one_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = b"shared";
        let path = write_payload(dir.path(), "a.bin", data);
        let mut store = ArtifactStore::new();

        let first = store
            .load(&spec_for(path.clone(), data, StorageEncoding::Identity))
            .expect("load");
        let second = store
            .load(&spec_for(path, data, StorageEncoding::Identity))
            .expect("load");

        assert_eq!(first, second);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn the_hash_is_case_insensitive_on_lookup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = b"x";
        let path = write_payload(dir.path(), "a.bin", data);
        let mut store = ArtifactStore::new();
        let sha = store
            .load(&spec_for(path, data, StorageEncoding::Identity))
            .expect("load");

        assert!(store.get(&sha.to_uppercase()).is_some());
    }

    #[test]
    fn unknown_encoding_is_rejected() {
        let err = StorageEncoding::parse("lz4").expect_err("must reject");
        assert!(matches!(err, ArtifactError::UnknownEncoding { .. }));
    }

    #[test]
    fn a_broken_zstd_stream_reports_a_decode_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_payload(dir.path(), "a.zst", b"not zstd at all");
        let spec = ArtifactSpec {
            name: "broken".into(),
            payload_path: path,
            encoding: StorageEncoding::Zstd,
            sha256_hex: "00".repeat(32),
            size_bytes: 10,
        };

        let err = ArtifactStore::new().load(&spec).expect_err("must reject");
        assert!(matches!(err, ArtifactError::Decode { .. }));
    }

    #[test]
    fn encoding_strings_round_trip() {
        for encoding in [StorageEncoding::Identity, StorageEncoding::Zstd] {
            assert_eq!(
                StorageEncoding::parse(encoding.as_str()).expect("parse"),
                encoding
            );
        }
    }

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff]), "000fff");
    }

    /// A payload larger than one buffer, to exercise the decoder's chunking.
    #[test]
    fn large_zstd_payload_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut data = Vec::new();
        for i in 0..200_000u32 {
            data.write_all(&i.to_le_bytes()).expect("build data");
        }
        let compressed = zstd::stream::encode_all(data.as_slice(), 1).expect("compress");
        let path = write_payload(dir.path(), "big.zst", &compressed);

        let mut store = ArtifactStore::new();
        let sha = store
            .load(&spec_for(path, &data, StorageEncoding::Zstd))
            .expect("load");

        assert_eq!(store.get(&sha).expect("get").len(), data.len());
    }
}
