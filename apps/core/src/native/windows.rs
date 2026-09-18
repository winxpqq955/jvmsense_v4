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
// Synthetic open state
// ---------------------------------------------------------------------------

/// The first handle handed out by the virtual open/read layer.
///
/// The value is negative and near `i64::MIN`, far outside the range Windows uses
/// for `HANDLE` values. It is also distinct from the large positive synthetic
/// HMODULE values used by memory-loaded native libraries. A handle in this range
/// can therefore be identified before any OS API is called.
const VIRTUAL_HANDLE_BASE: i64 = i64::MIN + 0x1000;

/// An open virtual file or a synthetic directory/find handle.
enum VirtualHandle {
    File {
        path: String,
        bytes: Arc<[u8]>,
        cursor: i64,
        is_directory: bool,
    },
    Find {
        path: String,
    },
}

#[derive(Default)]
struct VirtualOpenTable {
    next_handle: i64,
    handles: HashMap<i64, VirtualHandle>,
}

impl VirtualOpenTable {
    fn allocate(&mut self, handle: VirtualHandle) -> Option<i64> {
        let value = self.next_handle.checked_add(1)?;
        let handle_value = self.next_handle;
        self.next_handle = value;
        self.handles.insert(handle_value, handle);
        Some(handle_value)
    }
}

static VIRTUAL_OPENS: std::sync::OnceLock<parking_lot::Mutex<VirtualOpenTable>> =
    std::sync::OnceLock::new();

fn virtual_opens() -> &'static parking_lot::Mutex<VirtualOpenTable> {
    VIRTUAL_OPENS.get_or_init(|| {
        parking_lot::Mutex::new(VirtualOpenTable {
            next_handle: VIRTUAL_HANDLE_BASE,
            handles: HashMap::new(),
        })
    })
}

fn is_virtual_handle(handle: i64) -> bool {
    handle >= VIRTUAL_HANDLE_BASE
}

fn open_virtual_file(path: &str, bytes: Arc<[u8]>, is_directory: bool) -> Option<i64> {
    virtual_opens().lock().allocate(VirtualHandle::File {
        path: path.to_string(),
        bytes,
        cursor: 0,
        is_directory,
    })
}

fn open_virtual_find(path: &str) -> Option<i64> {
    virtual_opens().lock().allocate(VirtualHandle::Find {
        path: path.to_string(),
    })
}

fn close_virtual_handle(handle: i64) -> Option<VirtualHandle> {
    if !is_virtual_handle(handle) {
        return None;
    }
    virtual_opens().lock().handles.remove(&handle)
}

/// The bytes and current cursor for a virtual file handle.
fn virtual_file_state(handle: i64) -> Option<(String, Arc<[u8]>, i64, bool)> {
    if !is_virtual_handle(handle) {
        return None;
    }
    let table = virtual_opens().lock();
    let VirtualHandle::File {
        path,
        bytes,
        cursor,
        is_directory,
    } = table.handles.get(&handle)?
    else {
        return None;
    };
    Some((path.clone(), Arc::clone(bytes), *cursor, *is_directory))
}

fn seek_virtual_file(handle: i64, position: i64) -> Option<String> {
    if !is_virtual_handle(handle) {
        return None;
    }
    let mut table = virtual_opens().lock();
    let VirtualHandle::File { path, cursor, .. } = table.handles.get_mut(&handle)? else {
        return None;
    };
    *cursor = position;
    Some(path.clone())
}

fn advance_virtual_file(handle: i64, count: i64) {
    let mut table = virtual_opens().lock();
    if let Some(VirtualHandle::File { cursor, .. }) = table.handles.get_mut(&handle) {
        *cursor = cursor.saturating_add(count);
    }
}

/// Move a virtual file cursor by `count`, matching `FileInputStream.skip0`.
///
/// Forward skips may move past EOF. Backward skips stop at byte zero, because a
/// Windows file handle cannot seek before the beginning of a file.
fn skip_virtual_file(handle: i64, count: i64) -> Option<(String, i64)> {
    if !is_virtual_handle(handle) {
        return None;
    }
    let mut table = virtual_opens().lock();
    let VirtualHandle::File { path, cursor, .. } = table.handles.get_mut(&handle)? else {
        return None;
    };
    let previous = *cursor;
    *cursor = previous.saturating_add(count).max(0);
    Some((path.clone(), cursor.saturating_sub(previous)))
}

