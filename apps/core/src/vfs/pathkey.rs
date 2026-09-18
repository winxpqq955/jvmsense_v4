//! Path normalization for the virtual file system.
//!
//! Every virtual artifact is keyed by a normalized absolute path. Getting this
//! wrong is silent and expensive: the native hooks match on these strings, so a
//! mismatch means a read falls through to the 0-byte placeholder and the
//! application sees an empty file.
//!
//! Two Windows-specific rules, both established by the V1/V2 probes:
//!
//! - **Case-insensitive.** Windows resolves `C:\Foo\Bar.jar` and
//!   `c:\foo\bar.jar` to the same file. Java may hand us either, so lookup
//!   lowercases.
//! - **No `\\?\` prefix.** `std::fs::canonicalize` always adds the
//!   extended-length prefix on Windows, while Java's `Path.toRealPath()`
//!   returns the path exactly as constructed (V2 measured this). Since it is
//!   fabric-loader's `toRealPath` that drives mod discovery, we normalize the
//!   *Java* way and strip the prefix if it appears.

use std::path::{Component, Path, PathBuf};

/// A normalized absolute path, used as the key for every virtual artifact.
///
/// Constructing one is the only way to look something up in the registry, so
/// the normalization rules cannot be forgotten at a call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VirtualPath(String);

impl VirtualPath {
    /// Normalize `path` into a lookup key.
    ///
    /// The path must be absolute; a relative path has no stable meaning once
    /// the process working directory can change, which is exactly the trap the
    /// predecessor fell into.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::Relative`] for a relative path.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, PathError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(PathError::Relative {
                path: path.to_path_buf(),
            });
        }
        Ok(Self(normalize_str(&path.to_string_lossy())))
    }

    /// The normalized string, suitable for handing to Java or comparing.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path as a `PathBuf`. Always absolute and already normalized.
    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }

    /// True when `other`, after normalization, names the same file.
    #[must_use]
    pub fn matches(&self, other: &str) -> bool {
        normalize_str(other) == self.0
    }
}

impl std::fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Normalize a path string the way Java's `toRealPath()` would.
///
/// - unify separators to `\`
/// - collapse `.` and `..` components
/// - drop a trailing separator (except for a bare drive root)
/// - strip the `\\?\` extended-length prefix
/// - lowercase (Windows is case-insensitive)
#[must_use]
pub fn normalize_str(raw: &str) -> String {
    let unified = raw.replace('/', "\\");

    // `\\?\C:\...` and `\\?\UNC\server\share` are the two extended forms.
    let without_prefix = if let Some(rest) = unified.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = unified.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        unified
    };

    let collapsed = collapse_components(&without_prefix);

    // Windows is case-insensitive; lowercase so lookups are too.
    collapsed.to_lowercase()
}

