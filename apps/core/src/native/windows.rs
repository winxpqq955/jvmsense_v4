//! Windows hook implementation.
//!
//! Every detour follows the same shape, and the shape matters:
//!
//! 1. Resolve which file the call is about, from the receiver's `path` field.
//! 2. Ask the session's [`VirtualFileSystem`] whether it serves that path.
//! 3. If yes, answer from memory. If no, call the original — never guess.
//!
//! Step 3 is what keeps this safe for a real JVM: the overwhelming majority of
//! file reads in a running JVM are not ours, and they must behave exactly as
//! before.
//!
//! # Why cursors are keyed by path, not by file descriptor
//!
//! The obvious key for an open file is its `FileDescriptor`, and the first
//! version of this module used one. That is wrong. The JDK assigns the
//! descriptor *inside* `open0`, and the value is not reliably observable at
//! the point we would read it — registration recorded `-1`. The V1 probe
//! looked like it worked only because its lookup did not reject negative
//! descriptors, so the writer and the reader agreed on the same sentinel by
//! accident.
//!
//! `RandomAccessFile.path` is a plain `String` field set by the constructor
//! and present on the receiver for every detour, so it is both correct and
//! simpler. Position is tracked per path and reset on open, which matches the
//! pattern `ZipFile` uses: open once, then seek and read.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use jni::sys::{jobject, JNIEnv};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::native::lib::memload::strip_call_decoration;
use crate::native::symbols::{
    FILE_ATTRIBUTE_SYMBOLS, HOLLOW_READ_SYMBOLS, NATIVE_LOAD_SYMBOLS, NIO_READ_SYMBOLS,
};
use crate::vfs::VirtualFileSystem;

thread_local! {
    /// The session whose artifacts the hooks on this thread should serve.
    ///
    /// A thread-local rather than a global, so a second launch in the same
    /// process cannot see the first session's artifacts.
    static CURRENT_VFS: RefCell<Option<Arc<VirtualFileSystem>>> = const { RefCell::new(None) };
}

/// Run `f` with `vfs` published to this thread's hooks.
///
/// The guard restores the previous value on drop, so nesting is safe and a
/// panic cannot leave a stale session installed.
pub fn with_vfs<R>(vfs: Arc<VirtualFileSystem>, f: impl FnOnce() -> R) -> R {
    let previous = CURRENT_VFS.with(|cell| cell.borrow_mut().replace(vfs));
    let guard = VfsGuard {
        previous: Some(previous),
    };
    let result = f();
    drop(guard);
    result
}

struct VfsGuard {
    previous: Option<Option<Arc<VirtualFileSystem>>>,
}

impl Drop for VfsGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            CURRENT_VFS.with(|cell| {
                *cell.borrow_mut() = previous;
            });
        }
    }
}

/// Install a VFS for every thread while `f` runs.
///
/// Java code routinely performs I/O on worker threads (Fabric mod discovery,
/// for example, uses a ForkJoinPool). Those threads cannot inherit a Rust
/// thread-local, so a launch session needs this process-scoped fallback. The
/// thread-local installed by [`with_vfs`] still wins, preserving scoped test
/// sessions and nested launches on the controlling thread.
pub fn with_launch_vfs<R>(vfs: Arc<VirtualFileSystem>, f: impl FnOnce() -> R) -> R {
    let previous = {
        let mut global = launch_vfs().write();
        global.replace(vfs)
    };
    let guard = LaunchVfsGuard { previous };
    let result = f();
    drop(guard);
    result
}

struct LaunchVfsGuard {
    previous: Option<Arc<VirtualFileSystem>>,
}

impl Drop for LaunchVfsGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            *launch_vfs().write() = Some(previous);
        } else {
            *launch_vfs().write() = None;
        }
    }
}

fn launch_vfs() -> &'static parking_lot::RwLock<Option<Arc<VirtualFileSystem>>> {
    static LAUNCH_VFS: std::sync::OnceLock<parking_lot::RwLock<Option<Arc<VirtualFileSystem>>>> =
        std::sync::OnceLock::new();
    LAUNCH_VFS.get_or_init(|| parking_lot::RwLock::new(None))
}

/// The VFS installed on this thread, or the launch-wide fallback for Java
/// worker threads that did not inherit the controlling thread's local.
fn current_vfs() -> Option<Arc<VirtualFileSystem>> {
    CURRENT_VFS
        .with(|cell| cell.borrow().clone())
        .or_else(|| launch_vfs().read().clone())
}

// ---------------------------------------------------------------------------
// Cursor state
// ---------------------------------------------------------------------------

/// Read position for one open file.
///
/// A file position belongs to an *open file*, not to a path. `RandomAccessFile`
/// and a `FileChannel` opened separately are two independent open files with
/// two independent positions, so they cannot share a cursor: if they did,
/// `ZipFile` walking a jar would leave the position wherever its last seek
/// landed, and a later `Files.readAllBytes` would resume from there and return
/// a short read.
///
/// The two families are therefore namespaced. `RandomAccessFile` keys by path
/// (it has no handle); the nio layer keys by handle (it has no path).
static CURSORS: std::sync::OnceLock<parking_lot::Mutex<HashMap<String, i64>>> =
    std::sync::OnceLock::new();

fn cursors() -> &'static parking_lot::Mutex<HashMap<String, i64>> {
    CURSORS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Cursor key for a `RandomAccessFile`, which is identified by its path.
fn raf_cursor_key(path: &str) -> String {
    format!("raf:{path}")
}

/// Cursor key for a nio channel, which is identified by its handle.
fn nio_cursor_key(handle: i64) -> String {
    format!("nio:{handle}")
}

/// Native handle -> hollow path, for the detours that only receive a
/// `FileDescriptor`.
///
/// `WinNTFileSystem` and the nio `FileDispatcherImpl` methods are handed a
/// `FileDescriptor`, not a `File`, so they cannot resolve a path the way the
/// `RandomAccessFile` detours do. They learn the association from here.
///
/// The key is `FileDescriptor.handle`, **not** `FileDescriptor.fd`. On Windows
/// `fd` is a CRT descriptor and is `-1` for every file the JVM opens through
/// the Win32 handle path — which is all of them, including every `FileChannel`
/// that `Files.*` uses. Reading `fd` therefore yields `-1` for exactly the
/// descriptors this map needs to key, which is how the first attempt ended up
/// registering nothing.
///
/// Handles are process-unique while open, so unlike a CRT descriptor they
/// cannot be recycled into an unrelated file's number underneath us.
static HANDLE_PATHS: std::sync::OnceLock<parking_lot::Mutex<HashMap<i64, String>>> =
    std::sync::OnceLock::new();

fn handle_paths() -> &'static parking_lot::Mutex<HashMap<i64, String>> {
    HANDLE_PATHS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Record that `handle` refers to the hollow file at `path`.
///
/// Called from the `RandomAccessFile` detours, where the receiver carries both
/// a `path` field and a `FileDescriptor`.
unsafe fn remember_fd_path(env: *mut JNIEnv, receiver: jobject, path: &str) {
    if let Some(handle) = file_descriptor_handle_of(env, receiver) {
        if handle != 0 && handle != -1 {
            handle_paths().lock().insert(handle, path.to_string());
        }
    }
}

/// The hollow path a `FileDescriptor`-only detour should serve, if any.
unsafe fn hollow_path_for_descriptor(env: *mut JNIEnv, fd_obj: jobject) -> Option<String> {
    let vfs = current_vfs()?;
    let handle = file_descriptor_handle(env, fd_obj);
    if handle == 0 || handle == -1 {
        return None;
    }
    let path = handle_paths().lock().get(&handle).cloned()?;
    // Re-check against the session: the map is only meaningful while the path
    // is still mounted, and a stale entry would serve the wrong bytes.
    if vfs.contains_path(&path) {
        Some(path)
    } else {
        None
    }
}

/// Read `receiver.fd` (a `FileDescriptor`) and return its native handle.
///
/// # Safety
///
/// See [`string_field`].
unsafe fn file_descriptor_handle_of(env: *mut JNIEnv, receiver: jobject) -> Option<i64> {
    let t = table(env);
    let get_field = t.GetFieldID?;
    let cls = (t.GetObjectClass.unwrap())(env, receiver);
    if cls.is_null() {
        return None;
    }
    let id = get_field(
        env,
        cls,
        c"fd".as_ptr(),
        c"Ljava/io/FileDescriptor;".as_ptr(),
    );
    (t.DeleteLocalRef.unwrap())(env, cls);
    if id.is_null() {
        clear_pending(env);
        return None;
    }
    let fd_obj = (t.GetObjectField.unwrap())(env, receiver, id);
    if fd_obj.is_null() {
        return None;
    }
    let handle = file_descriptor_handle(env, fd_obj);
    (t.DeleteLocalRef.unwrap())(env, fd_obj);
    Some(handle)
}