/// Decode a NUL-terminated UTF-16 string from a native address.
///
/// `WindowsNativeDispatcher` receives paths as native wide strings rather than
/// Java `String` values.
///
/// # Safety
///
/// `address` must point to a NUL-terminated UTF-16 string.
unsafe fn wide_c_string_to_string(address: i64) -> Option<String> {
    if address == 0 {
        return None;
    }
    let pointer = address as *const u16;
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

/// Read `receiver.fd` and return the `FileDescriptor` object's native handle.
unsafe fn file_descriptor_handle_of(env: *mut JNIEnv, receiver: jobject) -> i64 {
    let t = table(env);
    let Some(get_field) = t.GetFieldID else {
        return -1;
    };
    let cls = (t.GetObjectClass.unwrap())(env, receiver);
    if cls.is_null() {
        return -1;
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
        return -1;
    }
    let fd_obj = (t.GetObjectField.unwrap())(env, receiver, id);
    if fd_obj.is_null() {
        return -1;
    }
    let handle = file_descriptor_handle(env, fd_obj);
    (t.DeleteLocalRef.unwrap())(env, fd_obj);
    handle
}

/// Set the handle on a stream receiver's nested `FileDescriptor`.
///
/// `RandomAccessFile.open0` is responsible for publishing this field. When the
/// original native implementation is skipped, the synthetic handle must be
/// written here or the JDK treats the stream as closed.
unsafe fn set_receiver_file_descriptor_handle(
    env: *mut JNIEnv,
    receiver: jobject,
    handle: i64,
) -> bool {
    let t = table(env);
    let Some(get_field) = t.GetFieldID else {
        return false;
    };
    let cls = (t.GetObjectClass.unwrap())(env, receiver);
    if cls.is_null() {
        return false;
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
        return false;
    }
    let fd_obj = (t.GetObjectField.unwrap())(env, receiver, id);
    if fd_obj.is_null() {
        return false;
    }
    set_long_field(env, fd_obj, "handle", handle);
    (t.DeleteLocalRef.unwrap())(env, fd_obj);
    true
}

/// Clear a pending exception left by a failed field lookup.
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

/// A normalized path as a VFS regular file (`false`) or directory (`true`).
fn virtual_node(path: &str) -> Option<bool> {
    current_vfs()?.virtual_node(path)
}

/// The normalized `java.io.File.path`, if it names a virtual artifact.
unsafe fn hollow_path_of_file(env: *mut JNIEnv, file: jobject) -> Option<String> {
    let raw = string_field(env, file, "path")?;
    let normalized = crate::vfs::pathkey::normalize_str(&raw);
    virtual_node(&normalized).map(|_| normalized)
}

/// Hook-hit counters, so a launch report can show which paths were exercised.
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Hit and miss totals since the process started.
#[must_use]
pub fn hit_miss_counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// What the detours decided, for the verification report and for tests.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    /// Number of calls answered from the VFS.
    pub hits: u64,
    /// Number of calls passed through to the OS.
    pub misses: u64,
    /// Hook entry-point name -> call count.
    pub entries: HashMap<&'static str, u64>,
    /// VFS paths answered from memory.
    pub resolved: Vec<String>,
    /// Paths observed by a hook but not served by this session.
    pub unresolved: Vec<String>,
}

#[derive(Default, Clone)]
struct TraceState {
    hits: u64,
    misses: u64,
    entries: HashMap<&'static str, u64>,
    resolved: Vec<String>,
    unresolved: Vec<String>,
}

impl From<TraceState> for Trace {
    fn from(value: TraceState) -> Self {
        Self {
            hits: value.hits,
            misses: value.misses,
            entries: value.entries,
            resolved: value.resolved,
            unresolved: value.unresolved,
        }
    }
}

static TRACE: std::sync::OnceLock<parking_lot::Mutex<TraceState>> = std::sync::OnceLock::new();

fn trace() -> &'static parking_lot::Mutex<TraceState> {
    TRACE.get_or_init(|| parking_lot::Mutex::new(TraceState::default()))
}

/// Snapshot of the detour trace.
#[must_use]
pub fn trace_snapshot() -> Trace {
    trace().lock().clone().into()
}

/// Reset the trace.
pub fn reset_trace() {
    *trace().lock() = TraceState::default();
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

/// The bytes of the artifact a virtual path refers to.
fn hollow_bytes(path: &str) -> Option<Arc<[u8]>> {
    current_vfs()?.artifact_bytes_by_path(path)
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
/// makes `GetFieldID` throw; on that path the pending exception is cleared so it
/// cannot poison a later JNI call.
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
        clear_pending(env);
        return None;
    }
    let value = (t.GetObjectField.unwrap())(env, obj, id);
    if value.is_null() {
        return None;
    }
    let result = java_string(env, value);
    (t.DeleteLocalRef.unwrap())(env, value);
    result
}

/// Write a Java `String` field using UTF-16.
///
/// # Safety
///
/// `env` must be valid and `obj` a live object with the named field.
unsafe fn set_string_field(env: *mut JNIEnv, obj: jobject, field: &str, value: &str) -> bool {
    let t = table(env);
    let Some(get_field) = t.GetFieldID else {
        return false;
    };
    let Ok(name) = std::ffi::CString::new(field) else {
        return false;
    };
    let cls = (t.GetObjectClass.unwrap())(env, obj);
    if cls.is_null() {
        return false;
    }
    let id = get_field(env, cls, name.as_ptr(), c"Ljava/lang/String;".as_ptr());
    (t.DeleteLocalRef.unwrap())(env, cls);
    if id.is_null() {
        clear_pending(env);
        return false;
    }
    let Some(string) = new_java_string(env, value) else {
        return false;
    };
    (t.SetObjectField.unwrap())(env, obj, id, string);
    (t.DeleteLocalRef.unwrap())(env, string);
    true
}

/// Construct a Java `String` through JNI's UTF-16 API.
///
/// # Safety
///
/// `env` must be a valid JNI environment.
unsafe fn new_java_string(env: *mut JNIEnv, value: &str) -> Option<jobject> {
    let units: Vec<u16> = value.encode_utf16().collect();
    let string = (table(env).NewString.unwrap())(env, units.as_ptr(), units.len() as i32);
    (!string.is_null()).then_some(string)
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
/// `FileInputStream.open0(String)`.
type FileInputStreamOpen0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, jobject);
/// `FileInputStream.available0()`.
type FileInputStreamAvailable0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject) -> i32;
/// `FileInputStream.isRegularFile0(FileDescriptor)`.
type FileInputStreamIsRegularFile0Fn =
    unsafe extern "system" fn(*mut JNIEnv, jobject, jobject) -> u8;
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
/// `WindowsNativeDispatcher.GetFileAttributes0(long)`.
type NiofsGetFileAttributes0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64) -> i32;
/// `WindowsNativeDispatcher.FindFirstFile0(long, FirstFile)`.
type NiofsFindFirstFile0Fn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64, jobject);
/// `WindowsNativeDispatcher.GetFinalPathNameByHandle(long)`.
type NiofsGetFinalPathNameByHandleFn =
    unsafe extern "system" fn(*mut JNIEnv, jobject, i64) -> jobject;
