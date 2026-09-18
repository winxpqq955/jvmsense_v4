//! The Win32 library-loading kernel hooks.
//!
//! These exist because hooking a VM's *internal* library-loading natives is not
//! portable. HotSpot exposes `NativeLibraries.load` as a JNI export and can be
//! intercepted by name; Eclipse OpenJ9 (which is what IBM Semeru ships) does
//! not — its `ClassLoader` natives live inside `j9vm29.dll`, which exports six
//! symbols and none of them a `Java_` entry point.
//!
//! Every VM, however, ultimately asks the OS loader for the file. Measured on
//! Semeru 25, `System.loadLibrary("zip")` produces a sequence of
//! `LoadLibraryExW` calls probing several path combinations and finishing with
//! the fully resolved `<java.library.path>\zip.dll`. That final call is a hook
//! point that works regardless of which VM is underneath, so it is the one this
//! module takes.
//!
//! Handling a managed library means returning a *synthetic* `HMODULE` that the
//! OS has never issued. Every later `GetProcAddress` on that handle has to be
//! answered from the mapped module's own export table, which is why that hook
//! is not optional.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{FreeLibrary as Win32FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress as Win32GetProcAddress, LoadLibraryW,
};

/// A synthetic module handle for a memory-loaded library.
///
/// Chosen from a range no real `HMODULE` occupies, so a genuine handle can
/// never be mistaken for one of ours. Real handles on Windows are small
/// multiples of 64 KiB in the low part of the address space; this is far above
/// any plausible value.
const SYNTHETIC_HANDLE_BASE: usize = 0x4A56_0001_0000_0000;

/// What the loader should hand back for a path it manages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResolution {
    /// The path is not ours; the original Win32 call must run.
    NotManaged,
    /// The path is ours and the module mapped at this synthetic handle.
    Managed(HMODULE),
}

/// The set of memory-loaded modules, keyed by the normalized path the OS loader
/// was asked for.
///
/// Provided by the session before the JVM starts loading natives, and consulted
/// from the `LoadLibraryExW` detour.
static MANAGED: OnceLock<ManagedModules> = OnceLock::new();

/// A resolver the detours consult.
///
/// Boxed and function-shaped rather than holding a concrete type, so this
/// module does not take a dependency on the native-library registry and the two
/// can be tested apart.
type Resolver = Box<dyn Fn(&str) -> ManagedResolution + Send + Sync>;

/// Install the resolver the detours will consult.
///
/// # Panics
///
/// Panics if called twice. The registry is process-global because the hooks it
/// answers are, and a second install would silently drop the first.
pub fn install_resolver(resolver: Resolver) {
    if MANAGED.set(ManagedModules { resolver }).is_err() {
        // The hooks are process-global, so the resolver must be too; a second
        // install would silently replace the first and change behaviour mid-run.
        panic!("the library resolver is installed once");
    }
}

struct ManagedModules {
    resolver: Resolver,
}

/// Resolve a path the OS loader asked for.
#[must_use]
pub fn resolve(path: &str) -> ManagedResolution {
    match MANAGED.get() {
        Some(modules) => (modules.resolver)(path),
        None => ManagedResolution::NotManaged,
    }
}

/// True when a handle came from [`SYNTHETIC_HANDLE_BASE`].
#[must_use]
pub fn is_synthetic(handle: HMODULE) -> bool {
    (handle as usize) >= SYNTHETIC_HANDLE_BASE
}

/// Turn an index into a synthetic handle.
#[must_use]
pub fn synthetic_handle(index: usize) -> HMODULE {
    (SYNTHETIC_HANDLE_BASE + index) as HMODULE
}

/// The index encoded in a synthetic handle.
#[must_use]
pub fn synthetic_index(handle: HMODULE) -> usize {
    (handle as usize) - SYNTHETIC_HANDLE_BASE
}

// ---------------------------------------------------------------------------
// Detours
// ---------------------------------------------------------------------------

