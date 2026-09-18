//! The virtual file system: artifact bytes, jar indexes, and the placeholder
//! paths that let a JVM address them.
//!
//! This is the module that makes "load a normal jar without writing it to
//! disk" work. It owns three things and keeps them consistent:
//!
//! - [`artifact::ArtifactStore`] — the bytes, verified.
//! - [`jar::JarIndex`] — what is inside each archive.
//! - [`hollow::HollowTree`] — the zero-length placeholders those bytes are
//!   addressed by.
//!
//! Unlike the predecessor, none of it is a process global. A [`VirtualFileSystem`]
//! is owned by a session and dropped with it, which is what allows two launches
//! in one process and makes integration testing possible at all.

pub mod hollow;
pub mod jar;
pub mod pathkey;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::artifact::{Artifact, ArtifactSpec, ArtifactStore};
use crate::vfs::hollow::{HollowFile, HollowTree};
use crate::vfs::jar::{JarError, JarIndex};
use crate::vfs::pathkey::{PathError, VirtualPath};

/// How an artifact participates in a launch. The role decides which
/// placeholder subdirectory it lands in, which keeps a game jar from ever
/// colliding with a library of the same file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactRole {
    /// The application or game jar.
    Game,
    /// A classpath dependency.
    Library,
    /// A mod jar.
    Mod,
    /// A memory-loaded native library.
    Native,
}

impl ArtifactRole {
    /// The placeholder subdirectory name for this role.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Library => "libraries",
            Self::Mod => "mods",
            Self::Native => "natives",
        }
    }
}

/// One artifact, indexed and given a placeholder path.
#[derive(Debug)]
pub struct MountedArtifact {
    role: ArtifactRole,
    /// The file name as the manifest gave it, used for the placeholder name.
    file_name: String,
    sha256_hex: String,
    hollow: HollowFile,
    /// `None` for artifacts that are not archives (a bare `.dll` payload).
    index: Option<JarIndex>,
}

impl MountedArtifact {
    /// The role this artifact was mounted under.
    #[must_use]
    pub fn role(&self) -> ArtifactRole {
        self.role
    }

    /// The original file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The verified SHA-256 hex.
    #[must_use]
    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }

    /// The placeholder path this artifact is addressed by.
    #[must_use]
    pub fn hollow(&self) -> &HollowFile {
        &self.hollow
    }

    /// The placeholder path, convenient for building system properties.
    #[must_use]
    pub fn path(&self) -> &VirtualPath {
        self.hollow.path()
    }

    /// The jar index, when this artifact is an archive.
    #[must_use]
    pub fn index(&self) -> Option<&JarIndex> {
        self.index.as_ref()
    }
}

/// An artifact mounted after launch, without requiring mutable VFS access.
#[derive(Debug, Clone)]
pub struct RuntimeMount {
    role: ArtifactRole,
    file_name: String,
    sha256_hex: String,
    hollow: HollowFile,
    bytes: Arc<[u8]>,
    index: Option<JarIndex>,
}

impl RuntimeMount {
    /// The role this artifact was mounted under.
    #[must_use]
    pub fn role(&self) -> ArtifactRole {
        self.role
    }

    /// The original file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The SHA-256 of the bytes.
    #[must_use]
    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }

    /// The zero-length placeholder path.
    #[must_use]
    pub fn path(&self) -> &VirtualPath {
        self.hollow.path()
    }

    /// The archive index, when this is an indexed jar.
    #[must_use]
    pub fn index(&self) -> Option<&JarIndex> {
        self.index.as_ref()
    }
}

#[derive(Debug, Default)]
struct RuntimeState {
    mounts: Vec<RuntimeMount>,
    by_path: HashMap<VirtualPath, usize>,
}

/// A session's virtual file system.
#[derive(Debug)]
pub struct VirtualFileSystem {
    artifacts: ArtifactStore,
    tree: HollowTree,
    /// Normalized placeholder path -> mounted artifact index.
    by_path: HashMap<VirtualPath, usize>,
    mounted: Vec<MountedArtifact>,
    /// Mutable overlay for artifacts injected after launch.
    runtime: parking_lot::RwLock<RuntimeState>,
}

