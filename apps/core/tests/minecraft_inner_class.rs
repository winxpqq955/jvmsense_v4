//! Minimal reproduction for the repeated real-Minecraft startup failure.
//!
//! The crash reports say `NoClassDefFoundError: af$2`, but the source client
//! jar contains `af$2.class`. This test isolates the two paths that matter:
//! `JarFile` central-directory lookup and the application class loader. It does
//! not start Fabric or Minecraft, so a failure names the broken layer instead
//! of hiding behind game bootstrap.
//!
//! Run with:
//!
//! ```text
//! cargo test --test minecraft_inner_class -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::path::Path;
use std::sync::Arc;

use jni::objects::JValue;
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::HookSet;
use jvmsense_core::vfs::{ArtifactRole, VirtualFileSystem};

mod support;

fn client_jar() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("minecraft")
        .join("client.jar")
}

#[test]
#[ignore = "creates a real JVM and mounts the 28 MB Minecraft client"]
fn minecraft_inner_class_is_visible_to_jarfile_and_system_classloader() {
    let source = client_jar();
    assert!(
        source.is_file(),
        "client fixture missing: {}",
        source.display()
    );

    let work = tempfile::tempdir().expect("tempdir");
    let mut vfs = VirtualFileSystem::create(work.path().join("session")).expect("create vfs");
    vfs.mount(
        ArtifactRole::Game,
        "client.jar",
        &jvmsense_core::vfs::identity_spec("client.jar", source.clone()).expect("fixture spec"),
        true,
    )
    .expect("mount client jar");

    let mounted = vfs.with_role(ArtifactRole::Game).next().expect("mounted");
    assert!(
        mounted
            .index()
            .expect("indexed")
            .class_entry("af$2")
            .is_some(),
        "the Rust jar index must see af$2"
    );
    let hollow_path = mounted.path().to_path_buf();
    assert!(
        !hollow_path.exists(),
        "the virtual artifact path must not exist on disk"
    );

    let jdk = support::test_jdk();
    let args = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={}", hollow_path.display()))
        .option("-Xshare:off")
        .build()
        .expect("init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");
    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install read hooks");

    jvmsense_core::native::reset_trace();
    let vfs = Arc::new(vfs);
    let (jar_entry_found, class_loaded) =
        jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
            let mut env = jvm.attach_current_thread().expect("attach");

            let path = env
                .new_string(hollow_path.to_string_lossy().as_ref())
                .expect("path string");
            let file = env
                .new_object(
                    "java/io/File",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&path)],
                )
                .expect("File");
            let jar = env
                .new_object(
                    "java/util/jar/JarFile",
                    "(Ljava/io/File;)V",
                    &[JValue::Object(&file)],
                )
                .expect("JarFile");
            let entry_name = env.new_string("af$2.class").expect("entry name");
            let entry = env
                .call_method(
                    &jar,
                    "getJarEntry",
                    "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
                    &[JValue::Object(&entry_name)],
                )
                .expect("getJarEntry")
                .l()
                .expect("entry value");
            let jar_entry_found = !entry.is_null();

            let class_name = env.new_string("af$2").expect("class name");
            let class = env
                .call_static_method(
                    "java/lang/Class",
                    "forName",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&class_name)],
                )
                .expect("Class.forName")
                .l()
                .expect("Class value");

            (jar_entry_found, !class.is_null())
        });

    println!("JarFile entry: {jar_entry_found}; Class.forName: {class_loaded}");
    let trace = jvmsense_core::native::trace_snapshot();
    println!(
        "trace: hits={}, misses={}, entries={:?}",
        trace.hits, trace.misses, trace.entries
    );
    assert!(
        jar_entry_found,
        "JarFile must see af$2.class in the hollow jar"
    );
    assert!(class_loaded, "the system class loader must load af$2");

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}