/// The hollow path a bare native handle belongs to, if any.
fn hollow_path_for_handle(handle: i64) -> Option<String> {
    let vfs = current_vfs()?;
    if handle == 0 || handle == -1 {
        return None;
    }
    let path = handle_paths().lock().get(&handle).cloned()?;
    if vfs.contains_path(&path) {
        Some(path)
    } else {
        None
    }
}

/// Decode a NUL-terminated UTF-16 string from a native address.
///
/// `WindowsNativeDispatcher.CreateFile0` receives the path this way rather
/// than as a Java `String`, so there is no `jstring` to read.
unsafe fn wide_c_string_to_string(address: i64) -> Option<String> {
    if address == 0 {
        return None;
    }
    let pointer = address as *const u16;
    // Bound the scan: a malformed or non-string address must not walk into
    // unmapped memory looking for a terminator that is not there.
    const MAX_PATH_UNITS: usize = 32 * 1024;
    let mut len = 0usize;
    while len < MAX_PATH_UNITS && *pointer.add(len) != 0 {
        len += 1;
    }
    if len == 0 || len >= MAX_PATH_UNITS {
        return None;
    }
    let units = std::slice::from_raw_parts(pointer, len);
    Some(String::from_utf16_lossy(units))
}

/// Read `FileDescriptor.handle` (a `long`).
unsafe fn file_descriptor_handle(env: *mut JNIEnv, fd_obj: jobject) -> i64 {
    let t = table(env);
    let Some(get_field) = t.GetFieldID else {
        return -1;
    };
    let cls = (t.GetObjectClass.unwrap())(env, fd_obj);
    if cls.is_null() {
        return -1;
    }
    let id = get_field(env, cls, c"handle".as_ptr(), c"J".as_ptr());
    (t.DeleteLocalRef.unwrap())(env, cls);
    if id.is_null() {
        clear_pending(env);
        return -1;
    }
    (t.GetLongField.unwrap())(env, fd_obj, id)
}

/// Clear a pending exception.
///
/// A failed `GetFieldID` throws, and leaving that pending poisons every later
/// JNI call on the thread. Since a missing field is an expected outcome here
/// (the receiver may be any object), the exception must be cleared rather than
/// propagated.
unsafe fn clear_pending(env: *mut JNIEnv) {
    let t = table(env);
    if let Some(check) = t.ExceptionCheck {
        if check(env) != 0 {
            if let Some(clear) = t.ExceptionClear {
                clear(env);
            }
        }
    }
}

/// The `java.io.File.path` of a `File` receiver, if it names a hollow artifact.
///
/// `WinNTFileSystem.getLength0(File)` receives a `File`, whose `path` field is
/// the same string the `RandomAccessFile` detours key on.
unsafe fn hollow_path_of_file(env: *mut JNIEnv, file: jobject) -> Option<String> {
    let vfs = current_vfs()?;
    let raw = string_field(env, file, "path")?;
    let normalized = crate::vfs::pathkey::normalize_str(&raw);
    if vfs.contains_path(&normalized) {
        Some(normalized)
    } else {
        None
    }
}

/// Hook-hit counters, so a launch report can show which paths were exercised.
/// A miss on every symbol is the fingerprint of the hooks never firing, which
/// is otherwise hard to distinguish from "the application read nothing".
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Hit and miss totals since the process started.
#[must_use]
pub fn hit_miss_counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// What the detours decided, for the verification report and for tests.
///
/// A hook that installs but never fires is the failure mode most easily
/// mistaken for "the application read nothing", and these counters are the
/// only way to tell those apart without a debugger.
#[derive(Debug, Default, Clone)]
pub struct Trace {
    pub hits: u64,
    pub misses: u64,
    /// How many times each labelled symbol was entered.
    pub entries: HashMap<&'static str, u64>,
    /// Files resolved to a hollow artifact, and files that were not.
    pub resolved: Vec<String>,
    pub unresolved: Vec<String>,
}

static TRACE: std::sync::OnceLock<parking_lot::Mutex<Trace>> = std::sync::OnceLock::new();

fn trace() -> &'static parking_lot::Mutex<Trace> {
    TRACE.get_or_init(|| parking_lot::Mutex::new(Trace::default()))
}

/// Snapshot of the detour trace.
#[must_use]
pub fn trace_snapshot() -> Trace {
    trace().lock().clone()
}

/// Reset the trace.
///
/// Tests may run several launches in one process and need each one's counters
/// in isolation.
pub fn reset_trace() {
    *trace().lock() = Trace::default();
}

fn enter(label: &'static str) {
    *trace().lock().entries.entry(label).or_insert(0) += 1;
}

fn record_hit(path: Option<&str>) {
    HITS.fetch_add(1, Ordering::Relaxed);
    let mut t = trace().lock();
    t.hits += 1;
    if let Some(path) = path {
        t.resolved.push(path.to_string());
    }
}

fn record_miss(path: Option<&str>) {
    MISSES.fetch_add(1, Ordering::Relaxed);
    let mut t = trace().lock();
    t.misses += 1;
    if let Some(path) = path {
        t.unresolved.push(path.to_string());
    }
}

// ---------------------------------------------------------------------------
// Raw JNI field access
// ---------------------------------------------------------------------------

/// `JNIEnv` in `jni-sys` is already `*const JNINativeInterface_`, so the
/// function table is one deref away.
#[inline]
unsafe fn table(env: *mut JNIEnv) -> &'static jni::sys::JNINativeInterface_ {
    &**env
}

/// Read a `String` field off `obj`.
///
/// Returns `None` if the field is absent or the value is null. A missing field
/// makes `GetFieldID` throw; on that path the pending exception is **cleared**,
/// because leaving it set poisons every later JNI call on this thread — a far
/// worse failure than the one being diagnosed.
///
/// # Safety
///
/// `env` must be a valid JNI environment and `obj` a live local reference.
unsafe fn string_field(env: *mut JNIEnv, obj: jobject, field: &str) -> Option<String> {
    let t = table(env);
    let get_field = t.GetFieldID?;
    let name = std::ffi::CString::new(field).ok()?;
    let sig = c"Ljava/lang/String;";

    let cls = (t.GetObjectClass.unwrap())(env, obj);
    if cls.is_null() {
        return None;
    }
    let id = get_field(env, cls, name.as_ptr(), sig.as_ptr());
    (t.DeleteLocalRef.unwrap())(env, cls);
    if id.is_null() {
        // A failed `GetFieldID` leaves a pending `NoSuchFieldError`.
        if let Some(check) = t.ExceptionCheck {
            if check(env) != 0 {
                if let Some(clear) = t.ExceptionClear {
                    clear(env);
                }
            }
        }
        return None;
    }

    let value = (t.GetObjectField.unwrap())(env, obj, id);
    if value.is_null() {
        return None;
    }
    let len = (t.GetStringUTFLength.unwrap())(env, value);
    let chars = (t.GetStringUTFChars.unwrap())(env, value, std::ptr::null_mut());
    if chars.is_null() {
        (t.DeleteLocalRef.unwrap())(env, value);
        return None;
    }
    let text = std::str::from_utf8(std::slice::from_raw_parts(chars as *const u8, len as usize))
        .ok()
        .map(str::to_string);
    (t.ReleaseStringUTFChars.unwrap())(env, value, chars);
    (t.DeleteLocalRef.unwrap())(env, value);
    text
}

/// The `path` of a `RandomAccessFile` receiver, if it names a hollow artifact.
///
/// This is the single resolution step every read detour funnels through, so a
/// path that is not ours falls through to the original implementation rather
/// than being guessed at.
unsafe fn hollow_path(env: *mut JNIEnv, receiver: jobject) -> Option<String> {
    let vfs = current_vfs()?;
    let raw = string_field(env, receiver, "path")?;
    let normalized = crate::vfs::pathkey::normalize_str(&raw);
    if vfs.contains_path(&normalized) {
        Some(normalized)
    } else {
        // Record what was asked for: without it, "the hook found no session"
        // and "the session does not serve this path" are indistinguishable.
        let mut t = trace().lock();
        if t.unresolved.len() < 16 {
            t.unresolved.push(normalized);
        }
        None
    }
}