/// `WindowsNativeDispatcher.FindClose(long)`.
type NiofsFindCloseFn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64);
/// `WindowsNativeDispatcher.CloseHandle(long)`.
type NiofsCloseHandleFn = unsafe extern "system" fn(*mut JNIEnv, jobject, i64);
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
    fis_open0: FileInputStreamOpen0Fn,
    fis_read0: Read0Fn,
    fis_read_bytes: ReadBytes0Fn,
    fis_length0: Length0Fn,
    fis_position0: GetFilePointerFn,
    fis_skip0: Seek0Fn,
    fis_available0: FileInputStreamAvailable0Fn,
    fis_is_regular_file0: FileInputStreamIsRegularFile0Fn,
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
    niofs_get_file_attributes0: NiofsGetFileAttributes0Fn,
    niofs_find_first_file0: NiofsFindFirstFile0Fn,
    niofs_get_final_path_name_by_handle: NiofsGetFinalPathNameByHandleFn,
    niofs_find_close: NiofsFindCloseFn,
    niofs_close_handle: NiofsCloseHandleFn,
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
/// A virtual regular file is opened by allocating a synthetic handle and writing
/// it into the receiver's `FileDescriptor.handle`. The original native open is
/// not called: there is intentionally no disk file to open. Read-write access is
/// deliberately passed through so the stock JDK raises its normal error instead
/// of silently discarding writes.
unsafe extern "system" fn detour_open0(env: *mut JNIEnv, this: jobject, path: jobject, mode: i32) {
    const O_RDWR: i32 = 2;
    enter("raf.open0");

    let requested = java_string(env, path)
        .map(|raw| crate::vfs::pathkey::normalize_str(&raw))
        .filter(|path| virtual_node(path) == Some(false));
    let Some(virtual_path) = requested else {
        record_miss(None);
        (trampolines().open0)(env, this, path, mode);
        return;
    };
    if mode & O_RDWR != 0 {
        record_miss(Some(&virtual_path));
        (trampolines().open0)(env, this, path, mode);
        return;
    }

    let Some(bytes) = hollow_bytes(&virtual_path) else {
        record_miss(Some(&virtual_path));
        (trampolines().open0)(env, this, path, mode);
        return;
    };
    let Some(handle) = open_virtual_file(&virtual_path, bytes, false) else {
        record_miss(Some(&virtual_path));
        (trampolines().open0)(env, this, path, mode);
        return;
    };
    if set_receiver_file_descriptor_handle(env, this, handle) {
        record_hit(Some(&virtual_path));
    } else {
        close_virtual_handle(handle);
        record_miss(Some(&virtual_path));
        // Publishing the descriptor failed. Let the JDK produce its normal
        // exception rather than return from a constructor with an unusable FD.
        (trampolines().open0)(env, this, path, mode);
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
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, bytes, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().read_bytes0)(env, this, array, offset, length);
    };

    let start = cursor.max(0) as usize;
    let remaining = bytes.len().saturating_sub(start);
    let count = remaining.min(length.max(0) as usize);
    if count == 0 {
        record_hit(Some(&path));
        return -1; // EOF
    }

    let t = table(env);
    (t.SetByteArrayRegion.unwrap())(
        env,
        array,
        offset,
        count as i32,
        bytes[start..start + count].as_ptr().cast::<i8>(),
    );
    advance_virtual_file(handle, count as i64);
    record_hit(Some(&path));
    count as i32
}

unsafe extern "system" fn detour_read0(env: *mut JNIEnv, this: jobject) -> i32 {
    enter("raf.read0");
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, bytes, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().read0)(env, this);
    };

    let index = cursor.max(0) as usize;
    let Some(&byte) = bytes.get(index) else {
        record_hit(Some(&path));
        return -1;
    };
    advance_virtual_file(handle, 1);
    record_hit(Some(&path));
    i32::from(byte)
}

unsafe extern "system" fn detour_length0(env: *mut JNIEnv, this: jobject) -> i64 {
    enter("raf.length0");
    let handle = file_descriptor_handle_of(env, this);
    if let Some((path, bytes, _, _)) = virtual_file_state(handle) {
        record_hit(Some(&path));
        return bytes.len() as i64;
    }
    record_miss(None);
    (trampolines().length0)(env, this)
}

unsafe extern "system" fn detour_seek0(env: *mut JNIEnv, this: jobject, position: i64) -> i64 {
    enter("raf.seek0");
    let handle = file_descriptor_handle_of(env, this);
    let Some(path) = seek_virtual_file(handle, position) else {
        record_miss(None);
        return (trampolines().seek0)(env, this, position);
    };
    record_hit(Some(&path));
    position
}

unsafe extern "system" fn detour_get_file_pointer(env: *mut JNIEnv, this: jobject) -> i64 {
    enter("raf.getFilePointer");
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, _, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().get_file_pointer)(env, this);
    };
    record_hit(Some(&path));
    cursor
}

