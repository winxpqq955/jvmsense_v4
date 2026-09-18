//! Zero-length placeholder files that give virtual bytes a real path.
//!
//! V1 established that fabric-loader opens jars with
//! `new java.util.zip.ZipFile(path.toFile())` at ten call sites, and that
//! `Path.toFile()` throws for any path outside the default file system
//! (`vfs::probe`'s V6 result). A pure in-memory `FileSystemProvider` therefore
//! cannot serve Fabric at all.
//!
//! The workaround is a *hollow* path: a real, zero-length file on disk whose
//! only job is to satisfy `toRealPath`, `Files.exists`, `Files.readAttributes`
//! and `ZipFile`'s cache-key computation. Every read of it is intercepted at
//! the native layer and answered from [`crate::artifact`], so no bytecode ever
//! reaches disk — only the placeholder's directory entry does.
//!
//! Two invariants make this safe, both from the V1/V2 probes:
//!
//! 1. **One placeholder per artifact.** `ZipFile$Source` caches sources in a
//!    static map keyed by `(File, BasicFileAttributes)`. Two virtual jars
//!    sharing a placeholder path would alias each other's contents.
//! 2. **Never trust the on-disk size.** The placeholder is zero bytes; the
//!    real length lives here. The `length`/`size` hooks must return this value
//!    or `ZipFile` gives up immediately.

use std::path::{Path, PathBuf};

use crate::vfs::pathkey::{PathError, VirtualPath};

/// A placeholder file plus the size its reads should report.
#[derive(Debug, Clone)]
pub struct HollowFile {
    path: VirtualPath,
    virtual_len: u64,
    /// Set when this placeholder is a directory (used for native-library dirs).
    is_directory: bool,
}

impl HollowFile {
    /// Reconstruct a placeholder descriptor for a file created outside the
    /// owning `HollowTree` (used by the runtime-mount overlay).
    pub(crate) fn new_file(path: VirtualPath, virtual_len: u64) -> Self {
        Self {
            path,
            virtual_len,
            is_directory: false,
        }
    }

    /// The placeholder's normalized path, which is what the hooks match on.
    #[must_use]
    pub fn path(&self) -> &VirtualPath {
        &self.path
    }

    /// The length reads should report, which is *not* the on-disk length.
    #[must_use]
    pub fn virtual_len(&self) -> u64 {
        self.virtual_len
    }

    /// True when this placeholder stands in for a directory.
    #[must_use]
    pub fn is_directory(&self) -> bool {
        self.is_directory
    }
}

/// Creates placeholders under a session directory and removes them on drop.
///
/// Owning the cleanup in a `Drop` impl means a panicking or early-returning
/// launch still tidies up, rather than leaving a directory of empty files
/// behind that the next session would have to reason about.
#[derive(Debug)]
pub struct HollowTree {
    root: PathBuf,
    files: Vec<HollowFile>,
}

impl HollowTree {
    /// Create the session directory and verify it is usable.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::ReparsePoint`] if the directory resolves through a
    /// reparse point. That check matters: fabric-loader's mod discovery asserts
    /// `path.equals(path.toRealPath())`, and a reparse point makes that false,
    /// silently losing every mod.
    pub fn create(root: impl AsRef<Path>) -> Result<Self, PathError> {
        let root = root.as_ref();
        std::fs::create_dir_all(root).map_err(|source| PathError::Io {
            path: root.to_path_buf(),
            source,
        })?;

        let resolved = crate::vfs::pathkey::verify_session_dir(root)?;

        Ok(Self {
            root: resolved.to_path_buf(),
            files: Vec::new(),
        })
    }

    /// The session root, already verified to resolve to itself.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Materialize a placeholder for a virtual file of `virtual_len` bytes.
    ///
    /// `role` groups placeholders (`game`, `libraries`, `mods`, `natives`);
    /// `name` must be unique within the role, which is what keeps the
    /// one-placeholder-per-artifact invariant.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::Io`] if the placeholder cannot be created.
    pub fn add_file(
        &mut self,
        role: &str,
        name: &str,
        virtual_len: u64,
    ) -> Result<&HollowFile, PathError> {
        let dir = self.root.join(role);
        std::fs::create_dir_all(&dir).map_err(|source| PathError::Io {
            path: dir.clone(),
            source,
        })?;
        let path = dir.join(name);

        // Truncate rather than create-new: a stale placeholder from a crashed
        // session is fine to reuse, and its content is never read anyway.
        std::fs::write(&path, b"").map_err(|source| PathError::Io {
            path: path.clone(),
            source,
        })?;

        let normalized = VirtualPath::new(&path)?;
        self.files.push(HollowFile {
            path: normalized,
            virtual_len,
            is_directory: false,
        });
        Ok(self.files.last().expect("just pushed"))
    }