/// The bytes of the artifact a hollow path refers to.
fn hollow_bytes(path: &str) -> Option<Vec<u8>> {
    let vfs = current_vfs()?;
    let bytes = vfs.artifact_bytes_by_path(path)?;
    Some(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// Detours
// ---------------------------------------------------------------------------

type Open0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, i32);
type Read0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject) -> i32;
type ReadBytes0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, i32, i32) -> i32;
type Length0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject) -> i64;
type Seek0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64) -> i64;
type GetFilePointerFn = unsafe extern "system" fn(*mut JNIEnv, jobject) -> i64;
type FileDescriptorClose0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject);
/// `WinNTFileSystem.getLength0(File)`.
type GetLength0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject) -> i64;
/// `WinNTFileSystem.getBooleanAttributes0(File)`.
type GetBooleanAttributes0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject) -> i32;
/// `FileDispatcherImpl.size0(FileDescriptor)`.
type NioSize0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject) -> i64;
/// `FileDispatcherImpl.read0(FileDescriptor, long, int)`.
type NioRead0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, i64, i32) -> i32;
/// `FileDispatcherImpl.pread0(FileDescriptor, long, int, long)`.
type NioPread0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, i64, i32, i64) -> i32;
/// `FileDispatcherImpl.seek0(FileDescriptor, long)`.
type NioSeek0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, i64) -> i64;
/// `FileDispatcherImpl.close0(FileDescriptor)`.
type NioClose0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject);
/// `WindowsNativeDispatcher.CreateFile0(long, int, int, long, int, int)`.
type NiofsCreateFile0Fn =
    unsafe extern "system" fn(*mut JNIEnv, jobject, i64, i32, i32, i64, i32, i32) -> i64;
/// `WindowsNativeDispatcher.GetFileSizeEx(long)`.
type NiofsGetFileSizeExFn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64) -> i64;
/// `WindowsNativeDispatcher.GetFileInformationByHandle0(long, long)`.
type NiofsGetFileInformationByHandle0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64, i64);
/// `WindowsNativeDispatcher.GetFileAttributesEx0(long, long)`.
type NiofsGetFileAttributesEx0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64, i64);
/// `NativeLibraries.load(NativeLibraryImpl, String, boolean, boolean)`.
type NativeLibrariesLoadFn =
    unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, jobject, u8, u8) -> u8;
/// `NativeLibraries.unload(String, boolean, long)`.
type NativeLibrariesUnloadFn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, u8, i64);
/// `NativeLibraries.findBuiltinLib(String)`.
type NativeLibrariesFindBuiltinFn =
    unsafe extern "system" fn(*mut JNIEnv, jobject, jobject) -> jobject;
/// `NativeLibrary.findEntry0(long, String)`.
type NativeLibraryFindEntry0Fn =
    unsafe extern "system" fn(*mut JNIEnv, jobject, i64, jobject) -> i64;
/// `RawNativeLibraries.load0(RawNativeLibraryImpl, String)`.
type RawNativeLibrariesLoad0Fn =
    unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, jobject) -> u8;
/// `RawNativeLibraries.unload0(String, long)`.
type RawNativeLibrariesUnload0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject, i64);

/// Trampolines to the original implementations, filled in at install time.
struct Trampolines {
    open0: Open0Fn,
    read0: Read0Fn,
    read_bytes0: ReadBytes0Fn,
    length0: Length0Fn,
    seek0: Seek0Fn,
    get_file_pointer: GetFilePointerFn,
    fd_close0: FileDescriptorClose0Fn,
    fs_get_length0: GetLength0Fn,
    fs_get_boolean_attributes0: GetBooleanAttributes0Fn,
    nio_size0: NioSize0Fn,
    nio_read0: NioRead0Fn,
    nio_pread0: NioPread0Fn,
    nio_seek0: NioSeek0Fn,
    nio_close0: NioClose0Fn,
    niofs_create_file0: NiofsCreateFile0Fn,
    niofs_get_file_size_ex: NiofsGetFileSizeExFn,
    niofs_get_file_information_by_handle0: NiofsGetFileInformationByHandle0Fn,
    niofs_get_file_attributes_ex0: NiofsGetFileAttributesEx0Fn,
    nativelibs_load: NativeLibrariesLoadFn,
    nativelibs_unload: NativeLibrariesUnloadFn,
    nativelibs_find_builtin_lib: NativeLibrariesFindBuiltinFn,
    nativelib_find_entry0: NativeLibraryFindEntry0Fn,
    raw_load0: RawNativeLibrariesLoad0Fn,
    raw_unload0: RawNativeLibrariesUnload0Fn,
}

// The detours are process-global `extern "system"` functions and cannot take a
// context pointer, so the trampolines live in a `OnceLock`, written once during
// install and read afterwards. A `OnceLock` rather than a `static mut` because
// the reads happen on JVM threads while the write happens on ours, and shared
// references to a `static mut` are undefined behaviour even when the writes
// have finished.
static TRAMPOLINES: std::sync::OnceLock<Trampolines> = std::sync::OnceLock::new();

fn trampolines() -> &'static Trampolines {
    TRAMPOLINES
        .get()
        .expect("hooks enabled before trampolines were installed")
}

/// `RandomAccessFile.open0(String path, int mode)`.
///
/// The `mode` argument is the JDK's packed `O_RDONLY`/`O_RDWR`/`O_SYNC` flags.
/// Dropping it — declaring the detour with only `(this, path)` — makes the
/// native open the file in mode 0 and return a descriptor the JDK then treats
/// as closed, which surfaces as `IOException: Stream Closed` on the first
/// write. That is what breaks anything opening a file read-write, log4j's
/// rolling appender being the first casualty in a Minecraft launch.
unsafe extern "system" fn detour_open0(env: *mut JNIEnv, this: jobject, path: jobject, mode: i32) {
    enter("raf.open0");

    // The original must run first: the JDK opens the real (zero-length)
    // placeholder, which is what makes the file appear to exist. Skipping it
    // makes `ZipFile` fail with "Stream Closed".
    //
    // It may also legitimately throw, and the JDK's own caller handles that.
    // Probing the receiver before the call has settled would leave a pending
    // exception behind, which poisons every later JNI call on this thread.
    (trampolines().open0)(env, this, path, mode);

    match hollow_path(env, this) {
        Some(path) => {
            // A fresh open rewinds the cursor. `ZipFile` seeks explicitly
            // afterwards, so this only matters for the first read.
            cursors().lock().insert(raf_cursor_key(&path), 0);
            // This is the one place both the path and the descriptor are
            // visible at once, so it is where the association is established
            // for the detours that only receive a descriptor.
            remember_fd_path(env, this, &path);
            record_hit(Some(&path));
        }
        None => record_miss(None),
    }
}

unsafe extern "system" fn detour_read_bytes0(
    env: *mut JNIEnv,
    this: jobject,
    array: jobject,
    offset: i32,
    length: i32,
) -> i32 {
    enter("raf.readBytes0");
    let Some(path) = hollow_path(env, this) else {
        record_miss(None);
        return (trampolines().read_bytes0)(env, this, array, offset, length);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().read_bytes0)(env, this, array, offset, length);
    };

    let mut cursors = cursors().lock();
    let position = cursors.entry(raf_cursor_key(&path)).or_insert(0);
    let start = (*position).max(0) as usize;
    let remaining = bytes.len().saturating_sub(start);
    let count = remaining.min(length.max(0) as usize);
    if count == 0 {
        record_hit(Some(&path));
        // EOF is -1, not 0: `ZipFile` relies on it to stop reading.
        return -1;
    }

    let t = table(env);
    (t.SetByteArrayRegion.unwrap())(
        env,
        array,
        offset,
        count as i32,
        bytes[start..start + count].as_ptr().cast::<i8>(),
    );
    *position += count as i64;
    record_hit(Some(&path));
    count as i32
}

unsafe extern "system" fn detour_read0(env: *mut JNIEnv, this: jobject) -> i32 {
    enter("raf.read0");
    let Some(path) = hollow_path(env, this) else {
        record_miss(None);
        return (trampolines().read0)(env, this);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().read0)(env, this);
    };

    let mut cursors = cursors().lock();
    let position = cursors.entry(raf_cursor_key(&path)).or_insert(0);
    let index = (*position).max(0) as usize;
    if index >= bytes.len() {
        record_hit(Some(&path));
        return -1;
    }
    let byte = bytes[index];
    *position += 1;
    record_hit(Some(&path));
    i32::from(byte)
}

unsafe extern "system" fn detour_length0(env: *mut JNIEnv, this: jobject) -> i64 {
    enter("raf.length0");
    let Some(path) = hollow_path(env, this) else {
        record_miss(None);
        return (trampolines().length0)(env, this);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().length0)(env, this);
    };

    record_hit(Some(&path));
    // The placeholder is zero bytes; reporting that would make `ZipFile` give
    // up immediately, since it seeks relative to the end of the file.
    bytes.len() as i64
}

unsafe extern "system" fn detour_seek0(env: *mut JNIEnv, this: jobject, position: i64) -> i64 {
    enter("raf.seek0");
    // DIAG
    {
        let t = table(env);
        let pending_before = t.ExceptionCheck.map(|c| c(env) != 0).unwrap_or(false);
        let p = string_field(env, this, "path");
        let pending_after = t.ExceptionCheck.map(|c| c(env) != 0).unwrap_or(false);
        if pending_before || pending_after || p.is_none() {
            eprintln!(
                "[diag] seek0 pending_before={pending_before} pending_after={pending_after} path={:?}",
                p.as_deref().map(|s| &s[..s.len().min(60)])
            );
        }
    }
    let Some(path) = hollow_path(env, this) else {
        record_miss(None);
        return (trampolines().seek0)(env, this, position);
    };

    cursors().lock().insert(raf_cursor_key(&path), position);
    record_hit(Some(&path));
    position
}