/// `FileInputStream.open0(String)`.
///
/// Guava and Mod Menu use `FileInputStream` to hash mod origins. A virtual
/// artifact must receive the same synthetic descriptor that archive and NIO
/// paths use, otherwise this API falls through to the absent disk path.
unsafe extern "system" fn detour_fis_open0(env: *mut JNIEnv, this: jobject, path: jobject) {
    enter("fis.open0");
    let requested = java_string(env, path)
        .map(|raw| crate::vfs::pathkey::normalize_str(&raw))
        .filter(|path| virtual_node(path) == Some(false));
    let Some(virtual_path) = requested else {
        record_miss(None);
        (trampolines().fis_open0)(env, this, path);
        return;
    };
    let Some(bytes) = hollow_bytes(&virtual_path) else {
        record_miss(Some(&virtual_path));
        (trampolines().fis_open0)(env, this, path);
        return;
    };
    let Some(handle) = open_virtual_file(&virtual_path, bytes, false) else {
        record_miss(Some(&virtual_path));
        (trampolines().fis_open0)(env, this, path);
        return;
    };
    if set_receiver_file_descriptor_handle(env, this, handle) {
        record_hit(Some(&virtual_path));
    } else {
        close_virtual_handle(handle);
        record_miss(Some(&virtual_path));
        (trampolines().fis_open0)(env, this, path);
    }
}

/// `FileInputStream.read0()`.
unsafe extern "system" fn detour_fis_read0(env: *mut JNIEnv, this: jobject) -> i32 {
    enter("fis.read0");
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, bytes, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().fis_read0)(env, this);
    };
    let index = cursor.max(0) as usize;
    let Some(&byte) = bytes.get(index) else {
        record_hit(Some(&path));
        return -1;
    };
    advance_virtual_file(handle, 1);
    record_hit(Some(&path));
    i32::from(byte)
}

/// `FileInputStream.readBytes(byte[], int, int)`.
unsafe extern "system" fn detour_fis_read_bytes(
    env: *mut JNIEnv,
    this: jobject,
    array: jobject,
    offset: i32,
    length: i32,
) -> i32 {
    enter("fis.readBytes");
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, bytes, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().fis_read_bytes)(env, this, array, offset, length);
    };
    if length == 0 {
        record_hit(Some(&path));
        return 0;
    }
    let start = cursor.max(0) as usize;
    let remaining = bytes.len().saturating_sub(start);
    let count = remaining.min(length.max(0) as usize);
    if count == 0 {
        record_hit(Some(&path));
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
    advance_virtual_file(handle, count as i64);
    record_hit(Some(&path));
    count as i32
}

/// `FileInputStream.length0()`.
unsafe extern "system" fn detour_fis_length0(env: *mut JNIEnv, this: jobject) -> i64 {
    enter("fis.length0");
    let handle = file_descriptor_handle_of(env, this);
    if let Some((path, bytes, _, _)) = virtual_file_state(handle) {
        record_hit(Some(&path));
        return bytes.len() as i64;
    }
    record_miss(None);
    (trampolines().fis_length0)(env, this)
}

/// `FileInputStream.position0()`.
unsafe extern "system" fn detour_fis_position0(env: *mut JNIEnv, this: jobject) -> i64 {
    enter("fis.position0");
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, _, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().fis_position0)(env, this);
    };
    record_hit(Some(&path));
    cursor
}

/// `FileInputStream.skip0(long)`.
unsafe extern "system" fn detour_fis_skip0(env: *mut JNIEnv, this: jobject, count: i64) -> i64 {
    enter("fis.skip0");
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, skipped)) = skip_virtual_file(handle, count) else {
        record_miss(None);
        return (trampolines().fis_skip0)(env, this, count);
    };
    record_hit(Some(&path));
    skipped
}

/// `FileInputStream.available0()`.
unsafe extern "system" fn detour_fis_available0(env: *mut JNIEnv, this: jobject) -> i32 {
    enter("fis.available0");
    let handle = file_descriptor_handle_of(env, this);
    let Some((path, bytes, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().fis_available0)(env, this);
    };
    let available = bytes
        .len()
        .saturating_sub(cursor.max(0) as usize)
        .min(i32::MAX as usize) as i32;
    record_hit(Some(&path));
    available
}

/// `FileInputStream.isRegularFile0(FileDescriptor)`.
unsafe extern "system" fn detour_fis_is_regular_file0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
) -> u8 {
    enter("fis.isRegularFile0");
    let handle = file_descriptor_handle(env, fd_obj);
    if let Some((path, _, _, is_directory)) = virtual_file_state(handle) {
        record_hit(Some(&path));
        return u8::from(!is_directory);
    }
    record_miss(None);
    (trampolines().fis_is_regular_file0)(env, this, fd_obj)
}

/// `FileDescriptor.close0` for a synthetic virtual handle.
///
/// The OS close is skipped and the descriptor is set to `-1`, exactly as the JDK
/// native method does. Removing the open-table entry also guarantees that a
/// later handle value cannot inherit this open file's cursor.
unsafe extern "system" fn detour_fd_close0(env: *mut JNIEnv, this: jobject) {
    enter("fd.close0");
    let handle = file_descriptor_handle(env, this);
    if !is_virtual_handle(handle) {
        record_miss(None);
        (trampolines().fd_close0)(env, this);
        return;
    }
    let path = close_virtual_handle(handle).map(|entry| match entry {
        VirtualHandle::File { path, .. } | VirtualHandle::Find { path, .. } => path,
    });
    set_long_field(env, this, "handle", -1);
    record_hit(path.as_deref());
}

/// `WinNTFileSystem.getLength0(File)` — `File.length()`.
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
    match hollow_bytes(&path) {
        Some(bytes) => {
            record_hit(Some(&path));
            bytes.len() as i64
        }
        None if virtual_node(&path) == Some(true) => {
            record_hit(Some(&path));
            0
        }
        None => {
            record_miss(Some(&path));
            (trampolines().fs_get_length0)(env, this, file)
        }
    }
}