impl VirtualFileSystem {
    /// Create a VFS whose placeholders live under `session_root`.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::ReparsePoint`] if the session root resolves through
    /// a reparse point — see [`pathkey::verify_session_dir`].
    pub fn create(session_root: impl AsRef<std::path::Path>) -> Result<Self, PathError> {
        Ok(Self {
            artifacts: ArtifactStore::new(),
            tree: HollowTree::create(session_root)?,
            by_path: HashMap::new(),
            mounted: Vec::new(),
            runtime: parking_lot::RwLock::new(RuntimeState::default()),
        })
    }

    /// Load, verify, index and mount one artifact.
    ///
    /// `index` must be false for non-archives such as native library payloads.
    ///
    /// # Errors
    ///
    /// Propagates artifact verification failures, and [`JarError`] when
    /// `index` is true and the artifact is not a usable archive.
    pub fn mount(
        &mut self,
        role: ArtifactRole,
        file_name: &str,
        spec: &ArtifactSpec,
        index: bool,
    ) -> Result<&MountedArtifact, MountError> {
        let sha256_hex = self
            .artifacts
            .load(spec)
            .map_err(|source| MountError::Artifact { source })?;

        self.mount_loaded(role, file_name, sha256_hex, index)
    }

    /// Index and mount bytes that never existed as a payload file.
    ///
    /// This is how a remapped jar enters the launch: its bytes come from the
    /// remapper process through a pipe, are stored under their digest, and the
    /// only filesystem representation is the usual zero-length placeholder.
    ///
    /// # Errors
    ///
    /// Returns [`MountError::Jar`] when `index` is true and the bytes are not
    /// a usable archive, and [`MountError::Path`] if the placeholder cannot be
    /// created.
    pub fn mount_memory(
        &mut self,
        role: ArtifactRole,
        file_name: &str,
        bytes: Vec<u8>,
        index: bool,
    ) -> Result<&MountedArtifact, MountError> {
        let sha256_hex = self.artifacts.insert_memory(bytes);
        self.mount_loaded(role, file_name, sha256_hex, index)
    }

    fn mount_loaded(
        &mut self,
        role: ArtifactRole,
        file_name: &str,
        sha256_hex: String,
        index: bool,
    ) -> Result<&MountedArtifact, MountError> {
        let artifact: &Artifact = self
            .artifacts
            .get(&sha256_hex)
            .ok_or_else(|| MountError::Internal("artifact vanished after load".into()))?;

        let virtual_len = artifact.len() as u64;

        let jar_index = if index {
            Some(
                JarIndex::from_artifact(artifact).map_err(|source| MountError::Jar {
                    file_name: file_name.to_string(),
                    source,
                })?,
            )
        } else {
            None
        };

        // The placeholder name must be unique within the role. Prefixing with
        // the content hash guarantees that two manifests that happen to share a
        // file name still get distinct placeholders, which is the invariant
        // `ZipFile$Source`'s path-keyed cache depends on.
        let placeholder_name = format!("{}-{file_name}", &sha256_hex[..16]);
        let hollow = self
            .tree
            .add_file(role.dir_name(), &placeholder_name, virtual_len)?
            .clone();

        let position = self.mounted.len();
        self.by_path.insert(hollow.path().clone(), position);
        self.mounted.push(MountedArtifact {
            role,
            file_name: file_name.to_string(),
            sha256_hex,
            hollow,
            index: jar_index,
        });

        Ok(self.mounted.last().expect("just pushed"))
    }

    /// Mount and index bytes through the runtime overlay.
    ///
    /// Unlike [`Self::mount_memory`], this takes `&self`, so it can be called
    /// after the VFS has been shared with the native hooks. The only
    /// filesystem artifact is the usual zero-length placeholder.
    ///
    /// # Errors
    ///
    /// Returns [`MountError::Jar`] when `index` is true and the bytes are not
    /// a usable archive, and [`MountError::Path`] if the placeholder cannot be
    /// created.
    pub fn mount_memory_runtime(
        &self,
        role: ArtifactRole,
        file_name: &str,
        bytes: Vec<u8>,
        index: bool,
    ) -> Result<RuntimeMount, MountError> {
        let digest: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&bytes).into();
        let sha256_hex = crate::artifact::hex_lower(&digest);

        let file_name = sanitize_file_name(file_name);
        let dir = self.tree.root().join(role.dir_name());
        std::fs::create_dir_all(&dir).map_err(|source| MountError::Path {
            source: PathError::Io {
                path: dir.clone(),
                source,
            },
        })?;
        let placeholder_name = format!("{}-{file_name}", &sha256_hex[..16]);
        let path = dir.join(placeholder_name);
        std::fs::write(&path, b"").map_err(|source| MountError::Path {
            source: PathError::Io {
                path: path.clone(),
                source,
            },
        })?;
        let normalized = VirtualPath::new(&path)?;