unsafe extern "system" fn detour_get_file_pointer(env: *mut JNIEnv, this: jobject) -> i64 {
    enter("raf.getFilePointer");
    let Some(path) = hollow_path(env, this) else {
        record_miss(None);
        return (trampolines().get_file_pointer)(env, this);
    };

    let position = *cursors().lock().entry(raf_cursor_key(&path)).or_insert(0);
    record_hit(Some(&path));
    position
}

/// `FileDescriptor.close0` — the receiver is the descriptor, not a stream, so
/// there is no `path` to resolve and nothing to serve. Cursor state is left in
/// place: the next open of the same file resets it, and pruning it here would
/// need a descriptor-to-path map whose reliability was just disproved.
unsafe extern "system" fn detour_fd_close0(env: *mut JNIEnv, this: jobject) {
    enter("fd.close0");
    (trampolines().fd_close0)(env, this);
}

/// `WinNTFileSystem.getLength0(File)` — `File.length()`.
///
/// Must report the virtual length. Code that sizes a buffer from
/// `File.length()` and then reads through `FileInputStream` would otherwise
/// allocate zero bytes and fail far from the cause.
unsafe extern "system" fn detour_fs_get_length0(
    env: *mut JNIEnv,
    this: jobject,
    file: jobject,
) -> i64 {
    enter("fs.getLength0");
    let Some(path) = hollow_path_of_file(env, file) else {
        record_miss(None);
        return (trampolines().fs_get_length0)(env, this, file);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().fs_get_length0)(env, this, file);
    };

    record_hit(Some(&path));
    bytes.len() as i64
}

/// `WinNTFileSystem.getBooleanAttributes0(File)`.
///
/// The JVM's `File` attribute bits: `BA_EXISTS = 0x01`, `BA_REGULAR = 0x02`,
/// `BA_DIRECTORY = 0x04`, `BA_HIDDEN = 0x08`. A hollow placeholder is a real
/// zero-byte file, so the original already reports "exists, regular" for it —
/// which is correct and needs no override. This detour only exists to *count*
/// the call so the trace shows whether `File`-based existence checks were
/// exercised, and to stay correct if a future mode uses non-materialized paths.
unsafe extern "system" fn detour_fs_get_boolean_attributes0(
    env: *mut JNIEnv,
    this: jobject,
    file: jobject,
) -> i32 {
    enter("fs.getBooleanAttributes0");
    let result = (trampolines().fs_get_boolean_attributes0)(env, this, file);
    if hollow_path_of_file(env, file).is_some() {
        record_hit(None);
    } else {
        record_miss(None);
    }
    result
}

/// `FileDispatcherImpl.size0(FileDescriptor)` — `Files.size`.
unsafe extern "system" fn detour_nio_size0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
) -> i64 {
    enter("nio.size0");
    let Some(path) = hollow_path_for_descriptor(env, fd_obj) else {
        record_miss(None);
        return (trampolines().nio_size0)(env, this, fd_obj);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().nio_size0)(env, this, fd_obj);
    };

    record_hit(Some(&path));
    bytes.len() as i64
}

/// `FileDispatcherImpl.read0(FileDescriptor, long address, int len)`.
///
/// The destination is a raw native address, not a Java array, so the bytes are
/// copied directly with `ptr::copy_nonoverlapping`.
unsafe extern "system" fn detour_nio_read0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
    address: i64,
    len: i32,
) -> i32 {
    enter("nio.read0");
    let handle = file_descriptor_handle(env, fd_obj);
    let Some(path) = hollow_path_for_descriptor(env, fd_obj) else {
        record_miss(None);
        return (trampolines().nio_read0)(env, this, fd_obj, address, len);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().nio_read0)(env, this, fd_obj, address, len);
    };

    let mut cursors = cursors().lock();
    let position = cursors.entry(nio_cursor_key(handle)).or_insert(0);
    let start = (*position).max(0) as usize;
    let remaining = bytes.len().saturating_sub(start);
    let count = remaining.min(len.max(0) as usize);
    if count == 0 {
        record_hit(Some(&path));
        return -1; // EOF
    }
    if address != 0 {
        std::ptr::copy_nonoverlapping(
            bytes[start..start + count].as_ptr(),
            address as *mut u8,
            count,
        );
    }
    *position += count as i64;
    record_hit(Some(&path));
    count as i32
}

/// `FileDispatcherImpl.pread0(FileDescriptor, long address, int len, long pos)`.
///
/// Like `read0` but at an explicit offset, and without disturbing the cursor.
unsafe extern "system" fn detour_nio_pread0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
    address: i64,
    len: i32,
    position: i64,
) -> i32 {
    enter("nio.pread0");
    let Some(path) = hollow_path_for_descriptor(env, fd_obj) else {
        record_miss(None);
        return (trampolines().nio_pread0)(env, this, fd_obj, address, len, position);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().nio_pread0)(env, this, fd_obj, address, len, position);
    };

    let start = position.max(0) as usize;
    let remaining = bytes.len().saturating_sub(start);
    let count = remaining.min(len.max(0) as usize);
    if count == 0 {
        record_hit(Some(&path));
        return -1;
    }
    if address != 0 {
        std::ptr::copy_nonoverlapping(
            bytes[start..start + count].as_ptr(),
            address as *mut u8,
            count,
        );
    }
    record_hit(Some(&path));
    count as i32
}

/// `FileDispatcherImpl.seek0(FileDescriptor, long)`.
unsafe extern "system" fn detour_nio_seek0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
    position: i64,
) -> i64 {
    enter("nio.seek0");
    let handle = file_descriptor_handle(env, fd_obj);
    let Some(path) = hollow_path_for_descriptor(env, fd_obj) else {
        record_miss(None);
        return (trampolines().nio_seek0)(env, this, fd_obj, position);
    };

    cursors().lock().insert(nio_cursor_key(handle), position);
    record_hit(Some(&path));
    position
}

/// `WindowsNativeDispatcher.CreateFile0(long pathAddress, int flags, ...)`.
///
/// This is where `Files.*` actually opens a file. The first argument is a
/// native `LPCWSTR`, so the path is decoded from raw UTF-16 and matched
/// against the session. When it matches, the handle this call returns is
/// recorded, which is what lets the `FileDispatcherImpl` reads — which only
/// see the handle — find their way back to the artifact.
///
/// The handle is recorded after the original returns, since the original is
/// what produces it.
unsafe extern "system" fn detour_niofs_create_file0(
    env: *mut JNIEnv,
    this: jobject,
    path_address: i64,
    flags: i32,
    mode: i32,
    attributes: i64,
    is_directory: i32,
    follow_links: i32,
) -> i64 {
    enter("niofs.createFile0");

    // Resolve the path before the call, so the match does not depend on the
    // resulting handle.
    let wanted = (|| -> Option<String> {
        let vfs = current_vfs()?;
        let raw = wide_c_string_to_string(path_address)?;
        let normalized = crate::vfs::pathkey::normalize_str(&raw);
        vfs.resolve_path(&normalized).map(|_| normalized)
    })();

    let handle = (trampolines().niofs_create_file0)(
        env,
        this,
        path_address,
        flags,
        mode,
        attributes,
        is_directory,
        follow_links,
    );

    match wanted {
        Some(path) if handle != 0 && handle != -1 => {
            handle_paths().lock().insert(handle, path.clone());
            record_hit(Some(&path));
        }
        Some(path) => record_miss(Some(&path)),
        None => record_miss(None),
    }

    handle
}

/// `WindowsNativeDispatcher.GetFileSizeEx(long handle)`.
///
/// The size the nio filesystem layer reports for a hollow path.
unsafe extern "system" fn detour_niofs_get_file_size_ex(
    env: *mut JNIEnv,
    this: jobject,
    handle: i64,
) -> i64 {
    enter("niofs.getFileSizeEx");
    let Some(path) = hollow_path_for_handle(handle) else {
        record_miss(None);
        return (trampolines().niofs_get_file_size_ex)(env, this, handle);
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return (trampolines().niofs_get_file_size_ex)(env, this, handle);
    };

    record_hit(Some(&path));
    bytes.len() as i64
}

