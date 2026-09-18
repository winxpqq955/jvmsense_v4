//! Every read path a JVM uses for a classpath jar must be served from memory.
//!
//! This began as a diagnostic: `ZipFile` worked with the first hook set while
//! `Files.size` and `File.length()` returned zero, which identified exactly
//! which native families were missing. Now that all of them work it asserts
//! the full set, so a regression in any one path fails loudly instead of
//! silently returning a short or empty file.
//!
//! The paths are not redundant. `ZipFile` and `JarFile` go through
//! `RandomAccessFile`; `Files.*` goes through `sun.nio.fs`'s
//! `WindowsNativeDispatcher` to open a handle and then reads it via
//! `FileDispatcherImpl`; `File.length()` goes through `WinNTFileSystem`. A
//! hook set that covers one and not the others produces an application that
//! half-works, which is far harder to diagnose than one that fails outright.
//!
//! Run with:
//!
//! ```text
//! cargo test --test read_path_probe -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::path::Path;
use std::sync::Arc;

use jni::objects::JValue;
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::HookSet;
use jvmsense_core::vfs::{ArtifactRole, VirtualFileSystem};

mod support;

/// One read path, as a Java expression to evaluate and a JNI signature.
struct PathProbe {
    label: &'static str,
    /// A `java.nio.file.Path` built from the virtual artifact.
    java: &'static str,
}

#[test]
#[ignore = "creates a real JVM; run with --ignored"]
fn report_which_read_paths_the_current_hooks_serve() {
    let jdk = support::test_jdk();
    let jar = support::ensure_helloworld_jar(&jdk);
    let real_len = std::fs::metadata(&jar).expect("stat").len();

    let work = tempfile::tempdir().expect("tempdir");
    let mut vfs = VirtualFileSystem::create(work.path().join("session")).expect("create vfs");
    vfs.mount(
        ArtifactRole::Game,
        "helloworld.jar",
        &jvmsense_core::vfs::identity_spec("helloworld.jar", jar).expect("spec"),
        true,
    )
    .expect("mount");
    let hollow = vfs
        .with_role(ArtifactRole::Game)
        .next()
        .expect("mounted")
        .path()
        .to_path_buf();
    assert!(
        !hollow.exists(),
        "the virtual artifact must have no disk file"
    );

    let args = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={}", hollow.display()))
        .option("-Xshare:off")
        .build()
        .expect("init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");
    let _hooks = HookSet::install_read_hooks(jdk.home()).expect("install hooks");

    let vfs = Arc::new(vfs);
    let results = jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
        let mut env = jvm.attach_current_thread().expect("attach");
        let mut results = Vec::new();

        for probe in probes() {
            let outcome = run_probe(&mut env, &hollow, &probe, real_len);
            results.push((probe.label, outcome));
        }
        results
    });

    println!("=== read paths against a virtual artifact ===");
    println!("(real jar length is {real_len} bytes)\n");

    let mut failures = Vec::new();
    for (label, outcome) in &results {
        println!("{label:<46} {outcome}");
        if !outcome.starts_with("OK") {
            failures.push(format!("{label}: {outcome}"));
        }
    }

    let trace = jvmsense_core::native::trace_snapshot();
    println!("\ntrace: {:?}", trace.entries);

    assert!(
        failures.is_empty(),
        "these read paths were not served from memory:\n  {}",
        failures.join("\n  ")
    );
    assert!(trace.hits > 0, "the hooks served the reads");

    drop(_hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}

fn probes() -> Vec<PathProbe> {
    vec![
        PathProbe {
            label: "new ZipFile(file).getInputStream(entry)",
            java: "ZipFile",
        },
        PathProbe {
            label: "new JarFile(file).getInputStream(entry)",
            java: "JarFile",
        },
        PathProbe {
            label: "URL(\"jar:file:...!/entry\").openStream()",
            java: "JarUrl",
        },
        PathProbe {
            label: "Files.readAllBytes(path)",
            java: "NioRead",
        },
        PathProbe {
            label: "Files.size(path)",
            java: "NioSize",
        },
        PathProbe {
            label: "File.length()",
            java: "FileLength",
        },
        PathProbe {
            label: "RandomAccessFile.readFully(byte[])",
            java: "RafRead",
        },
    ]
}

