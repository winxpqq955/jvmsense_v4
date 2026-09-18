//! A real Windows DLL used to prove the memory loader works end to end.
//!
//! Built with `rustc --crate-type cdylib` so the fixture needs no C toolchain,
//! and so it is a genuine PE image with a genuine export table — which is
//! exactly what `LoadLibrary` would map and what the memory loader must map
//! instead.
//!
//! The exports deliberately cover the shapes a JNI library has:
//!   - a plain C export returning a value, to prove a mapped call works
//!   - an export that allocates, to prove the mapped code can use the heap
//!   - `JNI_OnLoad`, the entry point a JNI library must provide

/// Returns a value that identifies this build, so a call through the mapping
/// proves the code really ran.
#[no_mangle]
pub extern "system" fn jvmsense_probe_value() -> i32 {
    0x5E_5E_5E
}

/// Sums two integers. Used to prove arguments cross the boundary correctly.
#[no_mangle]
pub extern "system" fn jvmsense_probe_add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

/// Allocates and returns a pointer to a NUL-terminated string.
///
/// Proves the mapped image can call into the allocator, which is the thing
/// most likely to fail if the mapping is wrong.
#[no_mangle]
pub extern "system" fn jvmsense_probe_message() -> *const u8 {
    static MESSAGE: &[u8] = b"mapped from memory\0";
    MESSAGE.as_ptr()
}

/// The entry point a JNI library exports.
///
/// This fixture is not a real JNI library, but its presence proves the export
/// table carries the name the loader will be asked for.
#[no_mangle]
pub extern "system" fn JNI_OnLoad(_vm: *mut core::ffi::c_void, _reserved: *mut core::ffi::c_void) -> i32 {
    // JNI_VERSION_1_8
    0x0001_0008
}