/// `WindowsNativeDispatcher.GetFileInformationByHandle0(long handle, long addr)`.
///
/// This is what `Files.size` actually reaches. It does not return a value: the
/// original fills a `BY_HANDLE_FILE_INFORMATION` into the caller's native
/// buffer, so the detour runs it first and then overwrites the two size fields
/// for a hollow handle.
///
/// Offsets come from `WindowsFileAttributes.fromFileInformation`, which reads
/// `nFileSizeHigh` at 28 and `nFileSizeLow` at 32.
unsafe extern "system" fn detour_niofs_get_file_information_by_handle0(
    env: *mut JNIEnv,
    this: jobject,
    handle: i64,
    address: i64,
) {
    enter("niofs.getFileInfoByHandle0");
    (trampolines().niofs_get_file_information_by_handle0)(env, this, handle, address);

    let Some(path) = hollow_path_for_handle(handle) else {
        record_miss(None);
        return;
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return;
    };
    if address == 0 {
        record_miss(Some(&path));
        return;
    }

    let size = bytes.len() as u64;
    let base = address as *mut u8;
    // SAFETY: the JDK passed this address to receive exactly this structure,
    // and the original call above wrote it. Writing the two fields back is the
    // same access it performs.
    std::ptr::write_unaligned(base.add(28).cast::<u32>(), (size >> 32) as u32);
    std::ptr::write_unaligned(base.add(32).cast::<u32>(), size as u32);

    record_hit(Some(&path));
}

/// `WindowsNativeDispatcher.GetFileAttributesEx0(long pathAddress, long buffer)`.
///
/// This is what `Files.size` actually reaches. It is the best hook point of
/// the three nio candidates: the path arrives as a native wide string *and*
/// the buffer the caller will read the size from is passed in, so neither a
/// handle mapping nor a synthesized return value is needed.
///
/// The original runs first (so a genuinely missing file still reports
/// `ERROR_FILE_NOT_FOUND`), and then the two size fields are overwritten for a
/// hollow path.
unsafe extern "system" fn detour_niofs_get_file_attributes_ex0(
    env: *mut JNIEnv,
    this: jobject,
    path_address: i64,
    buffer: i64,
) {
    enter("niofs.getFileAttributesEx0");
    (trampolines().niofs_get_file_attributes_ex0)(env, this, path_address, buffer);

    let wanted = (|| -> Option<String> {
        let vfs = current_vfs()?;
        let raw = wide_c_string_to_string(path_address)?;
        let normalized = crate::vfs::pathkey::normalize_str(&raw);
        vfs.resolve_path(&normalized).map(|_| normalized)
    })();

    let Some(path) = wanted else {
        record_miss(None);
        return;
    };
    let Some(bytes) = hollow_bytes(&path) else {
        record_miss(Some(&path));
        return;
    };
    if buffer == 0 {
        record_miss(Some(&path));
        return;
    }

    let size = bytes.len() as u64;
    let base = buffer as *mut u8;
    // SAFETY: the JDK passed this address to receive a
    // `WIN32_FILE_ATTRIBUTE_DATA`, and the original call above wrote it.
    // Correcting the two size fields is the same access it performs.
    std::ptr::write_unaligned(base.add(28).cast::<u32>(), (size >> 32) as u32);
    std::ptr::write_unaligned(base.add(32).cast::<u32>(), size as u32);

    record_hit(Some(&path));
}

/// `FileDispatcherImpl.close0(FileDescriptor)`.
///
/// Closes for real so the placeholder handle is released, and forgets the
/// descriptor association so a later reuse of the same descriptor number by an
/// unrelated file cannot be mistaken for ours.
unsafe extern "system" fn detour_nio_close0(env: *mut JNIEnv, this: jobject, fd_obj: jobject) {
    enter("nio.close0");
    let handle = file_descriptor_handle(env, fd_obj);
    if handle != 0 && handle != -1 {
        handle_paths().lock().remove(&handle);
    }
    (trampolines().nio_close0)(env, this, fd_obj);
}

/// `NativeLibraries.findBuiltinLib(String name)`.
///
/// Returning null for a name we manage is what stops the JDK from finding a
/// system-installed copy of the library first and short-circuiting past us.
unsafe extern "system" fn detour_nativelibs_find_builtin_lib(
    env: *mut JNIEnv,
    this: jobject,
    name: jobject,
) -> jobject {
    enter("nativelibs.findBuiltinLib");
    let null = std::ptr::null_mut();

    let Some(requested) = java_string(env, name) else {
        record_miss(None);
        return (trampolines().nativelibs_find_builtin_lib)(env, this, name);
    };
    let key = crate::native::lib::normalize_library_request(&requested);
    if managed_natives().lock().available.contains_key(&key) {
        record_hit(Some(&key));
        // Null means "not a built-in": the JDK then proceeds to `load`, which
        // is where the library is actually served.
        return null;
    }
    record_miss(Some(&key));
    (trampolines().nativelibs_find_builtin_lib)(env, this, name)
}

/// `NativeLibraries.load(NativeLibraryImpl impl, String name, boolean, boolean)`.
///
/// On a managed name the library is mapped from memory, its synthetic handle is
/// written into `impl.handle`, and `JNI_OnLoad` is invoked — in that order,
/// because the library's own `JNI_OnLoad` may call back into `find`, which
/// reads `handle`.
unsafe extern "system" fn detour_nativelibs_load(
    env: *mut JNIEnv,
    this: jobject,
    impl_object: jobject,
    name: jobject,
    is_builtin: u8,
    throw_if_fail: u8,
) -> u8 {
    enter("nativelibs.load");
    let Some(requested) = java_string(env, name) else {
        record_miss(None);
        return (trampolines().nativelibs_load)(
            env,
            this,
            impl_object,
            name,
            is_builtin,
            throw_if_fail,
        );
    };
    let key = crate::native::lib::normalize_library_request(&requested);

    // Take the library out of `available` under the lock, then map it outside
    // the lock: mapping executes no JNI, but `JNI_OnLoad` does, and holding a
    // lock across a call into the JVM risks a deadlock if that call re-enters
    // any of these detours.
    let library = managed_natives().lock().available.remove(&key);
    let Some(library) = library else {
        managed_natives().lock().missing.push(key.clone());
        record_miss(Some(&key));
        return (trampolines().nativelibs_load)(
            env,
            this,
            impl_object,
            name,
            is_builtin,
            throw_if_fail,
        );
    };

    let module = match crate::native::lib::map_image(library.image()) {
        Ok(module) => module,
        Err(error) => {
            eprintln!("jvmsense: cannot map native library {key}: {error}");
            record_miss(Some(&key));
            // Put it back so a retry reports the same specific error.
            managed_natives().lock().available.insert(key, library);
            return (trampolines().nativelibs_load)(
                env,
                this,
                impl_object,
                name,
                is_builtin,
                throw_if_fail,
            );
        }
    };

    let handle = {
        let mut registry = managed_natives().lock();
        let handle = registry.next_handle;
        registry.next_handle += 1;
        registry.loaded.insert(handle, (key.clone(), module));
        handle
    };

    // The handle must be visible before `JNI_OnLoad` runs.
    set_long_field(env, impl_object, "handle", handle);
    let version = invoke_jni_on_load(env, handle).unwrap_or(0);
    if version != 0 {
        set_int_field(env, impl_object, "jniVersion", version);
    }

    record_hit(Some(&key));
    1 // JNI_TRUE
}

/// `NativeLibraries.unload(String, boolean, long)`.
///
/// Deliberately does not unmap. A JVM may still be executing code inside the
/// library when the application asks to unload it, and unmapping it would
/// crash the process rather than fail; the predecessor made the same choice.
unsafe extern "system" fn detour_nativelibs_unload(
    env: *mut JNIEnv,
    this: jobject,
    name: jobject,
    is_builtin: u8,
    handle: i64,
) {
    enter("nativelibs.unload");
    if is_managed_handle(handle) {
        record_hit(None);
        return;
    }
    record_miss(None);
    (trampolines().nativelibs_unload)(env, this, name, is_builtin, handle)
}

/// `NativeLibrary.findEntry0(long handle, String name)`.
///
/// This is where every native symbol resolution for a loaded library arrives,
/// so it is where a memory-loaded module answers for its exports.
unsafe extern "system" fn detour_nativelib_find_entry0(
    env: *mut JNIEnv,
    this: jobject,
    handle: i64,
    name: jobject,
) -> i64 {
    enter("nativelib.findEntry0");
    if !is_managed_handle(handle) {
        record_miss(None);
        return (trampolines().nativelib_find_entry0)(env, this, handle, name);
    }

    let Some(symbol) = java_string(env, name) else {
        record_miss(None);
        return 0;
    };

    let registry = managed_natives().lock();
    let Some((library_name, module)) = registry.loaded.get(&handle) else {
        record_miss(Some(&symbol));
        return 0;
    };

    // JNI entry points are requested with the platform's decoration
    // (`_JNI_OnLoad@8`) which the export table does not carry, so the
    // undecorated form is tried too.
    let address = module
        .proc_address(&symbol)
        .or_else(|_| module.proc_address(strip_call_decoration(&symbol)))
        .map(|pointer| pointer as i64)
        .unwrap_or(0);
    let library_name = library_name.clone();
    drop(registry);

    if address == 0 {
        record_miss(Some(&symbol));
    } else {
        record_hit(Some(&library_name));
    }
    address
}

