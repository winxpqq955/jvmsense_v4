//! End-to-end test: a real JVM reads a jar that only exists in memory.
//!
//! This is the productized form of the V1 probe. Where V1 proved the technique
//! with a throwaway binary, this drives the real [`VirtualFileSystem`] and the
//! real `native::HookSet` — the same code paths a launch uses — and asserts
//! that `java.util.zip.ZipFile` returns the artifact's bytes while the file on
//! disk stays zero-length.
//!
//! It is `#[ignore]`d by default because it creates a real JVM, which is slow
//! and requires a JDK. Run it with:
//!
//! ```text
//! cargo test --test hollow_jar_end_to_end -- --ignored --nocapture
//! ```
//!
//! The JDK is located from `JVMSENSE_TEST_JRE`, falling back to the bundled
//! JRE the predecessor shipped.

#![cfg(windows)]

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use jni::objects::JValue;
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::HookSet;
use jvmsense_core::vfs::{ArtifactRole, VirtualFileSystem};

mod support;

/// Build a jar with the given entries and return its path.
fn build_jar(path: &Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).expect("create jar");
    let mut zw = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        zw.start_file(*name, opts).expect("start_file");
        zw.write_all(data).expect("write entry");
    }
    zw.finish().expect("finish jar");
}

#[test]
#[ignore = "creates a real JVM; run with --ignored"]
fn a_jvm_reads_a_hollow_jar_from_memory() {
    let jdk = support::test_jdk();

    let work = tempfile::tempdir().expect("tempdir");

    // The artifact: a jar with a class-like entry and a resource.
    let source_jar = work.path().join("source.jar");
    let class_bytes: Vec<u8> = (0u8..=255).cycle().take(70_000).collect();
    build_jar(
        &source_jar,
        &[
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\n\r\n"),
            ("net/minecraft/client/main/Main.class", &class_bytes),
            ("version.json", br#"{"id":"1.21.4"}"#),
        ],
    );

    // Mount it: load, verify, index, and materialize a hollow placeholder.
    let mut vfs = VirtualFileSystem::create(work.path().join("session")).expect("create vfs");
    vfs.mount(
        ArtifactRole::Game,
        "game.jar",
        &jvmsense_core::vfs::identity_spec("game.jar", source_jar).expect("spec"),
        true,
    )
    .expect("mount");

    let hollow_path = vfs
        .with_role(ArtifactRole::Game)
        .next()
        .expect("mounted")
        .path()
        .to_path_buf();
    assert_eq!(
        std::fs::metadata(&hollow_path).expect("stat").len(),
        0,
        "the placeholder must start empty"
    );

    // Create the JVM. The classpath is deliberately empty: nothing on disk
    // should be needed to read the jar.
    let empty_classpath = work.path().join("empty-classpath");
    let args = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={}", empty_classpath.display()))
        .build()
        .expect("init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");

    // Install the hooks now that java.dll exists.
    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install hooks");
    // Derived rather than hard-coded: the hook set grows as more of the JVM's
    // read paths are covered, and a literal here would go stale silently.
    let expected = jvmsense_core::native::symbols::hooked_symbol_count();
    assert_eq!(
        hooks.installed().len(),
        expected,
        "every symbol the harness knows about should be installed"
    );

    // Drive the read with the session published to this thread.
    let vfs = Arc::new(vfs);
    let (entry_size, entry_bytes, jarfile_ok, manifest_ok) =
        jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
            let mut env = jvm.attach_current_thread().expect("attach");

            let path = env
                .new_string(hollow_path.to_string_lossy().as_ref())
                .expect("new_string");
            let file = env
                .new_object(
                    "java/io/File",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&path)],
                )
                .expect("new File");

            // `File.length()` must report the *virtual* length even though the
            // placeholder on disk is zero bytes. Asserting the real length here
            // is what caught the `WinNTFileSystem` hook being missing: a zero
            // here means the hook is not serving this path.
            let reported_len = env
                .call_method(&file, "length", "()J", &[])
                .expect("File.length")
                .j()
                .expect("j");
            assert!(
                reported_len > 0,
                "File.length() must report the virtual length, not the placeholder's zero"
            );

            let zip = match env.new_object(
                "java/util/zip/ZipFile",
                "(Ljava/io/File;)V",
                &[JValue::Object(&file)],
            ) {
                Ok(zip) => zip,
                Err(error) => {
                    eprintln!("ZipFile failed: {error:?}");
                    eprintln!("trace: {:?}", jvmsense_core::native::trace_snapshot());
                    env.exception_describe().ok();
                    panic!("new ZipFile — the hooks must serve this");
                }
            };

            let name = env
                .new_string("net/minecraft/client/main/Main.class")
                .expect("string");
            let entry = env
                .call_method(
                    &zip,
                    "getEntry",
                    "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
                    &[JValue::Object(&name)],
                )
                .expect("getEntry")
                .l()
                .expect("entry");
            assert!(!entry.is_null(), "getEntry returned null");

            let size = env
                .call_method(&entry, "getSize", "()J", &[])
                .expect("getSize")
                .j()
                .expect("j");

            let stream = env
                .call_method(
                    &zip,
                    "getInputStream",
                    "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;",
                    &[JValue::Object(&entry)],
                )
                .expect("getInputStream")
                .l()
                .expect("stream");
            let array = env
                .call_method(&stream, "readAllBytes", "()[B", &[])
                .expect("readAllBytes")
                .l()
                .expect("array");
            let array = unsafe { jni::objects::JByteArray::from_raw(array.into_raw()) };
            let bytes = env.convert_byte_array(&array).expect("convert");

            let jar_file = env
                .new_object(
                    "java/util/jar/JarFile",
                    "(Ljava/io/File;)V",
                    &[JValue::Object(&file)],
                )
                .expect("new JarFile");
            let manifest = env
                .call_method(&jar_file, "getManifest", "()Ljava/util/jar/Manifest;", &[])
                .expect("getManifest")
                .l()
                .expect("manifest");

            (size, bytes, true, !manifest.is_null())
        });

    assert_eq!(entry_size as usize, class_bytes.len(), "declared size");
    assert_eq!(entry_bytes, class_bytes, "bytes came back from memory");
    assert!(jarfile_ok, "JarFile opened");
    assert!(manifest_ok, "manifest was parsed from memory");

    // The whole point: nothing was written.
    let footprint = vfs.disk_footprint();
    assert!(
        jvmsense_core::vfs::hollow::all_placeholders_empty(&footprint),
        "no artifact byte may reach disk: {footprint:?}"
    );

    let (hits, misses) = jvmsense_core::native::hit_miss_counts();
    println!("hook hits: {hits}, misses: {misses}");
    assert!(hits > 0, "the hooks must have served the reads");

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}
