//! Shared test support: locate (and if necessary provision) a JDK.
//!
//! The tests no longer borrow a JDK from another project on this machine. They
//! provision IBM Semeru 25 through [`jvmsense_core::jdk`], which downloads and
//! verifies it once into the user cache and reuses it afterwards. Setting
//! `JVMSENSE_TEST_JRE` overrides that with an explicit home, which is useful
//! offline and for pinning a different build during a spike.

#![cfg(windows)]
#![allow(dead_code)]

use std::path::PathBuf;

use jvmsense_core::jdk::JdkProvisioner;

/// A JDK to drive JVM tests with.
///
/// Panics rather than returning an error: every caller needs one, and a test
/// that cannot get a JDK has nothing useful to say.
pub fn test_jdk() -> jvmsense_core::jdk::Jdk {
    if let Ok(explicit) = std::env::var("JVMSENSE_TEST_JRE") {
        let home = PathBuf::from(explicit);
        let jdk = jvmsense_core::jdk::Jdk::from_home(home);
        if jdk.is_complete() {
            return jdk;
        }
        panic!(
            "JVMSENSE_TEST_JRE={} is not a usable JDK (no bin/server/jvm.dll)",
            jdk.home().display()
        );
    }

    let provisioner =
        JdkProvisioner::with_default_cache().expect("a cache location for the provisioned JDK");
    eprintln!(
        "provisioning IBM Semeru from {} (first run downloads ~250 MB)",
        provisioner.install_dir().display()
    );
    provisioner.ensure().expect(
        "provision IBM Semeru 25; set JVMSENSE_TEST_JRE to a local JDK to avoid the download",
    )
}

/// Build the HelloWorld fixture jar if it is missing.
///
/// Kept in the harness rather than only in the docs so a fresh checkout can run
/// the cloud-loading test without reading the instructions first.
pub fn ensure_helloworld_jar(jdk: &jvmsense_core::jdk::Jdk) -> PathBuf {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let example_dir = manifest_dir
        .join("..")
        .join("..")
        .join("examples")
        .join("helloworld");
    let jar = example_dir.join("out").join("helloworld.jar");

    if jar.is_file() {
        return jar;
    }

    let src = example_dir.join("src");
    let classes = example_dir.join("build");
    let out = example_dir.join("out");
    std::fs::create_dir_all(&classes).expect("create build dir");
    std::fs::create_dir_all(&out).expect("create out dir");

    let sources: Vec<PathBuf> = walk_java(&src);
    assert!(
        !sources.is_empty(),
        "no Java sources under {}",
        src.display()
    );

    // Compile against the provisioned JDK so the class file version always
    // matches the runtime that will load it.
    let javac = jdk.home().join("bin").join("javac.exe");
    let mut cmd = std::process::Command::new(&javac);
    cmd.arg("-d").arg(&classes);
    for source in &sources {
        cmd.arg(source);
    }
    let status = cmd.status().expect("run javac");
    assert!(status.success(), "javac failed");

    // The resource has to sit beside the classes for `getResourceAsStream` to
    // find it on the classpath.
    let resource = src.join("greeting.properties");
    if resource.is_file() {
        std::fs::copy(&resource, classes.join("greeting.properties")).expect("copy resource");
    }

    let jar_tool = jdk.home().join("bin").join("jar.exe");
    let status = std::process::Command::new(&jar_tool)
        .arg("--create")
        .arg("--file")
        .arg(&jar)
        .arg("--main-class")
        .arg("hello.HelloWorld")
        .arg("-C")
        .arg(&classes)
        .arg(".")
        .status()
        .expect("run jar");
    assert!(status.success(), "jar packaging failed");

    jar
}

