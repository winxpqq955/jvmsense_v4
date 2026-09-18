//! Real-JVM proof that the JVMTI bridge can redefine a loaded class.
//!
//! This isolates the primitive runtime Fabric injection depends on: changing
//! an already-loaded method body without a disk-writing agent.

#![cfg(windows)]

use std::path::{Path, PathBuf};

use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::jvmti::{ClassRedefinition, Jvmti};

mod support;

fn compile_variant(javac: &Path, root: &Path, value: i32) -> PathBuf {
    let source_dir = root.join("src");
    let classes = root.join("classes");
    let source = source_dir
        .join("jvmsense")
        .join("test")
        .join("RedefineTarget.java");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("mkdir source");
    std::fs::create_dir_all(&classes).expect("mkdir classes");
    std::fs::write(
        &source,
        format!(
            "package jvmsense.test;\npublic final class RedefineTarget {{\n    public static int value() {{ return {value}; }}\n}}\n"
        ),
    )
    .expect("write source");
    let status = std::process::Command::new(javac)
        .arg("-d")
        .arg(&classes)
        .arg(&source)
        .status()
        .expect("run javac");
    assert!(status.success(), "javac failed for variant {value}");
    classes
        .join("jvmsense")
        .join("test")
        .join("RedefineTarget.class")
}

#[test]
#[ignore = "creates a real JVM and uses JVMTI"]
fn a_loaded_class_can_be_redefined_from_jvmti() {
    let jdk = support::test_jdk();
    let work = tempfile::tempdir().expect("tempdir");
    let v1 = compile_variant(
        &jdk.home().join("bin").join("javac.exe"),
        &work.path().join("v1"),
        1,
    );
    let v2 = compile_variant(
        &jdk.home().join("bin").join("javac.exe"),
        &work.path().join("v2"),
        2,
    );
    // v1 is <root>/classes/jvmsense/test/RedefineTarget.class.
    let classes = v1
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("classes directory");
    let args = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={}", classes.display()))
        .option("-Xshare:off")
        .build()
        .expect("init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");
    let mut env = jvm.attach_current_thread().expect("attach");
    let class = env
        .find_class("jvmsense/test/RedefineTarget")
        .expect("find class");
    let before = env
        .call_static_method(&class, "value", "()I", &[])
        .expect("call before")
        .i()
        .expect("int before");
    assert_eq!(before, 1);

    let replacement = std::fs::read(&v2).expect("read replacement class");
    let jvmti = unsafe { Jvmti::from_vm(&jvm) }.expect("JVMTI environment");
    jvmti.enable_class_redefinition().expect("enable redefine");
    jvmti
        .redefine_classes(&[ClassRedefinition {
            class: &class,
            bytes: &replacement,
        }])
        .expect("redefine class");

    let after = env
        .call_static_method(&class, "value", "()I", &[])
        .expect("call after")
        .i()
        .expect("int after");
    assert_eq!(after, 2);

    drop(env);
    unsafe {
        let _ = jvm.destroy();
    }
}
