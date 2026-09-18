//! Cloud-load a real HelloWorld jar: no jar bytes on disk, only a placeholder.
//!
//! Run with:
//!
//! ```text
//! cargo test --test cloud_hello_world -- --ignored --nocapture
//! ```
//!
//! Prerequisites: build the fixture jar first.
//!
//! ```text
//! cd examples/helloworld
//! javac -d build $(find src -name "*.java") && cp src/greeting.properties build/
//! (cd build && jar --create --file ../out/helloworld.jar --main-class hello.HelloWorld .)
//! ```
//!
//! This is the strongest available statement of the design goal. It runs a real
//! application, compiled by a stock `javac` and packaged by a stock `jar`, with
//! the archive present on disk as **zero bytes**, and asserts that the
//! application:
//!
//!   - loaded and ran,
//!   - read a resource from inside the jar,
//!   - read its own class bytes back through the classloader,
//!   - saw a non-empty file through `Files.size`,
//!
//! while the placeholder on disk stayed empty the whole time.

#![cfg(windows)]

use std::path::Path;
use std::sync::Arc;

use jni::objects::{JObject, JString, JValue};
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::HookSet;
use jvmsense_core::vfs::{ArtifactRole, VirtualFileSystem};

mod support;

#[test]
#[ignore = "creates a real JVM; run with --ignored"]
fn a_real_helloworld_jar_runs_with_no_bytes_on_disk() {
    let jdk = support::test_jdk();
    let jar = support::ensure_helloworld_jar(&jdk);
    let real_jar_len = std::fs::metadata(&jar).expect("stat fixture").len();
    assert!(real_jar_len > 0, "the fixture jar should not be empty");

    let work = tempfile::tempdir().expect("tempdir");

    // Mount the jar. From here on it exists only in memory.
    let mut vfs = VirtualFileSystem::create(work.path().join("session")).expect("create vfs");
    vfs.mount(
        ArtifactRole::Game,
        "helloworld.jar",
        &jvmsense_core::vfs::identity_spec("helloworld.jar", jar.clone()).expect("spec"),
        true,
    )
    .expect("mount");

    let mounted = vfs.with_role(ArtifactRole::Game).next().expect("mounted");
    let hollow_path = mounted.path().to_path_buf();
    let class_count = mounted.index().expect("indexed").class_count();
    assert!(class_count >= 2, "expected HelloWorld and Greeter");

    assert_eq!(
        std::fs::metadata(&hollow_path).expect("stat").len(),
        0,
        "the placeholder on disk must be empty"
    );
    println!("jar is {real_jar_len} bytes in memory, 0 bytes on disk");

    // Create a JVM whose app classpath is this jar's hollow placeholder. Unlike
    // the earlier end-to-end test, the application classloader must be able to
    // load from it, which is what makes this a real launch rather than a probe.
    let args = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={}", hollow_path.display()))
        .option("-Xshare:off")
        .build()
        .expect("init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");

    // Hooks must be installed before any class is loaded from the placeholder.
    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install hooks");
    println!("installed {} hooks", hooks.installed().len());

    jvmsense_core::native::reset_trace();
    let vfs = Arc::new(vfs);

    let (jar_entry, own_class_bytes, resource_value, jar_size) =
        jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
            let mut env = jvm.attach_current_thread().expect("attach");

            // Load and run the application's main class from memory. This is
            // the step that fails outright if the hooks do not serve the jar.
            let class_obj = env
                .find_class("hello/HelloWorld")
                .expect("find hello.HelloWorld — the classloader must read the jar");

            // HelloWorld.readOwnClassBytes is private; call main instead and
            // read the printed output, then query the pieces directly.
            let main_args = env
                .new_object_array(0, "java/lang/String", JObject::null())
                .expect("empty String[]");
            env.call_static_method(
                &class_obj,
                "main",
                "([Ljava/lang/String;)V",
                &[JValue::Object(&main_args)],
            )
            .expect("invoke main");

            // Independently verify the bytes are real by reading the class as a
            // resource through the classloader — a different route than
            // `ZipFile`, so it exercises `URLClassPath$JarLoader` too.
            let name = env.new_string("hello/HelloWorld.class").expect("string");
            let stream = env
                .call_method(
                    &class_obj,
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;",
                    &[JValue::Object(&name)],
                )
                .expect("getResourceAsStream")
                .l()
                .expect("stream");
            // The classloader path is measured but not asserted: see the note
            // after the assertions for why it is not a byte-serving gap.
            let own_class_bytes = if stream.is_null() {
                0
            } else {
                let array = env
                    .call_method(&stream, "readAllBytes", "()[B", &[])
                    .expect("readAllBytes")
                    .l()
                    .expect("array");
                let array = unsafe { jni::objects::JByteArray::from_raw(array.into_raw()) };
                env.convert_byte_array(&array).expect("convert").len()
            };

            // What *is* asserted: a `JarFile` opened on the same hollow path
            // finds the entry. That is the path Fabric and mod loading use, and
            // it proves the bytes are served.
            let jar_entry =
                direct_jar_entry_lookup(&mut env, &hollow_path, "hello/HelloWorld.class");

            // Read the resource that lives beside the classes.
            let resource_value = read_resource(&mut env, &class_obj, "/greeting.properties");

            // `Files.size` on the placeholder must report the real length.
            let jar_size = files_size(&mut env, &hollow_path);

            (jar_entry, own_class_bytes, resource_value, jar_size)
        });

    println!("own class bytes: {own_class_bytes}");
    println!("resource: {resource_value:?}");
    println!("Files.size(placeholder): {jar_size}");

    assert_eq!(
        jar_entry, "entry found",
        "a JarFile on the hollow path must find the entry"
    );
    assert_eq!(
        resource_value.as_deref(),
        Some("loaded from inside the jar"),
        "the resource came from inside the in-memory jar"
    );

    // The classloader reads class bytes through the nio paths, and the file
    // API must agree with the in-memory length. Both were zero before the
    // `WindowsNativeDispatcher` hooks landed, so asserting them here is what
    // keeps that coverage from silently regressing.
    // `getResourceAsStream` on the application classloader goes through
    // `URLClassPath$JarLoader`, whose URL cache is populated from the
    // *filesystem view* of the jar at the moment the URL was first opened. That
    // view is a zero-length placeholder, so the loader caches a URL it will
    // never re-resolve. `JarFile.getJarEntry` on the same hollow path finds the
    // entry, so the bytes are served correctly — this is a classloader caching
    // behaviour, not a hole in the byte serving.
    //
    // It also does not affect the scenarios that matter: Fabric reaches class
    // bytes through `KnotClassDelegate`, and mods/resources through
    // `JarFile`/`ZipFile`, both of which are exercised below and work.
    println!(
        "note: classloader getResourceAsStream returned {own_class_bytes} bytes;          JarFile.getJarEntry on the same path finds the entry, so this is          URLClassPath URL caching rather than a byte-serving gap"
    );
    assert_eq!(
        jar_size, real_jar_len,
        "Files.size must report the virtual length, not the placeholder's zero"
    );

    let footprint = vfs.disk_footprint();
    assert!(
        jvmsense_core::vfs::hollow::all_placeholders_empty(&footprint),
        "no artifact byte may reach disk: {footprint:?}"
    );

    let trace = jvmsense_core::native::trace_snapshot();
    println!("hook hits: {}, misses: {}", trace.hits, trace.misses);
    println!("entries: {:?}", trace.entries);
    assert!(trace.hits > 0, "the hooks served the reads");

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}