/// The Fabric jars a launch test needs, downloaded and verified on demand.
///
/// The jars are large and are not committed, so the harness fetches them the
/// first time a test needs them. Each is verified by opening it as a zip: a
/// truncated download still looks like a file of the right rough size, and the
/// failure it causes much later ("zip END header not found" from the JVM) does
/// not point back at the download.
///
/// Returns the jar paths, sorted, or an empty vector if the network is
/// unavailable — a caller should treat that as "cannot run this test".
pub fn ensure_fabric_jars() -> Vec<PathBuf> {
    const JARS: &[(&str, &str)] = &[
        (
            "fabric-loader-0.18.4.jar",
            "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.18.4/fabric-loader-0.18.4.jar",
        ),
        (
            "asm-9.9.jar",
            "https://repo.maven.apache.org/maven2/org/ow2/asm/asm/9.9/asm-9.9.jar",
        ),
        (
            "asm-analysis-9.9.jar",
            "https://repo.maven.apache.org/maven2/org/ow2/asm/asm-analysis/9.9/asm-analysis-9.9.jar",
        ),
        (
            "asm-commons-9.9.jar",
            "https://repo.maven.apache.org/maven2/org/ow2/asm/asm-commons/9.9/asm-commons-9.9.jar",
        ),
        (
            "asm-tree-9.9.jar",
            "https://repo.maven.apache.org/maven2/org/ow2/asm/asm-tree/9.9/asm-tree-9.9.jar",
        ),
        (
            "asm-util-9.9.jar",
            "https://repo.maven.apache.org/maven2/org/ow2/asm/asm-util/9.9/asm-util-9.9.jar",
        ),
        (
            "sponge-mixin-0.17.0+mixin.0.8.7.jar",
            "https://repo.maven.apache.org/maven2/net/fabricmc/sponge-mixin/0.17.0+mixin.0.8.7/sponge-mixin-0.17.0+mixin.0.8.7.jar",
        ),
    ];

    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib = manifest_dir
        .join("..")
        .join("..")
        .join("examples")
        .join("fabric")
        .join("lib");
    if std::fs::create_dir_all(&lib).is_err() {
        return Vec::new();
    }

    let mut paths = Vec::new();
    for (name, url) in JARS {
        let path = lib.join(name);
        if !is_valid_zip(&path) && (!download(url, &path) || !is_valid_zip(&path)) {
            eprintln!("jvmsense tests: cannot obtain {name} from {url}");
            return Vec::new();
        }
        // `dunce` strips the `\?\` prefix `std::fs::canonicalize` adds; the
        // JVM cannot open a classpath entry carrying it.
        paths.push(dunce::canonicalize(&path).unwrap_or(path));
    }
    paths.sort();
    paths
}

/// Obtain the pinned TinyRemapper fat jar used by the offline remap tests.
pub fn ensure_tiny_remapper() -> Option<PathBuf> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("minecraft")
        .join("tools")
        .join("tiny-remapper-0.14.1-fat.jar");
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }

    let url =
        "https://maven.fabricmc.net/net/fabricmc/tiny-remapper/0.14.1/tiny-remapper-0.14.1-fat.jar";
    if !is_valid_zip(&path) && (!download(url, &path) || !is_valid_zip(&path)) {
        eprintln!("jvmsense tests: cannot obtain TinyRemapper from {url}");
        return None;
    }
    Some(dunce::canonicalize(&path).unwrap_or(path))
}

/// Obtain and extract the Minecraft 1.21.4 official-to-intermediary mappings.
pub fn ensure_yarn_named_mapping() -> Option<PathBuf> {
    let tools = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("minecraft")
        .join("tools");
    std::fs::create_dir_all(&tools).ok()?;

    let mapping = tools.join("yarn-1.21.4.tiny");
    let expected_header = "tiny\t2\t0\tintermediary\tnamed";
    if let Ok(text) = std::fs::read_to_string(&mapping) {
        if text.starts_with(expected_header) {
            return Some(dunce::canonicalize(&mapping).unwrap_or(mapping));
        }
    }

    let url = "https://maven.fabricmc.net/net/fabricmc/yarn/1.21.4%2Bbuild.8/yarn-1.21.4%2Bbuild.8-v2.jar";
    let archive = tools.join("yarn-1.21.4-v2.jar");
    if (!archive.is_file() || !is_valid_zip(&archive))
        && (!download(url, &archive) || !is_valid_zip(&archive))
    {
        eprintln!("jvmsense tests: cannot obtain Yarn from {url}");
        return None;
    }

    let file = std::fs::File::open(&archive).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entry = zip.by_name("mappings/mappings.tiny").ok()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut entry, &mut text).ok()?;
    drop(entry);

    if !text.starts_with(expected_header) {
        eprintln!("jvmsense tests: Yarn mapping has an unexpected header");
        return None;
    }
    std::fs::write(&mapping, text).ok()?;
    std::fs::remove_file(&archive).ok();
    Some(dunce::canonicalize(&mapping).unwrap_or(mapping))
}