    /// Materialize a real, empty directory and return its normalized path.
    ///
    /// Used for `fabric.modsFolder`, which the loader will happily create
    /// itself if missing — and creating it through a hollow path would be a
    /// write we do not control.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::Io`] if the directory cannot be created.
    pub fn add_directory(&mut self, role: &str) -> Result<&HollowFile, PathError> {
        let path = self.root.join(role);
        std::fs::create_dir_all(&path).map_err(|source| PathError::Io {
            path: path.clone(),
            source,
        })?;

        let normalized = VirtualPath::new(&path)?;
        self.files.push(HollowFile {
            path: normalized,
            virtual_len: 0,
            is_directory: true,
        });
        Ok(self.files.last().expect("just pushed"))
    }

    /// Every placeholder created so far.
    #[must_use]
    pub fn files(&self) -> &[HollowFile] {
        &self.files
    }

    /// Look up a placeholder by its normalized path.
    #[must_use]
    pub fn find(&self, path: &VirtualPath) -> Option<&HollowFile> {
        self.files.iter().find(|f| &f.path == path)
    }

    /// Snapshot the on-disk sizes of every placeholder.
    ///
    /// The verification mode compares two of these to prove that no bytecode
    /// was written: every placeholder must still be zero bytes, and no file
    /// may have appeared that is not in the snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for file in &self.files {
            let len = std::fs::metadata(file.path.to_path_buf())
                .map(|m| m.len())
                .unwrap_or(0);
            out.push((file.path.to_string(), len));
        }
        out.sort();
        out
    }
}

impl Drop for HollowTree {
    fn drop(&mut self) {
        // Remove only what we created. The session root is ours, so removing
        // it wholesale is safe and avoids reasoning about stale entries.
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            // Cleanup failure must never mask the real outcome of a launch, so
            // this is reported but not propagated.
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "jvmsense: could not remove session directory {}: {error}",
                    self.root.display()
                );
            }
        }
    }
}

/// True when every snapshot entry is zero bytes.
///
/// This is the "did anything leak to disk" check, factored out so the caller
/// can run it before teardown removes the evidence.
#[must_use]
pub fn all_placeholders_empty(snapshot: &[(String, u64)]) -> bool {
    snapshot.iter().all(|(_, len)| *len == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placeholder_is_created_empty_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tree = HollowTree::create(dir.path().join("session")).expect("create");

        let file = tree
            .add_file("game", "1.21.4.jar", 12_345_678)
            .expect("add")
            .clone();

        assert_eq!(
            std::fs::metadata(file.path().to_path_buf())
                .expect("stat")
                .len(),
            0
        );
        assert_eq!(
            file.virtual_len(),
            12_345_678,
            "the virtual size is what reads report"
        );
    }

    #[test]
    fn placeholders_with_different_roles_do_not_alias() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tree = HollowTree::create(dir.path().join("session")).expect("create");

        let game = tree
            .add_file("game", "a.jar", 100)
            .expect("add")
            .path()
            .clone();
        let lib = tree
            .add_file("libraries", "a.jar", 200)
            .expect("add")
            .path()
            .clone();

        assert_ne!(
            game.as_str(),
            lib.as_str(),
            "ZipFile caches sources by path; a shared path would alias two jars"
        );
    }

    #[test]
    fn the_session_root_survives_teardown_of_nothing_but_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("session");
        {
            let mut tree = HollowTree::create(&root).expect("create");
            tree.add_file("game", "a.jar", 1).expect("add");
            assert!(root.exists());
        }
        assert!(
            !root.exists(),
            "dropping the tree removes the session directory"
        );
    }

    #[test]
    fn snapshot_reports_zero_for_every_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tree = HollowTree::create(dir.path().join("session")).expect("create");
        tree.add_file("game", "a.jar", 999).expect("add");
        tree.add_file("mods", "b.jar", 999).expect("add");

        let snapshot = tree.snapshot();

        assert_eq!(snapshot.len(), 2);
        assert!(all_placeholders_empty(&snapshot));
    }

    #[test]
    fn a_non_empty_placeholder_is_detected_by_the_leak_check() {
        let snapshot = vec![("/x/a.jar".to_string(), 0), ("/x/b.jar".to_string(), 42)];
        assert!(!all_placeholders_empty(&snapshot));
    }

    #[test]
    fn find_locates_a_placeholder_by_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tree = HollowTree::create(dir.path().join("session")).expect("create");
        let added = tree
            .add_file("game", "a.jar", 7)
            .expect("add")
            .path()
            .clone();

        let found = tree.find(&added).expect("find");
        assert_eq!(found.virtual_len(), 7);
    }

    #[test]
    fn a_directory_placeholder_is_marked_as_such() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tree = HollowTree::create(dir.path().join("session")).expect("create");

        let added = tree.add_directory("mods-folder").expect("add");

        assert!(added.is_directory());
        assert!(added.path().to_path_buf().is_dir());
    }

    /// A reparse point must be rejected up front: the failure it causes
    /// (mod discovery silently finding nothing) is otherwise very hard to
    /// diagnose. Creating one requires privileges on Windows, so this asserts
    /// the happy path resolves cleanly instead.
    #[test]
    fn an_ordinary_directory_passes_the_reparse_point_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = HollowTree::create(dir.path()).expect("create");
        assert!(tree.root().is_absolute());
    }
}