/// Resolve `.` and `..` textually, and drop a redundant trailing separator.
///
/// This is deliberately textual rather than filesystem-based: the path may
/// refer to a placeholder that does not exist yet, and we must not touch disk
/// (or a reparse point) to compute a key.
fn collapse_components(path: &str) -> String {
    // Preserve a drive prefix (`C:`) or a UNC prefix (`\\server\share`).
    let (prefix, rest) = split_prefix(path);

    let mut parts: Vec<&str> = Vec::new();
    for component in rest.split('\\') {
        match component {
            "" | "." => {}
            ".." => {
                // Popping past the root is a no-op, as on Windows.
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let mut out = prefix;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 || !out.is_empty() {
            out.push('\\');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('\\');
    }
    out
}

/// Split a drive (`C:`) or UNC (`\\server\share`) prefix off a path.
fn split_prefix(path: &str) -> (String, &str) {
    let bytes = path.as_bytes();

    // UNC: \\server\share\...
    if path.starts_with(r"\\") {
        let mut seen = 0usize;
        for (i, b) in bytes.iter().enumerate().skip(2) {
            if *b == b'\\' {
                seen += 1;
                if seen == 2 {
                    return (path[..i].to_string(), &path[i + 1..]);
                }
            }
        }
        return (path.to_string(), "");
    }

    // Drive: C:\...
    if bytes.len() >= 2 && bytes[1] == b':' {
        let drive = &path[..2];
        let rest = path[2..].strip_prefix('\\').unwrap_or(&path[2..]);
        return (drive.to_string(), rest);
    }

    (String::new(), path)
}

/// Verify that a directory is safe to host a session.
///
/// A reparse point (OneDrive folder, Dev Drive, symlinked temp) changes what
/// `toRealPath()` returns, which silently breaks the path-equality assertions
/// fabric-loader's mod discovery relies on. Checking costs one syscall and
/// turns a confusing runtime failure into a clear startup error.
///
/// # Errors
///
/// Returns [`PathError::ReparsePoint`] when `dir` is one, or
/// [`PathError::NotResolved`] when the normalized path does not round-trip.
pub fn verify_session_dir(dir: &Path) -> Result<VirtualPath, PathError> {
    let canonical = dunce::canonicalize(dir).map_err(|source| PathError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    // Compare the *normalized* forms of both sides. Comparing a normalized
    // path against a raw canonical one can never succeed, because normalization
    // lowercases and canonicalization preserves case.
    let resolved = normalize_str(&canonical.to_string_lossy());
    let requested = normalize_str(&dir.to_string_lossy());
    if resolved != requested {
        return Err(PathError::ReparsePoint {
            path: dir.to_path_buf(),
        });
    }

    VirtualPath::new(&canonical)
}

/// Errors from path normalization.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path must be absolute: {path}")]
    Relative { path: PathBuf },

    #[error("session directory is a reparse point (OneDrive, Dev Drive, or a symlink): {path}")]
    ReparsePoint { path: PathBuf },

    #[error("session directory does not resolve to itself: {path}")]
    NotResolved { path: PathBuf },

    #[error("cannot resolve {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Count the components that make up `path`, ignoring `.` and a trailing
/// separator. Exposed because the jar indexer needs to synthesize parent
/// directory entries with the same notion of hierarchy.
#[must_use]
pub fn component_depth(path: &Path) -> usize {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_slashes_are_unified() {
        assert_eq!(
            normalize_str("C:/Users/winxp/game.jar"),
            normalize_str(r"C:\Users\winxp\game.jar")
        );
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(
            normalize_str(r"C:\Users\WinXP\Game.JAR"),
            normalize_str(r"c:\users\winxp\game.jar")
        );
    }

    /// V2 measured that Java's `toRealPath()` returns the path as constructed,
    /// without the extended-length prefix. `std::fs::canonicalize` adds it, so
    /// we must strip it or the two views disagree.
    #[test]
    fn extended_length_prefix_is_stripped() {
        assert_eq!(
            normalize_str(r"\\?\C:\Users\winxp\game.jar"),
            normalize_str(r"C:\Users\winxp\game.jar")
        );
    }

    #[test]
    fn unc_extended_prefix_becomes_a_plain_unc_path() {
        assert_eq!(
            normalize_str(r"\\?\UNC\server\share\game.jar"),
            normalize_str(r"\\server\share\game.jar")
        );
    }

    #[test]
    fn dot_components_are_removed() {
        assert_eq!(
            normalize_str(r"C:\Users\.\winxp\game.jar"),
            normalize_str(r"C:\Users\winxp\game.jar")
        );
    }

    #[test]
    fn parent_components_are_resolved_textually() {
        assert_eq!(
            normalize_str(r"C:\Users\winxp\..\winxp\game.jar"),
            normalize_str(r"C:\Users\winxp\game.jar")
        );
    }

    #[test]
    fn popping_past_the_root_is_a_noop() {
        assert_eq!(
            normalize_str(r"C:\..\..\game.jar"),
            normalize_str(r"C:\game.jar")
        );
    }

    #[test]
    fn trailing_separator_is_dropped() {
        assert_eq!(
            normalize_str(r"C:\Users\winxp\"),
            normalize_str(r"C:\Users\winxp")
        );
    }

    #[test]
    fn relative_paths_are_rejected() {
        let err = VirtualPath::new(r"relative\game.jar").expect_err("must reject");
        assert!(matches!(err, PathError::Relative { .. }));
    }

    #[test]
    fn matching_uses_normalization() {
        let vp = VirtualPath::new(r"C:\Users\winxp\game.jar").expect("absolute");
        assert!(vp.matches(r"c:/users/winxp/game.jar"));
        assert!(vp.matches(r"\\?\C:\Users\winxp\game.jar"));
        assert!(!vp.matches(r"C:\Users\winxp\other.jar"));
    }

    #[test]
    fn component_depth_ignores_non_normal_components() {
        assert_eq!(component_depth(Path::new(r"C:\a\b\c")), 3);
        assert_eq!(component_depth(Path::new(r"C:\a\.\b\")), 2);
    }
}
