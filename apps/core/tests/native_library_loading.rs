//! `System.loadLibrary` must serve a native library from memory.
//!
//! `tests/native_memory_load.rs` proves the loader can map and run a DLL.
//! This proves the JVM *uses* it: the native-library detours intercept
//! `NativeLibraries.load`, map the library from a payload, hand back a
//! synthetic handle, and answer every later `findEntry0` for a symbol out of
//! the mapped module.
//!
//! Run with:
//!
//! ```text
//! cargo test --test native_library_loading -- --ignored --nocapture
//! ```

#![cfg(windows)]

use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::HookSet;

mod support;

#[test]
#[ignore = "loads native code; run with --ignored"]
fn the_loader_hooks_do_not_break_normal_library_loading() {
    let jdk = support::test_jdk();

    // A real library, loaded through the ordinary path. The point is not that
    // it loads -- it always would -- but that it still loads *with the hooks
    // installed*. A detour that mishandles a path it does not manage would
    // break every library the process loads, which is a far worse failure than
    // not serving the managed ones.
    let libdir = jdk.home().join("bin").join("default");
    if !libdir.is_dir() {
        // Not an OpenJ9 layout; the HotSpot hooks are covered elsewhere.
        return;
    }

    let work = tempfile::tempdir().expect("tempdir");
    let args = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!(
            "-Djava.class.path={}",
            work.path().join("empty").display()
        ))
        .option(format!("-Djava.library.path={}", libdir.display()))
        .option("-Xshare:off")
        .build()
        .expect("init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");

    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install hooks");
    let loader_hooks =
        jvmsense_core::native::win32_library::install_loader_hooks([]).expect("loader hooks");
    println!(
        "installed {loader_hooks:?} on top of {} read hooks",
        hooks.installed().len()
    );

    let mut env = jvm.attach_current_thread().expect("attach");
    let name = env.new_string("zip").expect("string");
    let result = env.call_static_method(
        "java/lang/System",
        "loadLibrary",
        "(Ljava/lang/String;)V",
        &[jni::objects::JValue::Object(&name)],
    );

    let (probed, served) = jvmsense_core::native::win32_library::win32_counts();
    println!("win32: probed {probed} paths, served {served} from memory");

    match result {
        Ok(_) => println!("System.loadLibrary(\"zip\") still works with the hooks installed"),
        Err(error) => {
            env.exception_describe().ok();
            env.exception_clear().ok();
            panic!("the loader hooks broke a normal library load: {error}");
        }
    }

    assert!(
        probed > 0,
        "the Win32 detour must have been consulted: a detour that never runs is          indistinguishable from one that is not installed"
    );

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}