/// Run one probe and describe the outcome in one line.
fn run_probe(env: &mut jni::JNIEnv, hollow: &Path, probe: &PathProbe, _real_len: u64) -> String {
    let path = env
        .new_string(hollow.to_string_lossy().as_ref())
        .expect("new_string");
    let file = env
        .new_object(
            "java/io/File",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&path)],
        )
        .expect("new File");

    let entry_name = env
        .new_string("hello/HelloWorld.class")
        .expect("entry name");

    let result = match probe.java {
        "ZipFile" => read_zip_like(env, &file, &entry_name, "java/util/zip/ZipFile"),
        "JarFile" => read_zip_like(env, &file, &entry_name, "java/util/jar/JarFile"),
        "JarUrl" => read_jar_url(env, hollow, &entry_name),
        "NioRead" => nio_read(env, &path),
        "NioSize" => nio_size(env, &path).map(|n| format!("{n} bytes")),
        "FileLength" => env
            .call_method(&file, "length", "()J", &[])
            .ok()
            .and_then(|v| v.j().ok())
            .map(|n| format!("{n} bytes"))
            .ok_or_else(|| "exception".to_string()),
        "RafRead" => raf_read(env, &file),
        _ => Err("unknown probe".to_string()),
    };

    // A failed probe leaves an exception pending, which would poison every
    // later probe on this thread.
    if env.exception_check().unwrap_or(false) {
        env.exception_describe().ok();
        env.exception_clear().ok();
    }

    match result {
        Ok(summary) => format!("OK   {summary}"),
        Err(error) => format!("FAIL {error}"),
    }
}

fn read_zip_like(
    env: &mut jni::JNIEnv,
    file: &jni::objects::JObject,
    entry_name: &jni::objects::JString,
    class: &str,
) -> Result<String, String> {
    let zip = env
        .new_object(class, "(Ljava/io/File;)V", &[JValue::Object(file)])
        .map_err(|e| format!("ctor: {e}"))?;
    let entry = env
        .call_method(
            &zip,
            "getEntry",
            "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
            &[JValue::Object(entry_name)],
        )
        .map_err(|e| format!("getEntry: {e}"))?
        .l()
        .map_err(|e| format!("getEntry value: {e}"))?;
    if entry.is_null() {
        return Err("entry missing".into());
    }
    let stream = env
        .call_method(
            &zip,
            "getInputStream",
            "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;",
            &[JValue::Object(&entry)],
        )
        .map_err(|e| format!("getInputStream: {e}"))?
        .l()
        .map_err(|e| format!("stream value: {e}"))?;
    let array = env
        .call_method(&stream, "readAllBytes", "()[B", &[])
        .map_err(|e| format!("readAllBytes: {e}"))?
        .l()
        .map_err(|e| format!("array value: {e}"))?;
    let array = unsafe { jni::objects::JByteArray::from_raw(array.into_raw()) };
    let bytes = env
        .convert_byte_array(&array)
        .map_err(|e| format!("convert: {e}"))?;
    Ok(format!("{} bytes", bytes.len()))
}