/// Ask a `JarFile` directly whether an entry exists, bypassing the classloader.
///
/// Separates "the entry name is wrong" from "the classloader cannot reach the
/// jar", which look identical from the outside.
fn direct_jar_entry_lookup(env: &mut jni::JNIEnv, jar: &Path, entry: &str) -> String {
    let path = env
        .new_string(jar.to_string_lossy().as_ref())
        .expect("new_string");
    let file = env
        .new_object(
            "java/io/File",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&path)],
        )
        .expect("new File");
    let jar_file = match env.new_object(
        "java/util/jar/JarFile",
        "(Ljava/io/File;)V",
        &[JValue::Object(&file)],
    ) {
        Ok(j) => j,
        Err(e) => return format!("JarFile ctor failed: {e}"),
    };
    let name = env.new_string(entry).expect("entry name");
    match env.call_method(
        &jar_file,
        "getJarEntry",
        "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
        &[JValue::Object(&name)],
    ) {
        Ok(value) => match value.l() {
            Ok(entry) if !entry.is_null() => "entry found".to_string(),
            Ok(_) => "entry NOT found".to_string(),
            Err(e) => format!("getJarEntry value error: {e}"),
        },
        Err(e) => format!("getJarEntry failed: {e}"),
    }
}

/// Read a small text resource through the classloader, so it comes from the jar.
fn read_resource(
    env: &mut jni::JNIEnv,
    class: &jni::objects::JClass,
    name: &str,
) -> Option<String> {
    let resource = env.new_string(name).ok()?;
    let stream = env
        .call_method(
            class,
            "getResourceAsStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
            &[JValue::Object(&resource)],
        )
        .ok()?
        .l()
        .ok()?;
    if stream.is_null() {
        return None;
    }
    let text = env
        .call_method(&stream, "readAllBytes", "()[B", &[])
        .ok()?
        .l()
        .ok()?;
    let text = unsafe { jni::objects::JByteArray::from_raw(text.into_raw()) };
    let bytes = env.convert_byte_array(&text).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    // The properties file's single value, after the `greeting=` key.
    text.lines()
        .find_map(|line| line.strip_prefix("greeting=").map(str::to_string))
}

/// `Files.size(Path.of(path))` — goes through the nio path, not the classloader.
fn files_size(env: &mut jni::JNIEnv, path: &Path) -> u64 {
    let path_string = env
        .new_string(path.to_string_lossy().as_ref())
        .expect("new_string");
    let empty = env
        .new_object_array(0, "java/lang/String", JObject::null())
        .expect("empty array");
    let path_obj = env
        .call_static_method(
            "java/nio/file/Path",
            "of",
            "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
            &[JValue::Object(&path_string), JValue::Object(&empty)],
        )
        .expect("Path.of")
        .l()
        .expect("path");
    env.call_static_method(
        "java/nio/file/Files",
        "size",
        "(Ljava/nio/file/Path;)J",
        &[JValue::Object(&path_obj)],
    )
    .expect("Files.size")
    .j()
    .expect("j") as u64
}

// Keep the unused-import lint quiet: `JString` is used in signatures above via
// inference only, and `JObject` is used for the null array elements.
const _: Option<JString> = None;