/// `WinNTFileSystem.getBooleanAttributes0(File)`.
///
/// The JDK constants are stable ABI for this method. Virtual regular files and
/// ancestor directories are answered directly because there is no disk file for
/// the original implementation to stat.
unsafe extern "system" fn detour_fs_get_boolean_attributes0(
    env: *mut JNIEnv,
    this: jobject,
    file: jobject,
) -> i32 {
    const BA_EXISTS: i32 = 0x01;
    const BA_REGULAR: i32 = 0x02;
    const BA_DIRECTORY: i32 = 0x04;
    enter("fs.getBooleanAttributes0");
    let Some(path) = hollow_path_of_file(env, file) else {
        record_miss(None);
        return (trampolines().fs_get_boolean_attributes0)(env, this, file);
    };
    match virtual_node(&path) {
        Some(false) => {
            record_hit(Some(&path));
            BA_EXISTS | BA_REGULAR
        }
        Some(true) => {
            record_hit(Some(&path));
            BA_EXISTS | BA_DIRECTORY
        }
        None => {
            record_miss(Some(&path));
            (trampolines().fs_get_boolean_attributes0)(env, this, file)
        }
    }
}

/// `FileDispatcherImpl.size0(FileDescriptor)` — `Files.size`.
unsafe extern "system" fn detour_nio_size0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
) -> i64 {
    enter("nio.size0");
    let handle = file_descriptor_handle(env, fd_obj);
    let Some((path, bytes, _, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().nio_size0)(env, this, fd_obj);
    };
    record_hit(Some(&path));
    bytes.len() as i64
}

/// `FileDispatcherImpl.read0(FileDescriptor, long address, int len)`.
unsafe extern "system" fn detour_nio_read0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
    address: i64,
    len: i32,
) -> i32 {
    enter("nio.read0");
    let handle = file_descriptor_handle(env, fd_obj);
    let Some((path, bytes, cursor, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().nio_read0)(env, this, fd_obj, address, len);
    };

    let start = cursor.max(0) as usize;
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
    advance_virtual_file(handle, count as i64);
    record_hit(Some(&path));
    count as i32
}