/// `RawNativeLibraries.load0(RawNativeLibraryImpl impl, String name)`.
///
/// JDK 21 added this path behind `System.load`. Without it, a library loaded
/// that way would bypass the registry entirely.
unsafe extern "system" fn detour_raw_load0(
    env: *mut JNIEnv,
    this: jobject,
    impl_object: jobject,
    name: jobject,
) -> u8 {
    enter("raw.load0");
    let Some(requested) = java_string(env, name) else {
        record_miss(None);
        return (trampolines().raw_load0)(env, this, impl_object, name);
    };
    let key = crate::native::lib::normalize_library_request(&requested);

    let library = managed_natives().lock().available.remove(&key);
    let Some(library) = library else {
        managed_natives().lock().missing.push(key.clone());
        record_miss(Some(&key));
        return (trampolines().raw_load0)(env, this, impl_object, name);
    };

    let module = match crate::native::lib::map_image(library.image()) {
        Ok(module) => module,
        Err(error) => {
            eprintln!("jvmsense: cannot map native library {key}: {error}");
            managed_natives()
                .lock()
                .available
                .insert(key.clone(), library);
            record_miss(Some(&key));
            return (trampolines().raw_load0)(env, this, impl_object, name);
        }
    };

    let handle = {
        let mut registry = managed_natives().lock();
        let handle = registry.next_handle;
        registry.next_handle += 1;
        registry.loaded.insert(handle, (key.clone(), module));
        handle
    };
    set_long_field(env, impl_object, "handle", handle);
    record_hit(Some(&key));
    1
}

/// `RawNativeLibraries.unload0(String, long)`.
unsafe extern "system" fn detour_raw_unload0(
    env: *mut JNIEnv,
    this: jobject,
    name: jobject,
    handle: i64,
) {
    enter("raw.unload0");
    if is_managed_handle(handle) {
        record_hit(None);
        return;
    }
    record_miss(None);
    (trampolines().raw_unload0)(env, this, name, handle)
}

/// Invoke a memory-loaded module's `JNI_OnLoad`, if it exports one.
///
/// Returns the JNI version it reports, which the caller stores so the JDK's own
/// bookkeeping stays consistent.
unsafe fn invoke_jni_on_load(env: *mut JNIEnv, handle: i64) -> Option<i32> {
    let address = {
        let registry = managed_natives().lock();
        let (_, module) = registry.loaded.get(&handle)?;
        module.proc_address("JNI_OnLoad").ok()?
    };

    // SAFETY: the address came from the module's own export table. The
    // signature is the one JNI specifies for `JNI_OnLoad`.
    let on_load: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32 =
        std::mem::transmute(address);
    let mut vm: *mut jni::sys::JavaVM = std::ptr::null_mut();
    let status = table(env).GetJavaVM.unwrap()(env, &mut vm);
    if status != 0 || vm.is_null() {
        return None;
    }
    Some(on_load(vm.cast::<c_void>(), std::ptr::null_mut()))
}

/// Read a `java.lang.String` argument as Rust text.
unsafe fn java_string(env: *mut JNIEnv, value: jobject) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let t = table(env);
    let len = (t.GetStringUTFLength.unwrap())(env, value);
    let chars = (t.GetStringUTFChars.unwrap())(env, value, std::ptr::null_mut());
    if chars.is_null() {
        return None;
    }
    let text = std::str::from_utf8(std::slice::from_raw_parts(chars as *const u8, len as usize))
        .ok()
        .map(str::to_string);
    (t.ReleaseStringUTFChars.unwrap())(env, value, chars);
    text
}

/// Write a `long` field on `obj`.
unsafe fn set_long_field(env: *mut JNIEnv, obj: jobject, field: &str, value: i64) {
    let t = table(env);
    let Some(get_field) = t.GetFieldID else {
        return;
    };
    let Ok(name) = std::ffi::CString::new(field) else {
        return;
    };
    let cls = (t.GetObjectClass.unwrap())(env, obj);
    if cls.is_null() {
        return;
    }
    let id = get_field(env, cls, name.as_ptr(), c"J".as_ptr());
    (t.DeleteLocalRef.unwrap())(env, cls);
    if id.is_null() {
        clear_pending(env);
        return;
    }
    (t.SetLongField.unwrap())(env, obj, id, value);
}

/// Write an `int` field on `obj`.
unsafe fn set_int_field(env: *mut JNIEnv, obj: jobject, field: &str, value: i32) {
    let t = table(env);
    let Some(get_field) = t.GetFieldID else {
        return;
    };
    let Ok(name) = std::ffi::CString::new(field) else {
        return;
    };
    let cls = (t.GetObjectClass.unwrap())(env, obj);
    if cls.is_null() {
        return;
    }
    let id = get_field(env, cls, name.as_ptr(), c"I".as_ptr());
    (t.DeleteLocalRef.unwrap())(env, cls);
    if id.is_null() {
        clear_pending(env);
        return;
    }
    (t.SetIntField.unwrap())(env, obj, id, value);
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// The memory-loaded native libraries, and which handle each answers to.
///
/// A library the JVM loads normally gets an HMODULE from the OS. One of ours
/// has no such handle, so the registry hands out synthetic ones and remembers
/// them, which is what `findEntry0` and `JNI_OnLoad` are looked up against.
#[cfg(windows)]
static MANAGED_NATIVES: std::sync::OnceLock<parking_lot::Mutex<ManagedNatives>> =
    std::sync::OnceLock::new();

/// Synthetic handles start here. Chosen far above any real HMODULE the OS
/// would hand out, so a genuine handle can never be mistaken for one of ours by
/// accident.
const MANAGED_HANDLE_BASE: i64 = 0x4A56_0000_0000_0000;

#[cfg(windows)]
struct ManagedNatives {
    /// The libraries available to serve, keyed by normalized file name.
    available: std::collections::HashMap<String, crate::native::lib::ManagedLibrary>,
    /// Synthetic handle -> the module actually mapped for it.
    loaded: std::collections::HashMap<i64, (String, crate::native::lib::LoadedModule)>,
    /// Names the JVM asked for but that no payload provides. Recorded so the
    /// report can say which library was missing rather than only that a load
    /// fell through to the filesystem.
    missing: Vec<String>,
    next_handle: i64,
}

#[cfg(windows)]
fn managed_natives() -> &'static parking_lot::Mutex<ManagedNatives> {
    MANAGED_NATIVES.get_or_init(|| {
        parking_lot::Mutex::new(ManagedNatives {
            available: std::collections::HashMap::new(),
            loaded: std::collections::HashMap::new(),
            missing: Vec::new(),
            next_handle: MANAGED_HANDLE_BASE,
        })
    })
}

/// Make a set of native libraries available to the JVM's loader.
///
/// Called before the JVM starts using natives, so that the first
/// `System.loadLibrary` finds its library already waiting.
#[cfg(windows)]
pub fn provide_native_libraries(
    libraries: impl IntoIterator<Item = crate::native::lib::ManagedLibrary>,
) {
    let mut registry = managed_natives().lock();
    for library in libraries {
        let key = crate::native::lib::normalize_library_request(library.file_name());
        registry.available.insert(key, library);
    }
}

/// True when a handle belongs to the memory-loaded set.
#[cfg(windows)]
fn is_managed_handle(handle: i64) -> bool {
    handle >= MANAGED_HANDLE_BASE
}

/// Load a DLL, reporting the path on failure.
fn load_module(path: PathBuf) -> Result<*mut c_void, InstallError> {
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        Err(InstallError::SharedLibrary { path })
    } else {
        Ok(module.cast::<c_void>())
    }
}

/// A set of installed hooks.
#[derive(Debug)]
pub struct HookSet {
    installed: Vec<&'static str>,
}