type LoadLibraryExWFn = unsafe extern "system" fn(*const u16, *mut c_void, u32) -> HMODULE;
type LoadLibraryWFn = unsafe extern "system" fn(*const u16) -> HMODULE;
type GetProcAddressFn = unsafe extern "system" fn(HMODULE, *const u8) -> *mut c_void;
type FreeLibraryFn = unsafe extern "system" fn(HMODULE) -> i32;

/// Trampolines to the original Win32 functions.
pub struct Win32Trampolines {
    pub load_library_ex_w: LoadLibraryExWFn,
    pub load_library_w: LoadLibraryWFn,
    pub get_proc_address: GetProcAddressFn,
    pub free_library: FreeLibraryFn,
}

static TRAMPOLINES: OnceLock<Win32Trampolines> = OnceLock::new();

/// Counters for the Win32 detours.
///
/// Separate from the JNI hook counters because these fire for every library the
/// process loads, most of which are not ours, and mixing them would drown the
/// signal that matters.
static MANAGED_LOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PROBED_PATHS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(paths probed, libraries served from memory)`.
#[must_use]
pub fn win32_counts() -> (u64, u64) {
    (
        PROBED_PATHS.load(std::sync::atomic::Ordering::Relaxed),
        MANAGED_LOADS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

fn trampolines() -> Option<&'static Win32Trampolines> {
    TRAMPOLINES.get()
}

/// Detour for `LoadLibraryExW`.
///
/// # Safety
///
/// Called by the OS loader with its own argument conventions.
pub unsafe extern "system" fn detour_load_library_ex_w(
    name: *const u16,
    file: *mut c_void,
    flags: u32,
) -> HMODULE {
    if let Some(path) = wide_to_string(name) {
        PROBED_PATHS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let ManagedResolution::Managed(handle) = resolve(&path) {
            MANAGED_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return handle;
        }
    }
    match trampolines() {
        Some(t) => (t.load_library_ex_w)(name, file, flags),
        None => std::ptr::null_mut(),
    }
}

/// Detour for `LoadLibraryW`.
///
/// # Safety
///
/// Called by the OS loader.
pub unsafe extern "system" fn detour_load_library_w(name: *const u16) -> HMODULE {
    if let Some(path) = wide_to_string(name) {
        PROBED_PATHS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let ManagedResolution::Managed(handle) = resolve(&path) {
            MANAGED_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return handle;
        }
    }
    match trampolines() {
        Some(t) => (t.load_library_w)(name),
        None => std::ptr::null_mut(),
    }
}

/// Detour for `GetProcAddress`.
///
/// A synthetic handle has no entry in the OS loader's tables, so
/// `GetProcAddress` would fail on it. The lookup is answered from the mapped
/// module instead.
///
/// # Safety
///
/// Called by the JVM with a handle it obtained from one of the load detours.
pub unsafe extern "system" fn detour_get_proc_address(
    module: HMODULE,
    name: *const u8,
) -> *mut c_void {
    if is_synthetic(module) {
        let index = synthetic_index(module);
        if let Some(symbol) = c_string(name) {
            if let Some(address) = crate::native::lib::resolve_synthetic_export(index, &symbol) {
                return address;
            }
        }
        // A symbol the module does not export is a genuine miss; falling
        // through to the OS would return an address from an unrelated module.
        windows_sys::Win32::Foundation::SetLastError(127); // ERROR_PROC_NOT_FOUND
        return std::ptr::null_mut();
    }

    match trampolines() {
        Some(t) => (t.get_proc_address)(module, name),
        None => std::ptr::null_mut(),
    }
}

/// Detour for `FreeLibrary`.
///
/// A managed module is never unmapped. A JVM may still be executing code inside
/// it when the application asks to unload, and unmapping would crash the
/// process rather than fail cleanly — the same choice the predecessor made.
///
/// # Safety
///
/// Called by the OS loader.
pub unsafe extern "system" fn detour_free_library(module: HMODULE) -> i32 {
    if is_synthetic(module) {
        return 1; // success, deliberately without unloading
    }
    match trampolines() {
        Some(t) => (t.free_library)(module),
        None => 0,
    }
}

/// Record the trampolines the detours call through to.
///
/// # Errors
///
/// Returns the handle that could not be resolved, which means the OS loader
/// does not expose that function — a configuration this code cannot work under.
pub fn prepare_trampolines() -> Result<(), &'static str> {
    // `LoadLibraryW` on kernel32 returns the module without needing a path,
    // which is how a handle to an already-loaded DLL is obtained.
    let kernel32_name: Vec<u16> = "kernel32.dll"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let kernel32 = unsafe { LoadLibraryW(kernel32_name.as_ptr()) };
    if kernel32.is_null() {
        return Err("kernel32.dll");
    }

    let mut trampolines = Win32Trampolines {
        load_library_ex_w: unreachable_load_ex_w,
        load_library_w: unreachable_load_w,
        get_proc_address: unreachable_get_proc_address,
        free_library: unreachable_free_library,
    };

    // The detours are cast to opaque pointers up front: `create_hook` takes
    // `*mut c_void`, and a function item does not coerce at the call site.
    let plan: [(&str, *mut c_void); 4] = [
        (
            "LoadLibraryExW",
            detour_load_library_ex_w as *const () as *mut c_void,
        ),
        (
            "LoadLibraryW",
            detour_load_library_w as *const () as *mut c_void,
        ),
        (
            "GetProcAddress",
            detour_get_proc_address as *const () as *mut c_void,
        ),
        (
            "FreeLibrary",
            detour_free_library as *const () as *mut c_void,
        ),
    ];

    for &(symbol, detour_pointer) in &plan {
        let Ok(name) = std::ffi::CString::new(symbol) else {
            return Err("symbol name");
        };
        // `GetProcAddress` yields a function pointer; `create_hook` wants it as
        // an opaque address.
        let Some(target) = (unsafe { Win32GetProcAddress(kernel32, name.as_ptr().cast::<u8>()) })
            .map(|function| function as *const () as *mut c_void)
        else {
            return Err(symbol);
        };
        // `create_hook` takes an opaque pointer; the array above already holds
        // the detour cast to one.
        let Ok(original) = (unsafe { minhook::MinHook::create_hook(target, detour_pointer) })
        else {
            return Err(symbol);
        };
        unsafe { store(&mut trampolines, symbol, original) };
    }

    TRAMPOLINES.set(trampolines).ok();
    Ok(())
}

/// Enable every hook created by [`prepare_trampolines`].
///
/// # Errors
///
/// Returns MinHook's status rendered as text.
pub fn enable() -> Result<(), String> {
    unsafe { minhook::MinHook::enable_all_hooks() }.map_err(|status| format!("{status:?}"))
}

/// Install the Win32 hooks, matching a library at an explicit placeholder path.
///
/// A VM resolves a library *name* to a path before asking the OS loader, and
/// OpenJ9 only proceeds if that path exists on `java.library.path`. So the
/// session writes a zero-length placeholder at a known location and the hook
/// matches that full path. The placeholder is the same hollow-path trick the
/// jar side uses, applied to natives: the file is real, its bytes are not.
///
/// # Errors
///
/// See [`install_loader_hooks`].
pub fn install_loader_hooks_at(
    libraries: impl IntoIterator<Item = crate::native::lib::ManagedLibrary>,
    placeholder_path: &str,
) -> Result<Vec<&'static str>, String> {
    let normalized = crate::vfs::pathkey::normalize_str(placeholder_path);
    install_loader_hooks_with(
        libraries,
        Box::new(move |requested: &str| {
            crate::vfs::pathkey::normalize_str(requested) == normalized
        }),
    )
}

/// Install the Win32 library hooks and connect them to the memory loader.
///
/// This is the entry point a session calls. It does three things that only make
/// sense together:
///
/// 1. Registers a resolver so the detours can answer "is this path ours".
/// 2. Registers each available library under a synthetic handle index.
/// 3. Hooks the four Win32 functions and enables them.
///
/// The resolver serves a path by mapping the library on first request and
/// remembering the mapping under its handle index, so repeated loads of the
/// same library return the same module.
///
/// # Errors
///
/// Returns the name of whichever Win32 function could not be hooked. Every one
/// is required: without `GetProcAddress` a synthetic handle cannot be resolved
/// and the JVM would fail at the first symbol lookup.
pub fn install_loader_hooks(
    libraries: impl IntoIterator<Item = crate::native::lib::ManagedLibrary>,
) -> Result<Vec<&'static str>, String> {
    install_loader_hooks_with(libraries, Box::new(|_requested: &str| true))
}

/// The shared implementation, parameterised by how a requested path is matched.
///
/// # Errors
///
/// Returns the name of whichever Win32 function could not be hooked.
fn install_loader_hooks_with(
    libraries: impl IntoIterator<Item = crate::native::lib::ManagedLibrary>,
    matches: Box<dyn Fn(&str) -> bool + Send + Sync>,
) -> Result<Vec<&'static str>, String> {
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// The libraries available to serve, and the modules already mapped.
    struct Registry {
        available: HashMap<String, crate::native::lib::ManagedLibrary>,
        /// Normalized path -> handle index, so a second load of the same
        /// library returns the module already mapped for it.
        by_path: HashMap<String, usize>,
        next_index: usize,
    }

    // Declared inside the function so the whole registry lives and dies with
    // the resolver it backs, and nothing else can observe it.
    let registry: &'static Mutex<Registry> = Box::leak(Box::new(Mutex::new(Registry {
        available: HashMap::new(),
        by_path: HashMap::new(),
        next_index: 0,
    })));

    {
        let mut guard = registry.lock().map_err(|_| "registry poisoned")?;
        for library in libraries {
            let key = crate::native::lib::normalize_library_request(library.file_name());
            guard.available.insert(key, library);
        }
    }

    install_resolver(Box::new(move |requested: &str| {
        if !matches(requested) {
            return ManagedResolution::NotManaged;
        }

        // The registry is keyed by the library's own name, while `requested` is
        // whatever path the loader asked for. Normalizing both to a bare stem
        // is what lets a full path match the library it names.
        let key = crate::native::lib::normalize_library_request(requested);

        let Ok(mut guard) = registry.lock() else {
            return ManagedResolution::NotManaged;
        };

        // Already mapped: hand back the same handle so the JVM treats it as the
        // same module.
        if let Some(index) = guard.by_path.get(&key) {
            return ManagedResolution::Managed(synthetic_handle(*index));
        }

        let Some(library) = guard.available.remove(&key) else {
            return ManagedResolution::NotManaged;
        };

        let module = match crate::native::lib::map_image(library.image()) {
            Ok(module) => module,
            Err(error) => {
                eprintln!("jvmsense: cannot map native library {key}: {error}");
                // Put it back so a retry reports the same specific error.
                guard.available.insert(key, library);
                return ManagedResolution::NotManaged;
            }
        };

        let index = guard.next_index;
        guard.next_index += 1;
        guard.by_path.insert(key.clone(), index);

        // Register the module's exports so `GetProcAddress` can answer for the
        // synthetic handle.
        let exports = module
            .exported_names()
            .iter()
            .map(|(name, rva)| {
                let address = module
                    .base()
                    .cast::<u8>()
                    .wrapping_add(*rva as usize)
                    .cast::<std::ffi::c_void>();
                (name.clone(), address as usize)
            })
            .collect::<HashMap<_, _>>();
        drop(module);

        if crate::native::lib::register_synthetic_module(index, key.clone(), exports).is_err() {
            return ManagedResolution::NotManaged;
        }

        ManagedResolution::Managed(synthetic_handle(index))
    }));

    prepare_trampolines()?;
    enable()?;

    Ok(vec![
        "LoadLibraryExW",
        "LoadLibraryW",
        "GetProcAddress",
        "FreeLibrary",
    ])
}

unsafe fn store(trampolines: &mut Win32Trampolines, symbol: &str, original: *mut c_void) {
    match symbol {
        "LoadLibraryExW" => {
            trampolines.load_library_ex_w =
                std::mem::transmute::<*mut c_void, LoadLibraryExWFn>(original);
        }
        "LoadLibraryW" => {
            trampolines.load_library_w =
                std::mem::transmute::<*mut c_void, LoadLibraryWFn>(original);
        }
        "GetProcAddress" => {
            trampolines.get_proc_address =
                std::mem::transmute::<*mut c_void, GetProcAddressFn>(original);
        }
        "FreeLibrary" => {
            trampolines.free_library = std::mem::transmute::<*mut c_void, FreeLibraryFn>(original);
        }
        _ => {}
    }
}

fn wide_to_string(name: *const u16) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // Bound the scan: a malformed pointer must not walk into unmapped memory
    // looking for a terminator that is not there.
    const MAX_UNITS: usize = 32 * 1024;
    let mut len = 0usize;
    unsafe {
        while len < MAX_UNITS && *name.add(len) != 0 {
            len += 1;
        }
        if len == 0 || len >= MAX_UNITS {
            return None;
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(
            name, len,
        )))
    }
}

fn c_string(name: *const u8) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // A `GetProcAddress` name is either a name pointer or an ordinal in the low
    // 16 bits; an ordinal has zeros in the high half, which cannot be a valid
    // string start on a 64-bit build.
    if (name as usize) >> 16 == 0 {
        return None;
    }
    let limit = 4096usize;
    unsafe {
        let mut len = 0usize;
        while len < limit && *name.add(len) != 0 {
            len += 1;
        }
        if len == 0 || len >= limit {
            return None;
        }
        Some(String::from_utf8_lossy(std::slice::from_raw_parts(name, len)).into_owned())
    }
}

extern "system" fn unreachable_load_ex_w(_: *const u16, _: *mut c_void, _: u32) -> HMODULE {
    unreachable!("LoadLibraryExW detour called before trampolines were installed")
}
extern "system" fn unreachable_load_w(_: *const u16) -> HMODULE {
    unreachable!("LoadLibraryW detour called before trampolines were installed")
}
extern "system" fn unreachable_get_proc_address(_: HMODULE, _: *const u8) -> *mut c_void {
    unreachable!("GetProcAddress detour called before trampolines were installed")
}
extern "system" fn unreachable_free_library(_: HMODULE) -> i32 {
    unreachable!("FreeLibrary detour called before trampolines were installed")
}

/// Keep the direct import referenced so an unused-import warning does not hide
/// that this module intentionally goes through the trampolines instead.
const _: () = {
    let _ = Win32FreeLibrary;
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_handles_round_trip() {
        for index in [0usize, 1, 42, 1000] {
            let handle = synthetic_handle(index);
            assert!(is_synthetic(handle));
            assert_eq!(synthetic_index(handle), index);
        }
    }

    #[test]
    fn a_real_module_handle_is_not_synthetic() {
        // The module this test binary is loaded from is a genuine HMODULE.
        let kernel32 = unsafe {
            LoadLibraryW(
                "kernel32.dll"
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect::<Vec<u16>>()
                    .as_ptr(),
            )
        };
        assert!(!is_synthetic(kernel32));
        assert!(
            (kernel32 as usize) < SYNTHETIC_HANDLE_BASE,
            "real handles live below the synthetic range"
        );
    }

    #[test]
    fn a_null_path_is_not_managed() {
        assert!(wide_to_string(std::ptr::null()).is_none());
    }

    #[test]
    fn an_ordinal_lookup_is_not_treated_as_a_name() {
        // An ordinal is a small integer, not a pointer.
        assert!(c_string(std::ptr::null()).is_none());
        assert!(c_string(7usize as *const u8).is_none());
    }

    #[test]
    fn resolution_without_an_installed_resolver_falls_through() {
        // No resolver installed in this test binary, so every path is the OS's.
        assert_eq!(resolve("anything.dll"), ManagedResolution::NotManaged);
    }
}
