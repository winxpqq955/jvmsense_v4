//! Indexing a jar's entries into in-memory maps.
//!
//! The predecessor decompressed every entry into its own `Vec<u8>` and kept
//! four global maps. That works but wastes memory on large jars and makes the
//! registry a process singleton that multi-launch and testing cannot tolerate.
//!
//! Instead we keep **one** copy of the jar's bytes (in [`crate::artifact`])
//! and record, for each entry, the `(offset, length)` of its *compressed*
//! payload inside that archive. A read then inflates on demand. The index is a
//! plain value owned by the session, not a global.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;

use crate::artifact::Artifact;

/// Where one entry's bytes live inside the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryLocation {
    /// Offset of the entry's payload within the jar's bytes.
    pub offset: usize,
    /// Length of the stored payload (compressed, if the entry is deflated).
    pub stored_len: usize,
    /// Length after inflation; equal to `stored_len` for stored entries.
    pub uncompressed_len: usize,
    /// The entry's compression method, as recorded in the jar.
    pub method: CompressionMethod,
}

/// The compression methods this indexer can inflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMethod {
    /// Stored without compression.
    Stored,
    /// Raw DEFLATE, which is what jar entries normally use.
    Deflated,
}

/// What kind of thing an entry is. Classes are separated because the class
/// loader looks them up by binary name while everything else is looked up by
/// resource path, and mixing the two namespaces causes confusing collisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Class,
    Resource,
}

/// One indexed entry.
#[derive(Debug, Clone)]
pub struct JarEntry {
    /// The path as it appears in the archive, with `/` separators.
    pub name: String,
    /// The entry's location within the archive.
    pub location: EntryLocation,
    /// Whether this is a `.class` file.
    pub kind: EntryKind,
    /// True for a directory entry.
    pub is_directory: bool,
}

impl JarEntry {
    /// The binary class name (`a/b/C` -> `a.b.C`) for class entries.
    ///
    /// Returns `None` for non-class entries and for entries under `META-INF/`,
    /// which the JVM's own loader also excludes because a class file there is
    /// not a loadable class.
    #[must_use]
    pub fn binary_class_name(&self) -> Option<String> {
        if self.kind != EntryKind::Class || self.is_directory {
            return None;
        }
        if self.name.starts_with("META-INF/") {
            return None;
        }
        let stem = self.name.strip_suffix(".class")?;
        Some(stem.replace('/', "."))
    }
}

/// An indexed jar: the archive's bytes plus a map of what is inside it.
#[derive(Debug, Clone)]
pub struct JarIndex {
    bytes: Arc<[u8]>,
    /// Entry path (normalized, `/` separators, no leading `/`) -> entry.
    entries: HashMap<String, JarEntry>,
    /// Binary class name -> entry path.
    classes: HashMap<String, String>,
}