impl HookSet {
    /// Hook the jar-read natives out of `java.dll`.
    ///
    /// Must run **after** the JVM exists: `java.dll` is loaded by `jvm.dll`
    /// during creation, so before that there is nothing to hook.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::JavaDll`] if `java.dll` cannot be loaded from
    /// `java_home`, or [`InstallError::Symbol`] if an expected export is
    /// missing — which means the JDK is not one this build knows about, and
    /// guessing would produce hooks that silently never fire.
    pub fn install_read_hooks(java_home: &std::path::Path) -> Result<Self, InstallError> {
        // java.dll is loaded by jvm.dll during creation, so it is present by now.
        let java_module = load_module(java_home.join("bin").join("java.dll"))?;

        // nio.dll is NOT: the JDK loads it on first NIO use, which may never
        // happen if the application never touches Files. Resolving its exports
        // therefore requires forcing the load, or the hooks silently never
        // install and Files.size keeps reporting the placeholder's zero.
        let nio_module = load_module(java_home.join("bin").join("nio.dll"))?;

        let mut resolved: HashMap<&'static str, *mut c_void> = HashMap::new();
        let mut missing = Vec::new();
        for symbol in HOLLOW_READ_SYMBOLS
            .iter()
            .chain(FILE_ATTRIBUTE_SYMBOLS.iter())
            .chain(NIO_READ_SYMBOLS.iter())
            .chain(NATIVE_LOAD_SYMBOLS.iter())
        {
            let module = if symbol.family.module() == "nio.dll" {
                nio_module
            } else {
                java_module
            };
            let Ok(name) = std::ffi::CString::new(symbol.symbol) else {
                missing.push(symbol.symbol);
                continue;
            };
            match unsafe { GetProcAddress(module, name.as_ptr().cast::<u8>()) } {
                // `GetProcAddress` yields a function pointer; go through a thin
                // pointer to store it as an opaque address.
                Some(address) => {
                    resolved.insert(symbol.symbol, address as *const () as *mut c_void);
                }
                None => missing.push(symbol.symbol),
            }
        }

        // A missing native-load symbol is recorded but not fatal, because those
        // hooks land with the memory-loaded-native work. A missing read or
        // attribute symbol IS fatal: a hook that never fires looks exactly like
        // "the application read nothing", the hardest failure to diagnose from
        // the outside.
        let required_missing: Vec<&str> = missing
            .iter()
            .copied()
            .filter(|name| {
                HOLLOW_READ_SYMBOLS
                    .iter()
                    .chain(FILE_ATTRIBUTE_SYMBOLS.iter())
                    .chain(NIO_READ_SYMBOLS.iter())
                    .any(|s| s.symbol == *name)
            })
            .collect();
        if !required_missing.is_empty() {
            return Err(InstallError::Symbol {
                missing: required_missing,
            });
        }

        let mut trampolines = Trampolines {
            open0: unreachable_stub_open0,
            read0: unreachable_stub_read0,
            read_bytes0: unreachable_stub_read_bytes0,
            length0: unreachable_stub_length0,
            seek0: unreachable_stub_seek0,
            get_file_pointer: unreachable_stub_get_file_pointer,
            fd_close0: unreachable_stub_fd_close0,
            fs_get_length0: unreachable_stub_get_length0,
            fs_get_boolean_attributes0: unreachable_stub_get_boolean_attributes0,
            nio_size0: unreachable_stub_nio_size0,
            nio_read0: unreachable_stub_nio_read0,
            nio_pread0: unreachable_stub_nio_pread0,
            nio_seek0: unreachable_stub_nio_seek0,
            nio_close0: unreachable_stub_nio_close0,
            niofs_create_file0: unreachable_stub_niofs_create_file0,
            niofs_get_file_size_ex: unreachable_stub_niofs_get_file_size_ex,
            niofs_get_file_information_by_handle0:
                unreachable_stub_niofs_get_file_information_by_handle0,
            niofs_get_file_attributes_ex0: unreachable_stub_niofs_get_file_attributes_ex0,
            nativelibs_load: unreachable_stub_nativelibs_load,
            nativelibs_unload: unreachable_stub_nativelibs_unload,
            nativelibs_find_builtin_lib: unreachable_stub_nativelibs_find_builtin_lib,
            nativelib_find_entry0: unreachable_stub_nativelib_find_entry0,
            raw_load0: unreachable_stub_raw_load0,
            raw_unload0: unreachable_stub_raw_unload0,
        };

        let plan: [(&str, *mut c_void); 24] = [
            (
                "Java_java_io_RandomAccessFile_open0",
                detour_open0 as *mut c_void,
            ),
            (
                "Java_java_io_RandomAccessFile_read0",
                detour_read0 as *mut c_void,
            ),
            (
                "Java_java_io_RandomAccessFile_readBytes0",
                detour_read_bytes0 as *mut c_void,
            ),
            (
                "Java_java_io_RandomAccessFile_length0",
                detour_length0 as *mut c_void,
            ),
            (
                "Java_java_io_RandomAccessFile_seek0",
                detour_seek0 as *mut c_void,
            ),
            (
                "Java_java_io_RandomAccessFile_getFilePointer",
                detour_get_file_pointer as *mut c_void,
            ),
            (
                "Java_java_io_FileDescriptor_close0",
                detour_fd_close0 as *mut c_void,
            ),
            (
                "Java_java_io_WinNTFileSystem_getLength0",
                detour_fs_get_length0 as *mut c_void,
            ),
            (
                "Java_java_io_WinNTFileSystem_getBooleanAttributes0",
                detour_fs_get_boolean_attributes0 as *mut c_void,
            ),
            (
                "Java_sun_nio_ch_FileDispatcherImpl_size0",
                detour_nio_size0 as *mut c_void,
            ),
            (
                "Java_sun_nio_ch_FileDispatcherImpl_read0",
                detour_nio_read0 as *mut c_void,
            ),
            (
                "Java_sun_nio_ch_FileDispatcherImpl_pread0",
                detour_nio_pread0 as *mut c_void,
            ),
            (
                "Java_sun_nio_ch_FileDispatcherImpl_seek0",
                detour_nio_seek0 as *mut c_void,
            ),
            (
                "Java_sun_nio_ch_FileDispatcherImpl_close0",
                detour_nio_close0 as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_CreateFile0",
                detour_niofs_create_file0 as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileSizeEx",
                detour_niofs_get_file_size_ex as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileInformationByHandle0",
                detour_niofs_get_file_information_by_handle0 as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileAttributesEx0",
                detour_niofs_get_file_attributes_ex0 as *mut c_void,
            ),
            (
                "Java_jdk_internal_loader_NativeLibraries_load",
                detour_nativelibs_load as *mut c_void,
            ),
            (
                "Java_jdk_internal_loader_NativeLibraries_unload",
                detour_nativelibs_unload as *mut c_void,
            ),
            (
                "Java_jdk_internal_loader_NativeLibraries_findBuiltinLib",
                detour_nativelibs_find_builtin_lib as *mut c_void,
            ),
            (
                "Java_jdk_internal_loader_NativeLibrary_findEntry0",
                detour_nativelib_find_entry0 as *mut c_void,
            ),
            (
                "Java_jdk_internal_loader_RawNativeLibraries_load0",
                detour_raw_load0 as *mut c_void,
            ),
            (
                "Java_jdk_internal_loader_RawNativeLibraries_unload0",
                detour_raw_unload0 as *mut c_void,
            ),
        ];

        // `JVMSENSE_ONLY` restricts installation to symbols whose name contains
        // the given text. It exists so a hook-related misbehaviour can be
        // attributed to one family by bisection rather than by editing the
        // table and rebuilding between runs.
        // Comma-separated, so a combination can be selected without a rebuild.
        let only = std::env::var("JVMSENSE_ONLY").ok();
        let only: Option<Vec<String>> = only.map(|text| {
            text.split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect()
        });

        let mut installed = Vec::new();
        for (symbol, detour) in plan {
            if let Some(filters) = &only {
                if !filters.iter().any(|part| symbol.contains(part.as_str())) {
                    continue;
                }
            }
            let target = *resolved.get(symbol).ok_or(InstallError::Symbol {
                missing: vec![symbol],
            })?;
            let original =
                unsafe { minhook::MinHook::create_hook(target, detour) }.map_err(|status| {
                    InstallError::MinHook {
                        symbol,
                        status: format!("{status:?}"),
                    }
                })?;
            unsafe { store_trampoline(symbol, original, &mut trampolines) };
            installed.push(symbol);
        }

        // Publish the trampolines before enabling, so no enabled detour can
        // observe an unset table.
        TRAMPOLINES
            .set(trampolines)
            .map_err(|_| InstallError::MinHook {
                symbol: "*",
                status: "trampolines already installed".into(),
            })?;
        unsafe {
            minhook::MinHook::enable_all_hooks().map_err(|status| InstallError::MinHook {
                symbol: "*",
                status: format!("{status:?}"),
            })?;
        }

        Ok(Self { installed })
    }

    /// The symbols successfully hooked.
    #[must_use]
    pub fn installed(&self) -> &[&'static str] {
        &self.installed
    }
}

