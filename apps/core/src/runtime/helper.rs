//! Runtime-only Java helper compilation.
//!
//! The helper is implementation code, not mod payload. It is compiled into a
//! scratch directory, read into memory, and deleted before redefinition.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE: &str = include_str!("RuntimeInjector.java");

/// Compile the ASM-based redefinition helper and return its class bytes.
///
/// # Errors
///
/// Returns [`HelperError`] if the source cannot be staged, `javac` fails, or
/// the resulting class cannot be read.
pub fn compile_runtime_injector(
    java_home: &Path,
    classpath: &[PathBuf],
    scratch: &Path,
) -> Result<Vec<u8>, HelperError> {
    let source_dir = scratch.join("src");
    let classes = scratch.join("classes");
    let source = source_dir
        .join("jvmsense")
        .join("runtime")
        .join("RuntimeInjector.java");
    std::fs::create_dir_all(source.parent().expect("source parent"))
        .map_err(|source| HelperError::Io { source })?;
    std::fs::create_dir_all(&classes).map_err(|source| HelperError::Io { source })?;
    std::fs::write(&source, SOURCE).map_err(|source| HelperError::Io { source })?;

    let separator = if cfg!(windows) { ";" } else { ":" };
    let classpath = classpath
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(separator);
    let output = Command::new(java_home.join("bin").join("javac.exe"))
        .arg("-cp")
        .arg(classpath)
        .arg("-d")
        .arg(&classes)
        .arg(&source)
        .output()
        .map_err(|source| HelperError::Spawn { source })?;
    if !output.status.success() {
        return Err(HelperError::Compile {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let bytes = std::fs::read(
        classes
            .join("jvmsense")
            .join("runtime")
            .join("RuntimeInjector.class"),
    )
    .map_err(|source| HelperError::Io { source })?;

    let mut jar = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut jar);
        writer
            .start_file(
                "jvmsense/runtime/RuntimeInjector.class",
                zip::write::FileOptions::<()>::default(),
            )
            .map_err(|source| HelperError::Archive { source })?;
        writer
            .write_all(&bytes)
            .map_err(|source| HelperError::Io { source })?;
        writer
            .finish()
            .map_err(|source| HelperError::Archive { source })?;
    }
    let _ = std::fs::remove_dir_all(scratch);
    Ok(jar.into_inner())
}

/// Errors from compiling the runtime helper.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    /// A file operation failed.
    #[error("helper I/O failed: {source}")]
    Io {
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// `javac` could not be started.
    #[error("cannot start javac: {source}")]
    Spawn {
        /// The process error.
        #[source]
        source: std::io::Error,
    },
    /// Zip construction for the helper jar failed.
    #[error("cannot build helper jar: {source}")]
    Archive {
        /// The zip error.
        #[source]
        source: zip::result::ZipError,
    },
    /// `javac` rejected the helper source.
    #[error("javac failed with {status}: {stderr}")]
    Compile {
        /// The compiler exit status.
        status: std::process::ExitStatus,
        /// Captured compiler diagnostics.
        stderr: String,
    },
}