        let bytes: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
        let jar_index = if index {
            let artifact = crate::artifact::Artifact::from_memory(Arc::clone(&bytes), digest);
            Some(
                JarIndex::from_artifact(&artifact).map_err(|source| MountError::Jar {
                    file_name: file_name.to_string(),
                    source,
                })?,
            )
        } else {
            None
        };

        let mount = RuntimeMount {
            role,
            file_name: file_name.to_string(),
            sha256_hex,
            hollow: HollowFile::new_file(normalized.clone(), bytes.len() as u64),
            bytes,
            index: jar_index,
        };

        let mut state = self.runtime.write();
        let position = state.mounts.len();
        state.by_path.insert(normalized, position);
        state.mounts.push(mount.clone());
        Ok(mount)
    }

    /// True when a path resolves to either a startup or runtime artifact.
    #[must_use]
    pub fn contains_path(&self, path: &str) -> bool {
        let Ok(normalized) = VirtualPath::new(path) else {
            return false;
        };
        if self.by_path.contains_key(&normalized) {
            return true;
        }
        self.runtime.read().by_path.contains_key(&normalized)
    }

    /// Runtime-mounted artifacts, in mount order.
    #[must_use]
    pub fn runtime_mounts(&self) -> Vec<RuntimeMount> {
        self.runtime.read().mounts.clone()
    }

    /// The bytes behind a path, including runtime mounts.
    #[must_use]
    pub fn artifact_bytes_by_path(&self, path: &str) -> Option<Arc<[u8]>> {
        let normalized = VirtualPath::new(path).ok()?;
        if let Some(position) = self.by_path.get(&normalized) {
            let mounted = self.mounted.get(*position)?;
            return self.artifact_bytes(mounted);
        }
        let state = self.runtime.read();
        let position = *state.by_path.get(&normalized)?;
        state
            .mounts
            .get(position)
            .map(|mount| Arc::clone(&mount.bytes))
    }
    /// Create the real, empty mods directory the loader expects, and return it.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::Io`] if it cannot be created.
    pub fn create_mods_directory(&mut self) -> Result<&HollowFile, PathError> {
        self.tree.add_directory("mods-folder")
    }

    /// Every mounted artifact, in mount order.
    #[must_use]
    pub fn mounted(&self) -> &[MountedArtifact] {
        &self.mounted
    }

    /// Mounted artifacts with the given role, in mount order.
    pub fn with_role(&self, role: ArtifactRole) -> impl Iterator<Item = &MountedArtifact> {
        self.mounted.iter().filter(move |m| m.role == role)
    }

    /// Resolve a path the hooks received back to the artifact serving it.
    ///
    /// This is the single lookup every native hook funnels through. Keeping it
    /// in one place is what makes the hook set auditable: if a read is not
    /// resolved here, it is not served from memory.
    #[must_use]
    pub fn resolve_path(&self, path: &str) -> Option<&MountedArtifact> {
        let normalized = VirtualPath::new(path).ok()?;
        let position = *self.by_path.get(&normalized)?;
        self.mounted.get(position)
    }

    /// Read an entry from whichever mounted archive contains it.
    ///
    /// Later mounts shadow earlier ones, mirroring classpath semantics where
    /// the first hit wins — here the *last* mount is the most specific, since
    /// mods are mounted after libraries.
    ///
    /// # Errors
    ///
    /// Returns [`JarError`] only if a matching entry exists but cannot be read.
    pub fn read_class(&self, binary_name: &str) -> Result<Option<Vec<u8>>, JarError> {
        for mounted in self.mounted.iter().rev() {
            let Some(index) = &mounted.index else {
                continue;
            };
            if index.class_entry(binary_name).is_some() {
                return index.read_class(binary_name).map(Some);
            }
        }
        for mounted in self.runtime.read().mounts.iter().rev() {
            let Some(index) = &mounted.index else {
                continue;
            };
            if index.class_entry(binary_name).is_some() {
                return index.read_class(binary_name).map(Some);
            }
        }
        Ok(None)
    }

    /// Read a resource from whichever mounted archive contains it.
    ///
    /// # Errors
    ///
    /// Returns [`JarError`] only if a matching entry exists but cannot be read.
    pub fn read_resource(&self, name: &str) -> Result<Option<Vec<u8>>, JarError> {
        for mounted in self.mounted.iter().rev() {
            let Some(index) = &mounted.index else {
                continue;
            };
            if index.entry(name).is_some() {
                return index.read(name).map(Some);
            }
        }
        for mounted in self.runtime.read().mounts.iter().rev() {
            let Some(index) = &mounted.index else {
                continue;
            };
            if index.entry(name).is_some() {
                return index.read(name).map(Some);
            }
        }
        Ok(None)
    }

    /// The artifact bytes for a mounted artifact.
    #[must_use]
    pub fn artifact_bytes(&self, mounted: &MountedArtifact) -> Option<Arc<[u8]>> {
        self.artifacts
            .get(mounted.sha256_hex())
            .map(|a| Arc::clone(a.bytes()))
    }

    /// Snapshot of every placeholder's on-disk size, for the leak audit.
    #[must_use]
    pub fn disk_footprint(&self) -> Vec<(String, u64)> {
        let mut out = self.tree.snapshot();
        for mount in &self.runtime.read().mounts {
            let len = std::fs::metadata(mount.path().to_path_buf())
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            out.push((mount.path().to_string(), len));
        }
        out.sort();
        out
    }

    /// The session root directory.
    #[must_use]
    pub fn session_root(&self) -> &std::path::Path {
        self.tree.root()
    }

    /// The raw artifact store, for the native-library registry.
    #[must_use]
    pub fn artifacts(&self) -> &ArtifactStore {
        &self.artifacts
    }

    /// Number of mounted artifacts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mounted.len()
    }

    /// True when nothing is mounted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mounted.is_empty()
    }
}