fn read_jar_url(
    env: &mut jni::JNIEnv,
    hollow: &Path,
    entry_name: &jni::objects::JString,
) -> Result<String, String> {
    let url_text = format!(
        "jar:{}!/{}",
        url_of(hollow),
        env.get_string(entry_name)
            .map_err(|e| format!("{e}"))?
            .to_string_lossy()
    );
    let url_string = env.new_string(url_text).map_err(|e| format!("{e}"))?;
    let url = env
        .new_object(
            "java/net/URL",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&url_string)],
        )
        .map_err(|e| format!("new URL: {e}"))?;
    let stream = env
        .call_method(&url, "openStream", "()Ljava/io/InputStream;", &[])
        .map_err(|e| format!("openStream: {e}"))?
        .l()
        .map_err(|e| format!("stream value: {e}"))?;
    let array = env
        .call_method(&stream, "readAllBytes", "()[B", &[])
        .map_err(|e| format!("readAllBytes: {e}"))?
        .l()
        .map_err(|e| format!("array value: {e}"))?;
    let array = unsafe { jni::objects::JByteArray::from_raw(array.into_raw()) };
    let bytes = env
        .convert_byte_array(&array)
        .map_err(|e| format!("convert: {e}"))?;
    Ok(format!("{} bytes", bytes.len()))
}

/// A `file:` URL for a Windows path, with forward slashes.
fn url_of(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file:{text}")
    } else {
        format!("file:/{text}")
    }
}

fn nio_path<'local>(
    env: &mut jni::JNIEnv<'local>,
    path_string: &jni::objects::JString<'local>,
) -> Result<jni::objects::JObject<'local>, String> {
    let empty = env
        .new_object_array(0, "java/lang/String", jni::objects::JObject::null())
        .map_err(|e| format!("{e}"))?;
    env.call_static_method(
        "java/nio/file/Paths",
        "get",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        &[JValue::Object(path_string), JValue::Object(&empty)],
    )
    .map_err(|e| format!("Paths.get: {e}"))?
    .l()
    .map_err(|e| format!("path value: {e}"))
}

fn nio_read<'local>(
    env: &mut jni::JNIEnv<'local>,
    path_string: &jni::objects::JString<'local>,
) -> Result<String, String> {
    let path = nio_path(env, path_string)?;
    let array = env
        .call_static_method(
            "java/nio/file/Files",
            "readAllBytes",
            "(Ljava/nio/file/Path;)[B",
            &[JValue::Object(&path)],
        )
        .map_err(|e| format!("readAllBytes: {e}"))?
        .l()
        .map_err(|e| format!("array value: {e}"))?;
    let array = unsafe { jni::objects::JByteArray::from_raw(array.into_raw()) };
    let bytes = env
        .convert_byte_array(&array)
        .map_err(|e| format!("convert: {e}"))?;
    Ok(format!("{} bytes", bytes.len()))
}

fn nio_size<'local>(
    env: &mut jni::JNIEnv<'local>,
    path_string: &jni::objects::JString<'local>,
) -> Result<u64, String> {
    let path = nio_path(env, path_string)?;
    env.call_static_method(
        "java/nio/file/Files",
        "size",
        "(Ljava/nio/file/Path;)J",
        &[JValue::Object(&path)],
    )
    .map_err(|e| format!("size: {e}"))?
    .j()
    .map(|n| n as u64)
    .map_err(|e| format!("size value: {e}"))
}

fn raf_read(env: &mut jni::JNIEnv, file: &jni::objects::JObject) -> Result<String, String> {
    let mode = env.new_string("r").map_err(|e| format!("{e}"))?;
    let mode_obj: jni::objects::JObject = mode.into();
    let raf = env
        .new_object(
            "java/io/RandomAccessFile",
            "(Ljava/io/File;Ljava/lang/String;)V",
            &[JValue::Object(file), JValue::Object(&mode_obj)],
        )
        .map_err(|e| format!("ctor: {e}"))?;
    let len = env
        .call_method(&raf, "length", "()J", &[])
        .map_err(|e| format!("length: {e}"))?
        .j()
        .map_err(|e| format!("length value: {e}"))?;
    let buf = env
        .new_byte_array(64)
        .map_err(|e| format!("new array: {e}"))?;
    env.call_method(&raf, "readFully", "([B)V", &[JValue::Object(&buf)])
        .map_err(|e| format!("readFully: {e}"))?;
    Ok(format!("length {len}, read 64 bytes"))
}