impl JarIndex {
    /// Index a jar from an already-loaded, integrity-checked artifact.
    ///
    /// # Errors
    ///
    /// Returns [`JarError`] if the archive is malformed. A jar that cannot be
    /// indexed is a hard failure rather than a warning: silently skipping it
    /// would surface later as an unexplained `ClassNotFoundException`.
    pub fn from_artifact(artifact: &Artifact) -> Result<Self, JarError> {
        let bytes: Arc<[u8]> = Arc::clone(artifact.bytes());
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes.as_ref())).map_err(|source| JarError::Open {
                message: source.to_string(),
            })?;

        let mut entries = HashMap::new();
        let mut classes = HashMap::new();

        for i in 0..archive.len() {
            let file = archive.by_index_raw(i).map_err(|source| JarError::Entry {
                index: i,
                message: source.to_string(),
            })?;

            let name = normalize_entry_name(file.name());
            let is_directory = file.is_dir();
            let method = match file.compression() {
                zip::CompressionMethod::Stored => CompressionMethod::Stored,
                // Any other method we cannot inflate would silently produce
                // garbage, so refuse the archive instead.
                zip::CompressionMethod::Deflated => CompressionMethod::Deflated,
                other => {
                    return Err(JarError::UnsupportedCompression {
                        entry: name,
                        method: format!("{other:?}"),
                    })
                }
            };

            let location = EntryLocation {
                offset: file.data_start() as usize,
                stored_len: file.compressed_size() as usize,
                uncompressed_len: file.size() as usize,
                method,
            };

            let kind = if !is_directory && name.ends_with(".class") {
                EntryKind::Class
            } else {
                EntryKind::Resource
            };

            let entry = JarEntry {
                name: name.clone(),
                location,
                kind,
                is_directory,
            };

            if let Some(binary) = entry.binary_class_name() {
                // First definition wins, matching the JVM's own classpath
                // resolution order when several jars define the same class.
                classes.entry(binary).or_insert_with(|| name.clone());
            }
            entries.entry(name).or_insert(entry);
        }

        Ok(Self {
            bytes,
            entries,
            classes,
        })
    }

    /// The archive's raw bytes.
    #[must_use]
    pub fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    /// Number of indexed entries (including directories).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the archive had no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of distinct loadable binary class names.
    #[must_use]
    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// Look up an entry by its archive path.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&JarEntry> {
        self.entries.get(&normalize_entry_name(name))
    }

    /// Look up a class entry by binary name (`a.b.C`).
    #[must_use]
    pub fn class_entry(&self, binary_name: &str) -> Option<&JarEntry> {
        let path = self.classes.get(binary_name)?;
        self.entries.get(path)
    }

    /// Iterate over every entry path.
    pub fn entry_names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Read an entry's bytes, inflating if necessary.
    ///
    /// # Errors
    ///
    /// Returns [`JarError::EntryNotFound`] if the name is not indexed, or
    /// [`JarError::Inflate`] if the stored payload is corrupt.
    pub fn read(&self, name: &str) -> Result<Vec<u8>, JarError> {
        let normalized = normalize_entry_name(name);
        let entry = self
            .entries
            .get(&normalized)
            .ok_or(JarError::EntryNotFound { name: normalized })?;
        self.read_entry(entry)
    }

    /// Read a class's bytes by binary name.
    ///
    /// # Errors
    ///
    /// Returns [`JarError::ClassNotFound`] if the class is not indexed.
    pub fn read_class(&self, binary_name: &str) -> Result<Vec<u8>, JarError> {
        let entry = self
            .class_entry(binary_name)
            .ok_or_else(|| JarError::ClassNotFound {
                name: binary_name.to_string(),
            })?;
        self.read_entry(entry)
    }

    fn read_entry(&self, entry: &JarEntry) -> Result<Vec<u8>, JarError> {
        let start = entry.location.offset;
        let end = start + entry.location.stored_len;
        let stored = self
            .bytes
            .get(start..end)
            .ok_or_else(|| JarError::Truncated {
                entry: entry.name.clone(),
                offset: start,
                len: entry.location.stored_len,
            })?;

        let mut out = Vec::with_capacity(entry.location.uncompressed_len);
        match entry.location.method {
            CompressionMethod::Stored => out.extend_from_slice(stored),
            CompressionMethod::Deflated => {
                flate2_decode(stored, entry, &mut out).map_err(|message| JarError::Inflate {
                    entry: entry.name.clone(),
                    message,
                })?;
            }
        }

        // A length disagreement means the index and the bytes are out of sync,
        // which would hand the JVM a truncated class.
        if out.len() != entry.location.uncompressed_len {
            return Err(JarError::LengthMismatch {
                entry: entry.name.clone(),
                expected: entry.location.uncompressed_len,
                actual: out.len(),
            });
        }
        Ok(out)
    }
}

/// Raw DEFLATE inflate, as jar entries store it (no zlib header).
fn flate2_decode(stored: &[u8], entry: &JarEntry, out: &mut Vec<u8>) -> Result<(), String> {
    use std::io::Write;

    let mut decoder = flate2::write::DeflateDecoder::new(out);
    decoder
        .write_all(stored)
        .map_err(|source| format!("{}: {source}", entry.name))?;
    decoder
        .finish()
        .map_err(|source| format!("{}: {source}", entry.name))?;
    Ok(())
}

/// Normalize an archive entry path: `/` separators, no leading `/`.
#[must_use]
pub fn normalize_entry_name(name: &str) -> String {
    let unified = name.replace('\\', "/");
    unified.trim_start_matches('/').to_string()
}

/// Errors from indexing or reading a jar.
#[derive(Debug, thiserror::Error)]
pub enum JarError {
    // Zip parsing failures carry no std::error::Error we can wrap, so these
    // hold the rendered message rather than a `#[source]`.
    #[error("cannot open archive: {message}")]
    Open { message: String },

    #[error("cannot read entry {index}: {message}")]
    Entry { index: usize, message: String },

    #[error("entry {entry:?} uses unsupported compression {method}")]
    UnsupportedCompression { entry: String, method: String },

    #[error("entry not found: {name}")]
    EntryNotFound { name: String },

    #[error("class not found in this archive: {name}")]
    ClassNotFound { name: String },

    #[error("entry {entry:?} points past the end of the archive (offset {offset}, len {len})")]
    Truncated {
        entry: String,
        offset: usize,
        len: usize,
    },

    #[error("cannot inflate {entry:?}: {message}")]
    Inflate { entry: String, message: String },

    #[error("entry {entry:?} inflated to {actual} bytes but the index said {expected}")]
    LengthMismatch {
        entry: String,
        expected: usize,
        actual: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{ArtifactStore, StorageEncoding};
    use sha2::Digest as _;
    use std::io::Write;

    /// Build a jar with the given entries and load it as an artifact.
    fn jar_artifact(entries: &[(&str, &[u8])]) -> (tempfile::TempDir, ArtifactStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.jar");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, data) in entries {
                zw.start_file(*name, opts).expect("start_file");
                zw.write_all(data).expect("write");
            }
            zw.finish().expect("finish");
        }

