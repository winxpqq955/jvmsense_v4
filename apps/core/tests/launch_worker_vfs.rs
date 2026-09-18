//! Regression test for the VFS scope used by the production launch path.
//!
//! Run with:
//!
//! ```text
//! cargo test --test launch_worker_vfs -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use jvmsense_core::launch::{run, ClasspathEntry, LaunchRequest};
use jvmsense_core::native::{reset_trace, trace_snapshot};
use jvmsense_core::vfs::{identity_spec, ArtifactRole, VirtualFileSystem};

mod support;

#[test]
#[ignore = "creates a real JVM; run with --ignored"]
fn launch_run_makes_virtual_paths_visible_to_java_workers() {
    let jdk = support::test_jdk();
    let work = tempfile::tempdir().expect("tempdir");
    let source_jar = build_worker_jar(&jdk, work.path());

    let mut vfs = VirtualFileSystem::create(work.path().join("session")).expect("create VFS");
    vfs.mount(
        ArtifactRole::Game,
        "worker.jar",
        &identity_spec("worker.jar", source_jar).expect("artifact spec"),
        true,
    )
    .expect("mount worker jar");
    let virtual_path = vfs
        .with_role(ArtifactRole::Game)
        .next()
        .expect("mounted worker")
        .path()
        .to_path_buf();
    assert!(
        !virtual_path.exists(),
        "the worker jar must not have a materialized file"
    );

    reset_trace();
    let outcome = run(
        &LaunchRequest {
            jdk,
            main_class: "WorkerMain".to_string(),
            arguments: vec![virtual_path.to_string_lossy().into_owned()],
            classpath: vec![ClasspathEntry::Hollow(virtual_path)],
            system_properties: Vec::new(),
            working_directory: work.path().to_path_buf(),
        },
        vfs,
    )
    .expect("launch worker jar");

    assert!(outcome.is_success(), "worker launch failed: {outcome:?}");
    let trace = trace_snapshot();
    for label in [
        "niofs.createFile0",
        "nio.read0",
        "fis.open0",
        "fis.readBytes",
    ] {
        assert!(
            trace.entries.contains_key(label),
            "worker did not use {label}: {:?}",
            trace.entries
        );
    }
}

fn build_worker_jar(jdk: &jvmsense_core::jdk::Jdk, work: &Path) -> PathBuf {
    let source = work.join("WorkerMain.java");
    std::fs::write(
        &source,
        r#"
import java.io.FileInputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;

public final class WorkerMain {
    public static void main(String[] args) throws Exception {
        Path artifact = Path.of(args[0]);
        byte[][] reads = new byte[2][];
        Throwable[] failure = new Throwable[1];
        Thread worker = new Thread(() -> {
            try {
                reads[0] = Files.readAllBytes(artifact);
                try (FileInputStream stream = new FileInputStream(artifact.toFile())) {
                    reads[1] = stream.readAllBytes();
                }
            } catch (Throwable error) {
                failure[0] = error;
            }
        }, "jvmsense-vfs-worker");
        worker.start();
        worker.join();
        if (failure[0] != null) {
            throw new Exception("virtual artifact read failed on worker", failure[0]);
        }
        if (reads[0].length == 0 || !Arrays.equals(reads[0], reads[1])) {
            throw new IllegalStateException("worker reads disagreed");
        }
    }
}
"#,
    )
    .expect("write worker source");

    let classes = work.join("classes");
    std::fs::create_dir_all(&classes).expect("create class directory");
    let status = Command::new(jdk.home().join("bin").join("javac.exe"))
        .arg("-d")
        .arg(&classes)
        .arg(&source)
        .status()
        .expect("run javac");
    assert!(status.success(), "javac failed");

    let jar = work.join("worker.jar");
    let class = std::fs::read(classes.join("WorkerMain.class")).expect("read worker class");
    let file = std::fs::File::create(&jar).expect("create worker jar");
    let mut writer = zip::ZipWriter::new(file);
    writer
        .start_file("WorkerMain.class", zip::write::FileOptions::<()>::default())
        .expect("start class entry");
    writer.write_all(&class).expect("write class entry");
    writer.finish().expect("finish worker jar");
    jar
}
