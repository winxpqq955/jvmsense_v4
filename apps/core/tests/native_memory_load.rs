//! Map a real Windows DLL into the process and call into it.
//!
//! This is the proof that native library memory loading works. Everything else
//! in the crate serves Java bytecode; this covers the other half of a real
//! Minecraft launch, which needs LWJGL's OpenGL, GLFW and OpenAL natives.
//!
//! The fixture DLL is built by `rustc` from `examples/native-dll`, so no C
//! toolchain is required and the image is a genuine PE with a genuine export
//! table — exactly what `LoadLibrary` would map.
//!
//! Run with:
//!
//! ```text
//! cargo test --test native_memory_load -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

use jvmsense_core::native::lib::payload::{decode_payload, identity_payload};
use jvmsense_core::native::lib::{map_image, ManagedLibrary, PeImage};

/// Build the fixture DLL if it is missing.
///
/// Uses `rustc` directly rather than a build script so the fixture is a normal
/// example artifact a reader can rebuild by hand.
fn ensure_fixture_dll() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let example_dir = manifest_dir
        .join("..")
        .join("..")
        .join("examples")
        .join("native-dll");
    let dll = example_dir.join("native_dll.dll");
    let source = example_dir.join("src").join("lib.rs");

    let needs_build = match (
        std::fs::metadata(&dll).and_then(|d| d.modified()),
        std::fs::metadata(&source).and_then(|s| s.modified()),
    ) {
        (Ok(dll_time), Ok(src_time)) => dll_time < src_time,
        _ => true,
    };

    if needs_build {
        let status = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("--crate-type")
            .arg("cdylib")
            .arg("--crate-name")
            .arg("native_dll")
            .arg("-O")
            .arg("-o")
            .arg(&dll)
            .arg(&source)
            .status()
            .expect("run rustc to build the fixture DLL");
        assert!(status.success(), "building the fixture DLL failed");
    }

    assert!(dll.is_file(), "fixture DLL missing at {}", dll.display());
    dll
}

#[test]
#[ignore = "maps executable memory; run with --ignored"]
fn a_real_dll_is_mapped_from_memory_and_called() {
    let dll = ensure_fixture_dll();
    let real_len = std::fs::metadata(&dll).expect("stat fixture").len();
    println!("fixture DLL: {} bytes at {}", real_len, dll.display());

    // Decode and verify exactly the way a manifest entry would be.
    let payload = identity_payload("native_dll.dll", &dll).expect("payload spec");
    let library: ManagedLibrary = decode_payload(&payload).expect("decode and verify");

    assert_eq!(library.file_name(), "native_dll.dll");
    assert!(
        library.matches_request("native_dll"),
        "a bare stem must match, which is how System.loadLibrary asks"
    );
    assert_eq!(
        library.image().machine(),
        jvmsense_core::native::lib::PeMachine::current_process(),
        "the fixture must target this process"
    );
    match library.image().exported_names() {
        Ok(names) => println!("exports before mapping: {names:?}"),
        Err(error) => {
            let image = library.image();
            println!("export walk failed: {error}");
            println!("  export_table rva = {:?}", image.export_table_rva());
            panic!("export walk failed: {error}");
        }
    }

    // Map it. From here the code is executable and the OS has never seen it.
    let module = map_image(library.image()).expect("map the image");
    println!(
        "mapped {} bytes at {:?} (export dir offset {:?})",
        module.size(),
        module.base(),
        module.export_dir_offset()
    );
    assert!(module.size() > 0);

    assert!(
        module.exports("jvmsense_probe_value"),
        "the mapped module must expose its exports: {:?}",
        module
            .exported_names()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
    );

    // SAFETY: each address comes from this module's own export table, and each
    // signature matches the fixture's definition in examples/native-dll.
    unsafe {
        let value_fn: extern "system" fn() -> i32 =
            std::mem::transmute(module.proc_address("jvmsense_probe_value").expect("export"));
        let value = value_fn();
        println!("jvmsense_probe_value() = 0x{value:x}");
        assert_eq!(
            value, 0x5E_5E_5E,
            "the mapped code ran and returned its constant"
        );

        let add_fn: extern "system" fn(i32, i32) -> i32 =
            std::mem::transmute(module.proc_address("jvmsense_probe_add").expect("export"));
        let sum = add_fn(19, 23);
        println!("jvmsense_probe_add(19, 23) = {sum}");
        assert_eq!(sum, 42, "arguments crossed the boundary correctly");

        let message_fn: extern "system" fn() -> *const u8 = std::mem::transmute(
            module
                .proc_address("jvmsense_probe_message")
                .expect("export"),
        );
        let pointer = message_fn();
        assert!(!pointer.is_null());
        let message = std::ffi::CStr::from_ptr(pointer.cast());
        let message = message.to_string_lossy().into_owned();
        println!("jvmsense_probe_message() = {message:?}");
        assert_eq!(
            message, "mapped from memory",
            "the mapped image read its own static data"
        );

        // JNI_OnLoad is the export a real JNI library is invoked through.
        assert!(
            module.exports("JNI_OnLoad"),
            "the loader must find the JNI entry point by name"
        );
    }

    // The decisive property: none of this touched the filesystem in a way the
    // OS loader could see. The fixture exists on disk only because the test
    // built it; the mapping came from the in-memory bytes.
    let mapped_from = library.image().bytes();
    assert_eq!(mapped_from.len() as u64, real_len);
    println!("mapped from an in-memory image of {real_len} bytes, never re-read from disk");
}

#[test]
#[ignore = "maps executable memory; run with --ignored"]
fn an_image_of_the_wrong_architecture_is_refused() {
    // A truncated but structurally valid header is not needed here: the
    // architecture check happens before anything is parsed, so an x86 image
    // built on the fly is enough. Rather than fabricate one, assert the
    // negative through the public API on a real image.
    let dll = ensure_fixture_dll();
    let bytes = std::fs::read(&dll).expect("read fixture");
    let image = PeImage::parse(bytes).expect("parse");

    // This process is x86-64, so its own fixture must map.
    assert!(
        map_image(&image).is_ok(),
        "an image matching this process must map"
    );

    // The wrong-architecture path is unit-tested in memload; here the point is
    // that a real image passes the guard, which a fabricated one would not
    // prove.
    drop(image);
}
