//! Offline remapping helpers whose output never touches disk as a jar.
//!
//! The Java helpers in this module use TinyRemapper's in-memory output callback
//! and write a complete jar image to stdout. Rust reads that pipe into memory,
//! so a transformed payload has no filesystem artifact in the virtual file system.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod helper {
    pub const GAME: &str = include_str!("RemapToStdout.java");
    pub const MIXIN_MOD: &str = include_str!("RemapModToStdout.java");
}

/// The inputs needed to remap one jar through TinyRemapper.
#[derive(Debug, Clone)]
pub struct TinyRemapRequest<'a> {
    /// A `java` executable with Java 11+ single-file source launch support.
    pub java: &'a Path,
    /// The TinyRemapper fat jar supplied to the helper's classpath.
    pub remapper_jar: &'a Path,
    /// A Tiny v2 mapping file.
    pub mappings: &'a Path,
    /// The untrusted source jar. It may already be on disk.
    pub source_jar: &'a Path,
    /// Libraries TinyRemapper uses to resolve supertypes and propagation.
    pub classpath: &'a [PathBuf],
    /// A scratch directory for the helper source itself. No game bytes are
    /// written there.
    pub scratch_dir: &'a Path,
}

impl TinyRemapRequest<'_> {
    fn run(
        &self,
        helper_name: &str,
        source: &str,
        from: &str,
        to: &str,
    ) -> Result<Vec<u8>, TinyRemapError> {
        std::fs::create_dir_all(self.scratch_dir).map_err(|source| TinyRemapError::Stage {
            path: self.scratch_dir.to_path_buf(),
            source,
        })?;
        let helper = self.scratch_dir.join(helper_name);
        std::fs::write(&helper, source).map_err(|source| TinyRemapError::Stage {
            path: helper.clone(),
            source,
        })?;

        let mut command = Command::new(self.java);
        command
            .arg("-cp")
            .arg(self.remapper_jar)
            .arg(&helper)
            .arg(self.source_jar)
            .arg(self.mappings)
            .arg(from)
            .arg(to)
            .args(self.classpath.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = command.output().map_err(|source| TinyRemapError::Spawn {
            program: self.java.to_path_buf(),
            source,
        })?;
        let _ = std::fs::remove_file(&helper);

        if !output.status.success() {
            return Err(TinyRemapError::Remap {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(output.stdout)
    }
}

/// Remap Minecraft from `official` to `intermediary`.
///
/// # Errors
///
/// Returns [`TinyRemapError`] if the helper cannot be staged or TinyRemapper
/// exits unsuccessfully. A failed child's partial stdout is discarded.
pub fn official_to_intermediary(request: &TinyRemapRequest<'_>) -> Result<Vec<u8>, TinyRemapError> {
    request.run(
        "RemapToStdout.java",
        helper::GAME,
        "official",
        "intermediary",
    )
}

/// Remap and bake an intermediary Mixin mod to `named`.
///
/// The mod's refmap is already in intermediary form; TinyRemapper's Mixin
/// extension rewrites both normal class references and Mixin annotation
/// targets. Fabric normally performs this at runtime when
/// `fabric.development=true`; doing it here keeps the production launch away
/// from disk-writing runtime remapping.
///
/// # Errors
///
/// Returns [`TinyRemapError`] if the helper cannot be staged or TinyRemapper
/// exits unsuccessfully. A failed child's partial stdout is discarded.
pub fn intermediary_mixin_mod_to_named(
    request: &TinyRemapRequest<'_>,
) -> Result<Vec<u8>, TinyRemapError> {
    request.run(
        "RemapModToStdout.java",
        helper::MIXIN_MOD,
        "intermediary",
        "named",
    )
}

/// Errors from the offline remapping pipeline.
#[derive(Debug, thiserror::Error)]
pub enum TinyRemapError {
    /// The helper source could not be staged.
    #[error("cannot stage remap helper at {path}: {source}")]
    Stage {
        /// The path that could not be written.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },

    /// The Java helper could not be started.
    #[error("cannot start {program}: {source}")]
    Spawn {
        /// The Java executable that failed.
        program: PathBuf,
        /// The underlying process error.
        #[source]
        source: std::io::Error,
    },

    /// TinyRemapper failed.
    #[error("TinyRemapper exited with {status}: {stderr}")]
    Remap {
        /// The helper's exit status.
        status: std::process::ExitStatus,
        /// Diagnostics captured from the helper's stderr.
        stderr: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_are_embedded_and_use_in_memory_output_callbacks() {
        for source in [helper::GAME, helper::MIXIN_MOD] {
            assert!(source.contains("remapper.apply(("));
            assert!(!source.contains("OutputConsumerPath"));
        }
    }
}