/// Errors from mounting an artifact.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error(transparent)]
    Artifact {
        #[from]
        source: crate::artifact::ArtifactError,
    },

    #[error("cannot index {file_name}: {source}")]
    Jar {
        file_name: String,
        #[source]
        source: JarError,
    },

    #[error(transparent)]
    Path {
        #[from]
        source: PathError,
    },

    #[error("internal error: {0}")]
    Internal(String),
}

/// Build a placeholder name from a manifest file name, keeping only characters
/// that are safe in a path component.
#[must_use]
pub fn sanitize_file_name(name: &str) -> String {
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if base.is_empty() {
        "artifact".to_string()
    } else {
        base
    }
}

/// Convenience: build an [`ArtifactSpec`] for an uncompressed payload, hashing
/// it eagerly. Useful for tests and for payloads shipped inside the binary.
///
/// # Errors
///
/// Returns [`std::io::Error`] if the payload cannot be read.
pub fn identity_spec(
    name: impl Into<String>,
    payload_path: PathBuf,
) -> Result<ArtifactSpec, std::io::Error> {
    use sha2::{Digest, Sha256};

    let bytes = std::fs::read(&payload_path)?;
    let sha256_hex = crate::artifact::hex_lower(&Sha256::digest(&bytes));
    Ok(ArtifactSpec {
        name: name.into(),
        payload_path,
        encoding: crate::artifact::StorageEncoding::Identity,
        sha256_hex,
        size_bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a jar whose entries are the given (name, bytes) pairs.
    fn make_jar(dir: &std::path::Path, file_name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(file_name);
        let file = std::fs::File::create(&path).expect("create jar");
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            zw.start_file(*name, opts).expect("start_file");
            zw.write_all(data).expect("write");
        }
        zw.finish().expect("finish");
        path
    }

    fn vfs_with_tempdir() -> (tempfile::TempDir, VirtualFileSystem) {
        let dir = tempfile::tempdir().expect("tempdir");
        let vfs = VirtualFileSystem::create(dir.path().join("session")).expect("create");
        (dir, vfs)
    }

    #[test]
    fn mounting_a_jar_indexes_its_classes() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let jar = make_jar(dir.path(), "game.jar", &[("a/b/C.class", b"bytecode")]);
        let spec = identity_spec("game.jar", jar).expect("spec");

        let mounted = vfs
            .mount(ArtifactRole::Game, "game.jar", &spec, true)
            .expect("mount");

        assert_eq!(mounted.index().expect("indexed").class_count(), 1);
        assert_eq!(
            vfs.read_class("a.b.C").expect("read").expect("present"),
            b"bytecode"
        );
    }

    #[test]
    fn memory_bytes_can_be_indexed_without_a_payload_file() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let jar = make_jar(dir.path(), "remapped.jar", &[("a/b/C.class", b"remapped")]);
        let bytes = std::fs::read(&jar).expect("read jar");
        std::fs::remove_file(&jar).expect("remove source jar");

        let (class_count, sha256_hex) = {
            let mounted = vfs
                .mount_memory(ArtifactRole::Game, "remapped.jar", bytes, true)
                .expect("mount memory bytes");
            (
                mounted.index().expect("indexed").class_count(),
                mounted.sha256_hex().to_owned(),
            )
        };

        assert_eq!(class_count, 1);
        assert_eq!(
            vfs.read_class("a.b.C").expect("read").expect("present"),
            b"remapped"
        );
        assert_eq!(
            vfs.artifacts().get(&sha256_hex).expect("artifact").source(),
            std::path::Path::new(""),
            "an in-memory artifact has no payload source"
        );
    }
    #[test]
    fn runtime_mounts_are_visible_through_shared_vfs_state() {
        let (dir, vfs) = vfs_with_tempdir();
        let jar = make_jar(
            dir.path(),
            "runtime-mod.jar",
            &[("mod/Entry.class", b"entry"), ("fabric.mod.json", b"{}")],
        );
        let bytes = std::fs::read(&jar).expect("read jar");
        std::fs::remove_file(&jar).expect("remove source");

        let shared = Arc::new(vfs);
        let mounted = shared
            .mount_memory_runtime(ArtifactRole::Mod, "runtime-mod.jar", bytes, true)
            .expect("runtime mount");
        let path = mounted.path().to_string();

        assert!(shared.contains_path(&path));
        assert_eq!(
            shared
                .read_class("mod.Entry")
                .expect("read class")
                .expect("class present"),
            b"entry"
        );
        assert_eq!(
            shared
                .read_resource("fabric.mod.json")
                .expect("read resource")
                .expect("resource present"),
            b"{}"
        );
        assert_eq!(std::fs::metadata(path).expect("stat placeholder").len(), 0);
        assert_eq!(shared.runtime_mounts().len(), 1);
    }
    #[test]
    fn a_mounted_artifact_gets_a_zero_length_placeholder() {
        let (dir, mut vfs) = vfs_with_tempdir();
        // Incompressible content, so the jar's on-disk size is substantial and
        // the placeholder's zero length is a meaningful observation.
        let payload: Vec<u8> = (0..8192u32).flat_map(u32::to_le_bytes).collect();
        let jar = make_jar(dir.path(), "game.jar", &[("a/C.class", &payload)]);
        let spec = identity_spec("game.jar", jar).expect("spec");
        let jar_len = std::fs::metadata(&spec.payload_path).expect("stat").len();

        let mounted = vfs
            .mount(ArtifactRole::Game, "game.jar", &spec, true)
            .expect("mount");

        // The bytes exist only in memory; the path on disk is empty.
        assert_eq!(
            std::fs::metadata(mounted.path().to_path_buf())
                .expect("stat")
                .len(),
            0,
            "the placeholder must be zero bytes on disk"
        );
        assert_eq!(
            mounted.hollow().virtual_len(),
            jar_len,
            "reads must report the real archive length, not the placeholder's"
        );
    }

    #[test]
    fn paths_resolve_back_to_the_artifact_serving_them() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let jar = make_jar(dir.path(), "game.jar", &[("a/C.class", b"x")]);
        let spec = identity_spec("game.jar", jar).expect("spec");
        let path = vfs
            .mount(ArtifactRole::Game, "game.jar", &spec, true)
            .expect("mount")
            .path()
            .as_str()
            .to_string();

        let resolved = vfs.resolve_path(&path).expect("resolves");
        assert_eq!(resolved.file_name(), "game.jar");
    }

    /// The one-placeholder-per-artifact rule, from the V1 probe: two jars with
    /// the same file name must still get distinct placeholder paths.
    #[test]
    fn artifacts_with_the_same_name_get_distinct_placeholders() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let a = make_jar(dir.path(), "a.jar", &[("a/C.class", b"first")]);
        let b = make_jar(dir.path(), "b.jar", &[("a/C.class", b"second")]);

        let pa = vfs
            .mount(
                ArtifactRole::Mod,
                "same.jar",
                &identity_spec("s", a).expect("spec"),
                true,
            )
            .expect("mount")
            .path()
            .clone();
        let pb = vfs
            .mount(
                ArtifactRole::Mod,
                "same.jar",
                &identity_spec("s", b).expect("spec"),
                true,
            )
            .expect("mount")
            .path()
            .clone();

        assert_ne!(pa, pb);
    }

    #[test]
    fn later_mounts_shadow_earlier_ones_like_a_classpath() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let base = make_jar(dir.path(), "base.jar", &[("a/C.class", b"base")]);
        let over = make_jar(dir.path(), "over.jar", &[("a/C.class", b"override")]);

        vfs.mount(
            ArtifactRole::Library,
            "base.jar",
            &identity_spec("base", base).expect("spec"),
            true,
        )
        .expect("mount");
        vfs.mount(
            ArtifactRole::Mod,
            "over.jar",
            &identity_spec("over", over).expect("spec"),
            true,
        )
        .expect("mount");

        assert_eq!(
            vfs.read_class("a.C").expect("read").expect("present"),
            b"override",
            "the most recently mounted archive wins"
        );
    }

    #[test]
    fn a_missing_class_reads_as_none_rather_than_an_error() {
        let (_dir, vfs) = vfs_with_tempdir();
        assert!(vfs.read_class("no.Such").expect("read").is_none());
    }

    #[test]
    fn non_archives_mount_without_an_index() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let payload = dir.path().join("native.dll");
        std::fs::write(&payload, b"MZ fake pe").expect("write");

        let mounted = vfs
            .mount(
                ArtifactRole::Native,
                "native.dll",
                &identity_spec("native", payload).expect("spec"),
                false,
            )
            .expect("mount");

        assert!(mounted.index().is_none());
    }

    #[test]
    fn the_disk_footprint_is_all_zeroes() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let jar = make_jar(dir.path(), "game.jar", &[("a/C.class", &[1u8; 4096])]);
        vfs.mount(
            ArtifactRole::Game,
            "game.jar",
            &identity_spec("game", jar).expect("spec"),
            true,
        )
        .expect("mount");

        let footprint = vfs.disk_footprint();

        assert!(!footprint.is_empty());
        assert!(
            crate::vfs::hollow::all_placeholders_empty(&footprint),
            "no artifact byte may reach disk"
        );
    }

    #[test]
    fn roles_filter_mounts() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let g = make_jar(dir.path(), "g.jar", &[("a/C.class", b"g")]);
        let l = make_jar(dir.path(), "l.jar", &[("b/D.class", b"l")]);
        vfs.mount(
            ArtifactRole::Game,
            "g.jar",
            &identity_spec("g", g).expect("spec"),
            true,
        )
        .expect("mount");
        vfs.mount(
            ArtifactRole::Library,
            "l.jar",
            &identity_spec("l", l).expect("spec"),
            true,
        )
        .expect("mount");

        assert_eq!(vfs.with_role(ArtifactRole::Game).count(), 1);
        assert_eq!(vfs.with_role(ArtifactRole::Library).count(), 1);
        assert_eq!(vfs.with_role(ArtifactRole::Mod).count(), 0);
    }

    #[test]
    fn resources_are_readable_across_mounts() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let jar = make_jar(
            dir.path(),
            "game.jar",
            &[("version.json", br#"{"id":"1.21.4"}"#)],
        );
        vfs.mount(
            ArtifactRole::Game,
            "game.jar",
            &identity_spec("game", jar).expect("spec"),
            true,
        )
        .expect("mount");

        let bytes = vfs
            .read_resource("version.json")
            .expect("read")
            .expect("present");
        assert_eq!(bytes, br#"{"id":"1.21.4"}"#);
    }

    #[test]
    fn an_unreadable_archive_fails_the_mount_rather_than_mounting_empty() {
        let (dir, mut vfs) = vfs_with_tempdir();
        let broken = dir.path().join("broken.jar");
        std::fs::write(&broken, b"not a zip").expect("write");

        let err = vfs
            .mount(
                ArtifactRole::Game,
                "broken.jar",
                &identity_spec("broken", broken).expect("spec"),
                true,
            )
            .expect_err("must reject");

        assert!(matches!(err, MountError::Jar { .. }));
        assert!(
            vfs.is_empty(),
            "a failed mount must not leave partial state"
        );
    }

    #[test]
    fn file_names_are_sanitized_for_path_use() {
        assert_eq!(sanitize_file_name("my mod v2.jar"), "my_mod_v2.jar");
        assert_eq!(sanitize_file_name(""), "artifact");
    }
}