/// `FileDispatcherImpl.pread0(FileDescriptor, long address, int len, long pos)`.
unsafe extern "system" fn detour_nio_pread0(
    env: *mut JNIEnv,
    this: jobject,
    fd_obj: jobject,
    address: i64,
    len: i32,
    position: i64,
) -> i32 {
    enter("nio.pread0");
    let handle = file_descriptor_handle(env, fd_obj);
    let Some((path, bytes, _, _)) = virtual_file_state(handle) else {
        record_miss(None);
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
    let Some(path) = seek_virtual_file(handle, position) else {
        record_miss(None);
        return (trampolines().nio_seek0)(env, this, fd_obj, position);
    };
    record_hit(Some(&path));
    position
}

/// `WindowsNativeDispatcher.CreateFile0`.
///
/// Virtual read-only `OPEN_EXISTING` opens allocate a synthetic handle before
/// the OS is called. Other access and creation dispositions fall through so the
/// JDK retains its normal create/write/error behavior.
unsafe extern "system" fn detour_niofs_create_file0(
    env: *mut JNIEnv,
    this: jobject,
    path_address: i64,
    desired_access: i32,
    share_mode: i32,
    security_attributes: i64,
    creation_disposition: i32,
    flags_and_attributes: i32,
) -> i64 {
    const GENERIC_WRITE: i32 = 0x4000_0000;
    const OPEN_EXISTING: i32 = 3;
    enter("niofs.createFile0");

    let requested = (|| -> Option<(String, bool)> {
        let raw = wide_c_string_to_string(path_address)?;
        let normalized = crate::vfs::pathkey::normalize_str(&raw);
        let is_directory = virtual_node(&normalized)?;
        Some((normalized, is_directory))
    })();

    let Some((path, is_directory)) = requested else {
        record_miss(None);
        return (trampolines().niofs_create_file0)(
            env,
            this,
            path_address,
            desired_access,
            share_mode,
            security_attributes,
            creation_disposition,
            flags_and_attributes,
        );
    };

    if desired_access & GENERIC_WRITE != 0 || creation_disposition != OPEN_EXISTING {
        record_miss(Some(&path));
        return (trampolines().niofs_create_file0)(
            env,
            this,
            path_address,
            desired_access,
            share_mode,
            security_attributes,
            creation_disposition,
            flags_and_attributes,
        );
    }

    let bytes = if is_directory {
        Arc::from(Vec::new().into_boxed_slice())
    } else {
        let Some(bytes) = hollow_bytes(&path) else {
            record_miss(Some(&path));
            return (trampolines().niofs_create_file0)(
                env,
                this,
                path_address,
                desired_access,
                share_mode,
                security_attributes,
                creation_disposition,
                flags_and_attributes,
            );
        };
        bytes
    };
    match open_virtual_file(&path, bytes, is_directory) {
        Some(handle) => {
            record_hit(Some(&path));
            handle
        }
        None => {
            record_miss(Some(&path));
            (trampolines().niofs_create_file0)(
                env,
                this,
                path_address,
                desired_access,
                share_mode,
                security_attributes,
                creation_disposition,
                flags_and_attributes,
            )
        }
    }
}

/// `WindowsNativeDispatcher.GetFileSizeEx(long handle)`.
unsafe extern "system" fn detour_niofs_get_file_size_ex(
    env: *mut JNIEnv,
    this: jobject,
    handle: i64,
) -> i64 {
    enter("niofs.getFileSizeEx");
    let Some((path, bytes, _, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().niofs_get_file_size_ex)(env, this, handle);
    };
    record_hit(Some(&path));
    bytes.len() as i64
}

/// `WindowsNativeDispatcher.GetFileAttributes0(long pathAddress)`.
unsafe extern "system" fn detour_niofs_get_file_attributes0(
    env: *mut JNIEnv,
    this: jobject,
    path_address: i64,
) -> i32 {
    const FILE_ATTRIBUTE_READONLY: i32 = 0x0000_0001;
    const FILE_ATTRIBUTE_DIRECTORY: i32 = 0x0000_0010;
    const FILE_ATTRIBUTE_NORMAL: i32 = 0x0000_0080;
    enter("niofs.getFileAttributes0");

    let requested = (|| {
        let raw = wide_c_string_to_string(path_address)?;
        let normalized = crate::vfs::pathkey::normalize_str(&raw);
        let is_directory = virtual_node(&normalized)?;
        Some((normalized, is_directory))
    })();
    let Some((path, is_directory)) = requested else {
        record_miss(None);
        return (trampolines().niofs_get_file_attributes0)(env, this, path_address);
    };
    record_hit(Some(&path));
    if is_directory {
        FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_READONLY
    } else {
        FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_READONLY
    }
}

/// Fill the layout shared by `WIN32_FILE_ATTRIBUTE_DATA` and the initial fields
/// of `BY_HANDLE_FILE_INFORMATION`.
///
/// # Safety
///
/// `address` must point to at least 36 writable bytes supplied by the JDK.
unsafe fn write_attribute_data(address: i64, is_directory: bool, len: u64) -> bool {
    const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    if address == 0 {
        return false;
    }
    let base = address as *mut u8;
    let attributes = if is_directory {
        FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_READONLY
    } else {
        FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_READONLY
    };
    // SAFETY: the JDK supplies this exact native output buffer.
    std::ptr::write_bytes(base, 0, 36);
    std::ptr::write_unaligned(base.cast::<u32>(), attributes);
    std::ptr::write_unaligned(base.add(28).cast::<u32>(), (len >> 32) as u32);
    std::ptr::write_unaligned(base.add(32).cast::<u32>(), len as u32);
    true
}

/// `WindowsNativeDispatcher.GetFileInformationByHandle0(long, long)`.
unsafe extern "system" fn detour_niofs_get_file_information_by_handle0(
    env: *mut JNIEnv,
    this: jobject,
    handle: i64,
    address: i64,
) {
    enter("niofs.getFileInfoByHandle0");
    let Some((path, bytes, _, _)) = virtual_file_state(handle) else {
        record_miss(None);
        (trampolines().niofs_get_file_information_by_handle0)(env, this, handle, address);
        return;
    };
    let len = bytes.len() as u64;
    if write_attribute_data(address, false, len) {
        // BY_HANDLE_FILE_INFORMATION continues with volume/file identity fields.
        // Zeroes preserve the common attributes and sizes while making the file
        // identity explicitly synthetic.
        if address != 0 {
            std::ptr::write_bytes((address + 36) as *mut u8, 0, 16);
        }
        record_hit(Some(&path));
    } else {
        record_miss(Some(&path));
    }
}

/// `WindowsNativeDispatcher.GetFileAttributesEx0(long pathAddress, long buffer)`.
unsafe extern "system" fn detour_niofs_get_file_attributes_ex0(
    env: *mut JNIEnv,
    this: jobject,
    path_address: i64,
    buffer: i64,
) {
    enter("niofs.getFileAttributesEx0");
    let requested = (|| {
        let raw = wide_c_string_to_string(path_address)?;
        let normalized = crate::vfs::pathkey::normalize_str(&raw);
        let is_directory = virtual_node(&normalized)?;
        let len = if is_directory {
            0
        } else {
            hollow_bytes(&normalized)?.len() as u64
        };
        Some((normalized, is_directory, len))
    })();
    let Some((path, is_directory, len)) = requested else {
        record_miss(None);
        (trampolines().niofs_get_file_attributes_ex0)(env, this, path_address, buffer);
        return;
    };
    if write_attribute_data(buffer, is_directory, len) {
        record_hit(Some(&path));
    } else {
        record_miss(Some(&path));
    }
}

/// `WindowsNativeDispatcher.FindFirstFile0(long pathAddress, FirstFile obj)`.
///
/// `Path.toRealPath()` resolves every component through this method. Virtual
/// role directories therefore need synthetic find handles and final names even
/// though they do not exist on disk.
unsafe extern "system" fn detour_niofs_find_first_file0(
    env: *mut JNIEnv,
    this: jobject,
    path_address: i64,
    first_file: jobject,
) {
    const FILE_ATTRIBUTE_READONLY: i32 = 0x0000_0001;
    const FILE_ATTRIBUTE_DIRECTORY: i32 = 0x0000_0010;
    const FILE_ATTRIBUTE_NORMAL: i32 = 0x0000_0080;
    enter("niofs.findFirstFile0");

    let requested = (|| {
        let raw = wide_c_string_to_string(path_address)?;
        if raw.ends_with('*') {
            return None;
        }
        let normalized = crate::vfs::pathkey::normalize_str(&raw);
        let is_directory = virtual_node(&normalized)?;
        Some((normalized, is_directory))
    })();
    let Some((path, is_directory)) = requested else {
        record_miss(None);
        (trampolines().niofs_find_first_file0)(env, this, path_address, first_file);
        return;
    };
    let Some(handle) = open_virtual_find(&path) else {
        record_miss(Some(&path));
        (trampolines().niofs_find_first_file0)(env, this, path_address, first_file);
        return;
    };
    let name = path.rsplit('\\').next().unwrap_or(&path);
    set_long_field(env, first_file, "handle", handle);
    set_string_field(env, first_file, "name", name);
    set_int_field(
        env,
        first_file,
        "attributes",
        if is_directory {
            FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_READONLY
        } else {
            FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_READONLY
        },
    );
    record_hit(Some(&path));
}

/// `WindowsNativeDispatcher.GetFinalPathNameByHandle(long)`.
unsafe extern "system" fn detour_niofs_get_final_path_name_by_handle(
    env: *mut JNIEnv,
    this: jobject,
    handle: i64,
) -> jobject {
    enter("niofs.getFinalPathNameByHandle");
    let Some((path, _, _, _)) = virtual_file_state(handle) else {
        record_miss(None);
        return (trampolines().niofs_get_final_path_name_by_handle)(env, this, handle);
    };
    record_hit(Some(&path));
    new_java_string(env, &path).unwrap_or(std::ptr::null_mut())
}

/// `WindowsNativeDispatcher.FindClose(long)`.
unsafe extern "system" fn detour_niofs_find_close(env: *mut JNIEnv, this: jobject, handle: i64) {
    enter("niofs.findClose");
    if !is_virtual_handle(handle) {
        record_miss(None);
        (trampolines().niofs_find_close)(env, this, handle);
        return;
    }
    let path = close_virtual_handle(handle).map(|entry| match entry {
        VirtualHandle::File { path, .. } | VirtualHandle::Find { path, .. } => path,
    });
    record_hit(path.as_deref());
}

/// `WindowsNativeDispatcher.CloseHandle(long)`.
unsafe extern "system" fn detour_niofs_close_handle(env: *mut JNIEnv, this: jobject, handle: i64) {
    enter("niofs.closeHandle");
    if !is_virtual_handle(handle) {
        record_miss(None);
        (trampolines().niofs_close_handle)(env, this, handle);
        return;
    }
    let path = close_virtual_handle(handle).map(|entry| match entry {
        VirtualHandle::File { path, .. } | VirtualHandle::Find { path, .. } => path,
    });
    record_hit(path.as_deref());
}

/// `FileDispatcherImpl.close0(FileDescriptor)`.
unsafe extern "system" fn detour_nio_close0(env: *mut JNIEnv, this: jobject, fd_obj: jobject) {
    enter("nio.close0");
    let handle = file_descriptor_handle(env, fd_obj);
    if !is_virtual_handle(handle) {
        record_miss(None);
        (trampolines().nio_close0)(env, this, fd_obj);
        return;
    }
    let path = close_virtual_handle(handle).map(|entry| match entry {
        VirtualHandle::File { path, .. } | VirtualHandle::Find { path, .. } => path,
    });
    set_long_field(env, fd_obj, "handle", -1);
    record_hit(path.as_deref());
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

/// Read a `java.lang.String` argument as Rust text using UTF-16.
unsafe fn java_string(env: *mut JNIEnv, value: jobject) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let t = table(env);
    let len = (t.GetStringLength.unwrap())(env, value);
    if len < 0 {
        return None;
    }
    let mut is_copy: jni::sys::jboolean = 0;
    let chars = (t.GetStringChars.unwrap())(env, value, &mut is_copy);
    if chars.is_null() {
        return None;
    }
    let units = std::slice::from_raw_parts(chars, len as usize);
    let text = String::from_utf16(units).ok();
    (t.ReleaseStringChars.unwrap())(env, value, chars);
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
        // install and Files.size keeps bypassing the virtual artifact.
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
            fis_open0: unreachable_stub_fis_open0,
            fis_read0: unreachable_stub_read0,
            fis_read_bytes: unreachable_stub_read_bytes0,
            fis_length0: unreachable_stub_length0,
            fis_position0: unreachable_stub_get_file_pointer,
            fis_skip0: unreachable_stub_seek0,
            fis_available0: unreachable_stub_fis_available0,
            fis_is_regular_file0: unreachable_stub_fis_is_regular_file0,
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
            niofs_get_file_attributes0: unreachable_stub_niofs_get_file_attributes0,
            niofs_find_first_file0: unreachable_stub_niofs_find_first_file0,
            niofs_get_final_path_name_by_handle:
                unreachable_stub_niofs_get_final_path_name_by_handle,
            niofs_find_close: unreachable_stub_niofs_find_close,
            niofs_close_handle: unreachable_stub_niofs_close_handle,
            nativelibs_load: unreachable_stub_nativelibs_load,
            nativelibs_unload: unreachable_stub_nativelibs_unload,
            nativelibs_find_builtin_lib: unreachable_stub_nativelibs_find_builtin_lib,
            nativelib_find_entry0: unreachable_stub_nativelib_find_entry0,
            raw_load0: unreachable_stub_raw_load0,
            raw_unload0: unreachable_stub_raw_unload0,
        };

        let plan: Vec<(&str, *mut c_void)> = vec![
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
                "Java_java_io_FileInputStream_open0",
                detour_fis_open0 as *mut c_void,
            ),
            (
                "Java_java_io_FileInputStream_read0",
                detour_fis_read0 as *mut c_void,
            ),
            (
                "Java_java_io_FileInputStream_readBytes",
                detour_fis_read_bytes as *mut c_void,
            ),
            (
                "Java_java_io_FileInputStream_length0",
                detour_fis_length0 as *mut c_void,
            ),
            (
                "Java_java_io_FileInputStream_position0",
                detour_fis_position0 as *mut c_void,
            ),
            (
                "Java_java_io_FileInputStream_skip0",
                detour_fis_skip0 as *mut c_void,
            ),
            (
                "Java_java_io_FileInputStream_available0",
                detour_fis_available0 as *mut c_void,
            ),
            (
                "Java_java_io_FileInputStream_isRegularFile0",
                detour_fis_is_regular_file0 as *mut c_void,
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
                "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileAttributes0",
                detour_niofs_get_file_attributes0 as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_FindFirstFile0",
                detour_niofs_find_first_file0 as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_FindClose",
                detour_niofs_find_close as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_CloseHandle",
                detour_niofs_close_handle as *mut c_void,
            ),
            (
                "Java_sun_nio_fs_WindowsNativeDispatcher_GetFinalPathNameByHandle",
                detour_niofs_get_final_path_name_by_handle as *mut c_void,
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
        "Java_java_io_FileInputStream_open0" => {
            trampolines.fis_open0 =
                std::mem::transmute::<*mut c_void, FileInputStreamOpen0Fn>(original);
        }
        "Java_java_io_FileInputStream_read0" => {
            trampolines.fis_read0 = std::mem::transmute::<*mut c_void, Read0Fn>(original);
        }
        "Java_java_io_FileInputStream_readBytes" => {
            trampolines.fis_read_bytes = std::mem::transmute::<*mut c_void, ReadBytes0Fn>(original);
        }
        "Java_java_io_FileInputStream_length0" => {
            trampolines.fis_length0 = std::mem::transmute::<*mut c_void, Length0Fn>(original);
        }
        "Java_java_io_FileInputStream_position0" => {
            trampolines.fis_position0 =
                std::mem::transmute::<*mut c_void, GetFilePointerFn>(original);
        }
        "Java_java_io_FileInputStream_skip0" => {
            trampolines.fis_skip0 = std::mem::transmute::<*mut c_void, Seek0Fn>(original);
        }
        "Java_java_io_FileInputStream_available0" => {
            trampolines.fis_available0 =
                std::mem::transmute::<*mut c_void, FileInputStreamAvailable0Fn>(original);
        }
        "Java_java_io_FileInputStream_isRegularFile0" => {
            trampolines.fis_is_regular_file0 =
                std::mem::transmute::<*mut c_void, FileInputStreamIsRegularFile0Fn>(original);
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
        "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileAttributes0" => {
            trampolines.niofs_get_file_attributes0 =
                std::mem::transmute::<*mut c_void, NiofsGetFileAttributes0Fn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_FindFirstFile0" => {
            trampolines.niofs_find_first_file0 =
                std::mem::transmute::<*mut c_void, NiofsFindFirstFile0Fn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_GetFinalPathNameByHandle" => {
            trampolines.niofs_get_final_path_name_by_handle =
                std::mem::transmute::<*mut c_void, NiofsGetFinalPathNameByHandleFn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_FindClose" => {
            trampolines.niofs_find_close =
                std::mem::transmute::<*mut c_void, NiofsFindCloseFn>(original);
        }
        "Java_sun_nio_fs_WindowsNativeDispatcher_CloseHandle" => {
            trampolines.niofs_close_handle =
                std::mem::transmute::<*mut c_void, NiofsCloseHandleFn>(original);
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
extern "system" fn unreachable_stub_fis_open0(_: *mut JNIEnv, _: jobject, _: jobject) {
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
extern "system" fn unreachable_stub_fis_available0(_: *mut JNIEnv, _: jobject) -> i32 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_fis_is_regular_file0(
    _: *mut JNIEnv,
    _: jobject,
    _: jobject,
) -> u8 {
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
extern "system" fn unreachable_stub_niofs_get_file_attributes0(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
) -> i32 {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_find_first_file0(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
    _: jobject,
) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_get_final_path_name_by_handle(
    _: *mut JNIEnv,
    _: jobject,
    _: i64,
) -> jobject {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_find_close(_: *mut JNIEnv, _: jobject, _: i64) {
    unreachable!("detour called before trampolines were installed")
}
extern "system" fn unreachable_stub_niofs_close_handle(_: *mut JNIEnv, _: jobject, _: i64) {
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

    /// These tests observe process-wide VFS state, so they must not overlap.
    static VFS_STATE_TESTS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn a_thread_without_a_session_reports_none() {
        let _state = VFS_STATE_TESTS.lock();
        assert!(current_vfs().is_none());
    }

    #[test]
    fn installing_a_session_is_scoped_to_the_closure() {
        let _state = VFS_STATE_TESTS.lock();
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
        let _state = VFS_STATE_TESTS.lock();
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
        let _state = VFS_STATE_TESTS.lock();
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
    fn synthetic_opens_have_independent_cursors() {
        let bytes: Arc<[u8]> = Arc::from([1, 2, 3, 4]);
        let first = open_virtual_file("first", Arc::clone(&bytes), false).expect("open");
        let second = open_virtual_file("second", Arc::clone(&bytes), false).expect("open");

        assert_eq!(
            virtual_file_state(first).map(|(_, _, cursor, _)| cursor),
            Some(0)
        );
        assert_eq!(seek_virtual_file(first, 3), Some("first".into()));
        assert_eq!(
            virtual_file_state(second).map(|(_, _, cursor, _)| cursor),
            Some(0)
        );
        assert!(
            close_virtual_handle(first).is_some(),
            "a closed handle must never be reused"
        );
        assert!(virtual_file_state(first).is_none());
        assert!(virtual_file_state(second).is_some());
        close_virtual_handle(second);
    }

    #[test]
    fn virtual_file_input_skips_can_cross_eof_and_rewind_to_zero() {
        let bytes: Arc<[u8]> = Arc::from([1, 2, 3, 4]);
        let handle = open_virtual_file("stream", bytes, false).expect("open");

        assert_eq!(
            skip_virtual_file(handle, 9),
            Some(("stream".to_string(), 9))
        );
        assert_eq!(
            virtual_file_state(handle).map(|(_, _, cursor, _)| cursor),
            Some(9)
        );
        assert_eq!(
            skip_virtual_file(handle, -20),
            Some(("stream".to_string(), -9))
        );
        assert_eq!(
            virtual_file_state(handle).map(|(_, _, cursor, _)| cursor),
            Some(0)
        );
        assert_eq!(
            skip_virtual_file(handle, -1),
            Some(("stream".to_string(), 0))
        );

        close_virtual_handle(handle);
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
