//! Virtual paths that give in-memory bytes a stable filesystem-shaped address.
//!
//! Java's archive APIs insist on ordinary absolute paths. The native open/read
//! layer therefore treats these registry entries as virtual regular files:
//! open returns a synthetic handle, metadata reports the in-memory length, and
//! reads copy bytes from the process. No artifact directory entry is created on
//! disk.
//!
//! Two rules keep jar identity intact:
//!
//! 1. **One virtual path per artifact.** `ZipFile$Source` caches sources by
//!    path attributes, so two virtual jars sharing a path would alias each
//!    other's contents.
//! 2. **Never consult the disk.** The real length lives in memory; metadata and
//!    read hooks must synthesize it or `ZipFile` gives up immediately.

use std::path::{Path, PathBuf};

use crate::vfs::pathkey::{PathError, VirtualPath};

/// A virtual path plus the size its reads should report.
#[derive(Debug, Clone)]
pub struct HollowFile {
    path: VirtualPath,
    virtual_len: u64,
    /// Set when this virtual node is a real directory (used for the Fabric mods directory).
    is_directory: bool,
}

impl HollowFile {
    /// Reconstruct a virtual-file descriptor created outside the owning
    /// `HollowTree` (used by the runtime-mount overlay).
    pub(crate) fn new_file(path: VirtualPath, virtual_len: u64) -> Self {
        Self {
            path,
            virtual_len,
            is_directory: false,
        }
    }

    /// The normalized virtual path, which is what the hooks match on.
    #[must_use]
    pub fn path(&self) -> &VirtualPath {
        &self.path
    }

    /// The length reads should report, which is *not* the on-disk length.
    #[must_use]
    pub fn virtual_len(&self) -> u64 {
        self.virtual_len
    }

    /// True when this node stands in for a directory.
    #[must_use]
    pub fn is_directory(&self) -> bool {
        self.is_directory
    }
}

/// Registers virtual paths under a session directory and removes the directory on drop.
///
/// Owning the cleanup in a `Drop` impl means a panicking or early-returning
/// launch still tidies up, rather than leaving stale virtual-path registry state
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

    /// Register a virtual file without materializing any file or parent directory.
    ///
    /// The native open layer hands out a synthetic handle for this path and reads
    /// from memory. Only real directories created explicitly by
    /// [`HollowTree::add_directory`] may appear below the session root.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::Relative`] if the derived path is not absolute.
    pub fn add_file(
        &mut self,
        role: &str,
        name: &str,
        virtual_len: u64,
    ) -> Result<&HollowFile, PathError> {
        let path = self.root.join(role).join(name);
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

    /// Every virtual path created so far.
    #[must_use]
    pub fn files(&self) -> &[HollowFile] {
        &self.files
    }

    /// Look up a virtual path by its normalized key.
    #[must_use]
    pub fn find(&self, path: &VirtualPath) -> Option<&HollowFile> {
        self.files.iter().find(|f| &f.path == path)
    }

    /// Every regular file below the session root.
    ///
    /// This scan is deliberately not tied to the registry: a stale or leaked
    /// artifact file is a materialization failure, whether empty or non-empty.
    /// Explicit empty directories do not violate the invariant.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        collect_files(&self.root, &mut out);
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

fn collect_files(dir: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_files(&path, out);
        } else if file_type.is_file() {
            let len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            out.push((path.to_string_lossy().into_owned(), len));
        }
    }
}

/// True only when no materialized file exists.
///
/// Empty and non-empty files are both failures: the invariant is that artifact
/// paths have no disk file at all, not merely that their disk file is empty.
#[must_use]
pub fn no_materialized_files(snapshot: &[(String, u64)]) -> bool {
    snapshot.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_virtual_file_is_not_materialized_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tree = HollowTree::create(dir.path().join("session")).expect("create");

        let file = tree
            .add_file("game", "1.21.4.jar", 12_345_678)
            .expect("add")
            .clone();

        assert!(
            !file.path().to_path_buf().exists(),
            "virtual artifact paths must not have a disk file"
        );
        assert_eq!(
            file.virtual_len(),
            12_345_678,
            "the virtual size is what reads report"
        );
    }

    #[test]
    fn virtual_paths_with_different_roles_do_not_alias() {
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
    fn the_session_root_contains_no_materialized_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tree = HollowTree::create(dir.path().join("session")).expect("create");
        tree.add_file("game", "a.jar", 999).expect("add");
        tree.add_file("mods", "b.jar", 999).expect("add");

        let snapshot = tree.snapshot();

        assert!(snapshot.is_empty());
        assert!(no_materialized_files(&snapshot));
    }

    #[test]
    fn empty_and_nonempty_regular_files_are_detected_by_the_session_audit() {
        let snapshot = vec![("/x/a.jar".to_string(), 0), ("/x/b.jar".to_string(), 42)];
        assert!(!no_materialized_files(&snapshot));
    }

    #[test]
    fn find_locates_a_virtual_path_by_its_key() {
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
    fn a_directory_virtual_path_is_marked_as_a_directory() {
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
