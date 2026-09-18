//! The Windows native layer: hooks that serve file reads from memory.
//!
//! This productizes what the V1 probe proved. The probe showed that a small
//! set of `RandomAccessFile` hooks is enough for `java.util.zip.ZipFile` — and
//! therefore for fabric-loader's ten `new ZipFile(path.toFile())` call sites —
//! to read a jar that has no on-disk form.
//!
//! The design constraint that shapes everything here is that the hooks are
//! process-global `extern "system"` functions, but the state they need (the
//! `VirtualFileSystem`) belongs to a session. The controlling thread installs a
//! scoped thread-local, while a launch additionally installs a scoped process
//! fallback so Java worker threads (such as Fabric's ForkJoin discovery pool)
//! can read the same session. The fallback is restored on unwind, preserving
//! isolated sequential launches.

pub mod lib;
pub mod symbols;

/// JVMTI class redefinition, used only as the already-loaded-class fallback.
#[cfg(windows)]
pub mod jvmti;

/// Win32 library-loading hooks, which work on any VM.
#[cfg(windows)]
pub mod win32_library;

/// Environment switches for attributing a failure to the hooks or to
/// everything else, documented here so they are discoverable rather than
/// folklore:
///
/// - `JVMSENSE_NO_HOOKS` — install no hooks at all. A launch that behaves the
///   same with and without them has a cause elsewhere.
/// - `JVMSENSE_ONLY=<text>[,<text>…]` — install only symbols whose name
///   contains one of the given fragments. This is how a hook-related
///   misbehaviour is bisected to a family without rebuilding between runs.
/// - `JVMSENSE_NO_FS_HOOKS`, `JVMSENSE_NO_NIO_HOOKS`, `JVMSENSE_NO_RAF_HOOKS`,
///   `JVMSENSE_NO_NATIVE_LOAD_HOOKS` — skip one family each.
///
/// They are read at install time, so they cost nothing when unset.
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    hit_miss_counts, provide_native_libraries, reset_trace, trace_snapshot, with_launch_vfs,
    with_vfs, HookSet, InstallError, Trace,
};

/// Alias used by integration tests, where the name reads more clearly.
#[cfg(windows)]
pub use windows::with_vfs as with_vfs_for_test;