/// # Safety
///
/// `trampolines` must be the table being assembled during install.
unsafe fn store_trampoline(symbol: &str, original: *mut c_void, trampolines: &mut Trampolines) {
    match symbol {
        "Java_java_io_RandomAccessFile_open0" => {
            trampolines.open0 = std::mem::transmute::<*mut c_void, Open0Fn>(original);
        }
        "Java_java_io_RandomAccessFile_read0" => {
            trampolines.read0 = std::mem::transmute::<*mut c_void, Read0Fn>(original);
        }
        "Java_java_io_RandomAccessFile_readBytes0" => {
            trampolines.read_bytes0 = std::mem::transmute::<*mut c_void, ReadBytes0Fn>(original);
        }
        "Java_java_io_RandomAccessFile_length0" => {
            trampolines.length0 = std::mem::transmute::<*mut c_void, Length0Fn>(original);
        }
        "Java_java_io_RandomAccessFile_seek0" => {
            trampolines.seek0 = std::mem::transmute::<*mut c_void, Seek0Fn>(original);
        }
        "Java_java_io_RandomAccessFile_getFilePointer" => {
            trampolines.get_file_pointer =
                std::mem::transmute::<*mut c_void, GetFilePointerFn>(original);
        }
        "Java_java_io_FileDescriptor_close0" => {
            trampolines.fd_close0 =
                std::mem::transmute::<*mut c_void, FileDescriptorClose0Fn>(original);
        }
        "Java_java_io_WinNTFileSystem_getLength0" => {
            trampolines.fs_get_length0 = std::mem::transmute::<*mut c_void, GetLength0Fn>(original);
        }
        "Java_java_io_WinNTFileSystem_getBooleanAttributes0" => {
            trampolines.fs_get_boolean_attributes0 =
                std::mem::transmute::<*mut c_void, GetBooleanAttributes0Fn>(original);
        }
        "Java_sun_nio_ch_FileDispatcherImpl_size0" => {
            trampolines.nio_size0 = std::mem::transmute::<*mut c_void, NioSize0Fn>(original);
        }
        "Java_sun_nio_ch_FileDispatcherImpl_read0" => {
            trampolines.nio_read0 = std::mem::transmute::<*mut c_void, NioRead0Fn>(original);
        }
        "Java_sun_nio_ch_FileDispatcherImpl_pread0" => {
            trampolines.nio_pread0 = std::mem::transmute::<*mut c_void, NioPread0Fn>(original);
        }
        "Java_sun_nio_ch_FileDispatcherImpl_seek0" => {
            trampolines.nio_seek0 = std::mem::transmute::<*mut c_void, NioSeek0Fn>(original);
        }
        "Java_sun_nio_ch_FileDispatcherImpl_close0" => {
            trampolines.nio_close0 = std::mem::transmute::<*mut c_void, NioClose0Fn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_CreateFile0" => {
            trampolines.niofs_create_file0 =
                std::mem::transmute::<*mut c_void, NiofsCreateFile0Fn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileSizeEx" => {
            trampolines.niofs_get_file_size_ex =
                std::mem::transmute::<*mut c_void, NiofsGetFileSizeExFn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileInformationByHandle0" => {
            trampolines.niofs_get_file_information_by_handle0 =
                std::mem::transmute::<*mut c_void, NiofsGetFileInformationByHandle0Fn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileAttributesEx0" => {
            trampolines.niofs_get_file_attributes_ex0 =
                std::mem::transmute::<*mut c_void, NiofsGetFileAttributesEx0Fn>(original);
        }
        "Java_jdk_internal_loader_NativeLibraries_load" => {
            trampolines.nativelibs_load =
                std::mem::transmute::<*mut c_void, NativeLibrariesLoadFn>(original);
        }
        "Java_jdk_internal_loader_NativeLibraries_unload" => {
            trampolines.nativelibs_unload =
                std::mem::transmute::<*mut c_void, NativeLibrariesUnloadFn>(original);
        }
        "Java_jdk_internal_loader_NativeLibraries_findBuiltinLib" => {
            trampolines.nativelibs_find_builtin_lib =
                std::mem::transmute::<*mut c_void, NativeLibrariesFindBuiltinFn>(original);
        }
        "Java_jdk_internal_loader_NativeLibrary_findEntry0" => {
            trampolines.nativelib_find_entry0 =
                std::mem::transmute::<*mut c_void, NativeLibraryFindEntry0Fn>(original);
        }
        "Java_jdk_internal_loader_RawNativeLibraries_load0" => {
            trampolines.raw_load0 =
                std::mem::transmute::<*mut c_void, RawNativeLibrariesLoad0Fn>(original);
        }
        "Java_jdk_internal_loader_RawNativeLibraries_unload0" => {
            trampolines.raw_unload0 =
                std::mem::transmute::<*mut c_void, RawNativeLibrariesUnload0Fn>(original);
        }
        _ => {}
    }
}

extern "system" fn unreachable_stub_open0(_: *mut JNIEnv, _: jobject, _: jobject, _: i32) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_read0(_: *mut JNIEnv, _: jobject) -> i32 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_read_bytes0(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
    _: i32,
    _: i32,
) -> i32 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_length0(_: *mut JNIEnv, _: jobject) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_seek0(_: *mut JNIEnv, _: jobject, _: i64) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_get_file_pointer(_: *mut JNIEnv, _: jobject) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_fd_close0(_: *mut JNIEnv, _: jobject) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_get_length0(_: *mut JNIEnv, _: jobject, _: jobject) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_get_boolean_attributes0(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
) -> i32 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nio_size0(_: *mut JNIEnv, _: jobject, _: jobject) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nio_read0(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
    _: i64,
    _: i32,
) -> i32 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nio_pread0(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
    _: i64,
    _: i32,
    _: i64,
) -> i32 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nio_seek0(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
    _: i64,
) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nio_close0(_: *mut JNIEnv, _: jobject, _: jobject) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_create_file0(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
    _: i32,
    _: i32,
    _: i64,
    _: i32,
    _: i32,
) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_get_file_size_ex(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_get_file_information_by_handle0(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
    _: i64,
) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_get_file_attributes_ex0(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
    _: i64,
) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nativelibs_load(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
    _: jobject,
    _: u8,
    _: u8,
) -> u8 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nativelibs_unload(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
    _: u8,
    _: i64,
) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nativelibs_find_builtin_lib(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
) -> jobject {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_nativelib_find_entry0(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
    _: jobject,
) -> i64 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_raw_load0(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
    _: jobject,
) -> u8 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_raw_unload0(_: *mut JNIEnv, _: jobject, _: jobject, _: i64) {
    unreachable!("detour called before trampolines were installed")
}

/// Errors from installing hooks.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("cannot load {path}")]
    SharedLibrary { path: PathBuf },

    #[error("this JDK does not export: {missing:?}")]
    Symbol { missing: Vec<&'static str> },

    #[error("MinHook could not hook {symbol}: {status}")]
    MinHook {
        symbol: &'static str,
        status: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_without_a_session_reports_none() {
        assert!(current_vfs().is_none());
    }

    #[test]
    fn installing_a_session_is_scoped_to_the_closure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vfs =
            Arc::new(VirtualFileSystem::create(dir.path().join("session")).expect("create vfs"));

        let seen = with_vfs(Arc::clone(&vfs), || current_vfs().is_some());

        assert!(seen, "the session is visible inside the closure");
        assert!(
            current_vfs().is_none(),
            "and restored afterwards, so a second launch cannot see it"
        );
    }

    #[test]
    fn launch_session_is_visible_to_worker_threads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vfs =
            Arc::new(VirtualFileSystem::create(dir.path().join("session")).expect("create vfs"));

        let seen_on_worker = with_launch_vfs(Arc::clone(&vfs), || {
            std::thread::spawn(|| current_vfs().is_some())
                .join()
                .expect("worker must not panic")
        });
        assert!(
            seen_on_worker,
            "Java worker threads must see the launch-wide VFS"
        );
        assert!(
            current_vfs().is_none(),
            "the launch-wide fallback must be restored"
        );
    }
    #[test]
    fn sessions_nest_and_unwind_correctly() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let a = Arc::new(VirtualFileSystem::create(dir_a.path().join("s")).expect("vfs a"));
        let b = Arc::new(VirtualFileSystem::create(dir_b.path().join("s")).expect("vfs b"));

        with_vfs(Arc::clone(&a), || {
            assert!(Arc::ptr_eq(&current_vfs().expect("a"), &a));
            with_vfs(Arc::clone(&b), || {
                assert!(Arc::ptr_eq(&current_vfs().expect("b"), &b));
            });
            assert!(
                Arc::ptr_eq(&current_vfs().expect("a again"), &a),
                "the inner guard must restore the outer session"
            );
        });

        assert!(current_vfs().is_none());
    }

    #[test]
    fn cursors_start_at_zero_and_advance() {
        cursors().lock().clear();

        assert_eq!(*cursors().lock().entry("x".into()).or_insert(0), 0);
        cursors().lock().insert("x".into(), 42);
        assert_eq!(*cursors().lock().get("x").expect("present"), 42);
    }

    #[test]
    fn trace_counts_are_separated_by_outcome() {
        reset_trace();
        record_hit(Some("a"));
        record_hit(Some("b"));
        record_miss(Some("c"));

        let trace = trace_snapshot();
        assert_eq!(trace.hits, 2);
        assert_eq!(trace.misses, 1);
        assert_eq!(trace.resolved, vec!["a", "b"]);
        assert_eq!(trace.unresolved, vec!["c"]);
    }

    #[test]
    fn enter_counts_per_label() {
        reset_trace();
        enter("raf.open0");
        enter("raf.open0");
        enter("raf.length0");

        let trace = trace_snapshot();
        assert_eq!(trace.entries.get("raf.open0"), Some(&2));
        assert_eq!(trace.entries.get("raf.length0"), Some(&1));
    }
}