/// Obtain a real intermediary Mixin mod with a refmap.
pub fn ensure_modmenu() -> Option<PathBuf> {
    let tools = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("minecraft")
        .join("tools");
    std::fs::create_dir_all(&tools).ok()?;

    let path = tools.join("modmenu-13.0.4.jar");
    let url = "https://cdn.modrinth.com/data/mOgUt4GM/versions/qEKKsTqd/modmenu-13.0.4.jar";
    if !is_valid_zip(&path) && (!download(url, &path) || !is_valid_zip(&path)) {
        eprintln!("jvmsense tests: cannot obtain Mod Menu from {url}");
        return None;
    }
    Some(dunce::canonicalize(&path).unwrap_or(path))
}

pub fn ensure_minecraft_intermediary_mapping() -> Option<PathBuf> {
    let tools = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("minecraft")
        .join("tools");
    std::fs::create_dir_all(&tools).ok()?;

    let mapping = tools.join("intermediary-1.21.4.tiny");
    if let Ok(text) = std::fs::read_to_string(&mapping) {
        if text.starts_with("tiny\t2\t0\tofficial\tintermediary") {
            return Some(dunce::canonicalize(&mapping).unwrap_or(mapping));
        }
    }

    let archive = tools.join("intermediary-1.21.4-v2.jar");
    let url =
        "https://maven.fabricmc.net/net/fabricmc/intermediary/1.21.4/intermediary-1.21.4-v2.jar";
    if (!archive.is_file() || !is_valid_zip(&archive))
        && (!download(url, &archive) || !is_valid_zip(&archive))
    {
        eprintln!("jvmsense tests: cannot obtain intermediary mappings from {url}");
        return None;
    }

    let file = std::fs::File::open(&archive).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entry = zip.by_name("mappings/mappings.tiny").ok()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut entry, &mut text).ok()?;
    drop(entry);

    if !text.starts_with("tiny\t2\t0\tofficial\tintermediary") {
        eprintln!("jvmsense tests: intermediary mapping has an unexpected header");
        return None;
    }
    std::fs::write(&mapping, text).ok()?;
    std::fs::remove_file(&archive).ok();
    Some(dunce::canonicalize(&mapping).unwrap_or(mapping))
}
/// True when `path` is a zip whose central directory is intact.
fn is_valid_zip(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // Opening succeeds only if the central directory can be read, which a
    // truncated download fails.
    zip::ZipArchive::new(file).is_ok()
}

/// Fetch `url` to `path`. Returns false on any failure.
fn download(url: &str, path: &std::path::Path) -> bool {
    // A small Rust HTTP client is out of scope for the test harness, so this
    // shells out to whichever fetch tool is present rather than embedding one.
    for (program, args) in [
        ("curl", vec!["-sL", "--max-time", "300", "-o"]),
        ("wget", vec!["-q", "-O"]),
    ] {
        let mut command = std::process::Command::new(program);
        command.args(&args);
        command.arg(path);
        command.arg(url);
        if let Ok(status) = command.status() {
            if status.success() {
                return true;
            }
        }
    }
    false
}

/// Every `.java` file under `dir`, recursively.
fn walk_java(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_java(&path));
        } else if path.extension().is_some_and(|e| e == "java") {
            out.push(path);
        }
    }
    out
}