        let raw = std::fs::read(&path).expect("read back");
        let mut store = ArtifactStore::new();
        store
            .load(&crate::artifact::ArtifactSpec {
                name: "test.jar".into(),
                payload_path: path,
                encoding: StorageEncoding::Identity,
                sha256_hex: crate::artifact::hex_lower(&sha2::Sha256::digest(&raw)),
                size_bytes: raw.len() as u64,
            })
            .expect("load");
        (dir, store)
    }

    fn index_of(entries: &[(&str, &[u8])]) -> (tempfile::TempDir, JarIndex) {
        let (dir, store) = jar_artifact(entries);
        let artifact = store.iter().next().expect("one artifact").1;
        let index = JarIndex::from_artifact(artifact).expect("index");
        (dir, index)
    }

    #[test]
    fn classes_are_indexed_by_binary_name() {
        let (_dir, index) = index_of(&[("net/minecraft/client/main/Main.class", b"CAFEBABE")]);

        assert!(index
            .class_entry("net.minecraft.client.main.Main")
            .is_some());
        assert_eq!(index.class_count(), 1);
    }

    #[test]
    fn a_class_under_meta_inf_is_not_loadable() {
        let (_dir, index) = index_of(&[("META-INF/versions/9/Foo.class", b"x")]);

        assert!(index.entry("META-INF/versions/9/Foo.class").is_some());
        assert_eq!(
            index.class_count(),
            0,
            "a class file under META-INF is not a loadable class"
        );
    }

    #[test]
    fn deflated_entries_inflate_back_to_their_original_bytes() {
        // Highly compressible so the stored form differs from the original.
        let payload = vec![b'A'; 8192];
        let (_dir, index) = index_of(&[("big.bin", &payload)]);

        let entry = index.entry("big.bin").expect("entry");
        assert_eq!(entry.location.method, CompressionMethod::Deflated);
        assert!(entry.location.stored_len < payload.len());
        assert_eq!(index.read("big.bin").expect("read"), payload);
    }

    #[test]
    fn stored_entries_are_returned_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stored.jar");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("raw.bin", opts).expect("start");
            zw.write_all(b"uncompressed payload").expect("write");
            zw.finish().expect("finish");
        }
        let raw = std::fs::read(&path).expect("read");
        let mut store = ArtifactStore::new();
        store
            .load(&crate::artifact::ArtifactSpec {
                name: "stored.jar".into(),
                payload_path: path,
                encoding: StorageEncoding::Identity,
                sha256_hex: crate::artifact::hex_lower(&sha2::Sha256::digest(&raw)),
                size_bytes: raw.len() as u64,
            })
            .expect("load");
        let index = JarIndex::from_artifact(store.iter().next().expect("one").1).expect("index");

        let entry = index.entry("raw.bin").expect("entry");
        assert_eq!(entry.location.method, CompressionMethod::Stored);
        assert_eq!(
            index.read("raw.bin").expect("read"),
            b"uncompressed payload"
        );
    }

    #[test]
    fn reading_a_missing_entry_reports_not_found() {
        let (_dir, index) = index_of(&[("a.txt", b"a")]);
        let err = index.read("missing.txt").expect_err("must fail");
        assert!(matches!(err, JarError::EntryNotFound { .. }));
    }

    #[test]
    fn reading_a_missing_class_reports_class_not_found() {
        let (_dir, index) = index_of(&[("a.txt", b"a")]);
        let err = index.read_class("no.Such").expect_err("must fail");
        assert!(matches!(err, JarError::ClassNotFound { .. }));
    }

    #[test]
    fn entry_paths_are_case_and_slash_normalized_for_lookup() {
        let (_dir, index) = index_of(&[("dir/file.txt", b"x")]);

        assert!(index.entry("/dir/file.txt").is_some());
        assert!(index.entry("dir\\file.txt").is_some());
    }

    #[test]
    fn a_binary_class_name_is_none_for_resources() {
        let (_dir, index) = index_of(&[("notes.txt", b"x")]);
        assert!(index
            .entry("notes.txt")
            .expect("entry")
            .binary_class_name()
            .is_none());
    }

    #[test]
    fn a_corrupt_archive_is_an_error_not_an_empty_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.jar");
        std::fs::write(&path, b"this is not a zip file").expect("write");
        let raw = std::fs::read(&path).expect("read");
        let mut store = ArtifactStore::new();
        store
            .load(&crate::artifact::ArtifactSpec {
                name: "broken.jar".into(),
                payload_path: path,
                encoding: StorageEncoding::Identity,
                sha256_hex: crate::artifact::hex_lower(&sha2::Sha256::digest(&raw)),
                size_bytes: raw.len() as u64,
            })
            .expect("load");

        let err =
            JarIndex::from_artifact(store.iter().next().expect("one").1).expect_err("must reject");
        assert!(matches!(err, JarError::Open { .. }));
    }
}
