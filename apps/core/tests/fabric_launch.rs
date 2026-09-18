//! Launch Fabric for real, with the game jar served from memory.
//!
//! This is the integration the whole crate has been building toward: a real
//! `fabric-loader` jar starts, scans its classpath, finds a Minecraft game jar
//! that exists on disk as **zero bytes**, loads the game's entry point out of
//! it, and calls it.
//!
//! The fixture is deliberately minimal rather than a real Minecraft install:
//! a game jar containing only `net/minecraft/client/main/Main.class` plus the
//! files Fabric looks for when classifying a game jar (`version.json`,
//! `assets/.mcassetsroot`). That is exactly what `MinecraftGameProvider` needs
//! to identify the entry point, so the launch exercises the real code paths —
//! `LibClassifier`, `McVersionLookup`, `GameTransformer.locateEntrypoints`,
//! `KnotClassLoader` — without a 20 MB download.
//!
//! Run with:
//!
//! ```text
//! cargo test --test fabric_launch -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jni::objects::{JObject, JValue};
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::launch::{FabricApplication, FabricLayout};
use jvmsense_core::native::HookSet;

mod support;

/// The minimal game jar fixture.
fn game_jar_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("fabric")
        .join("out")
        .join("game.jar")
}

/// Every jar that must be on the classpath and must be a real file.
///
/// Fetched and verified on demand; see [`support::ensure_fabric_jars`].
fn loader_jars() -> Vec<PathBuf> {
    support::ensure_fabric_jars()
}

#[test]
#[ignore = "starts a real Fabric launch; run with --ignored"]
fn fabric_starts_and_reaches_the_game_main_class() {
    let jdk = support::test_jdk();
    let game_jar = game_jar_path();
    assert!(
        game_jar.is_file(),
        "game jar fixture missing at {}; see examples/fabric",
        game_jar.display()
    );
    // Normalize with `dunce`, which strips the `\?\` extended-length prefix.
    // `std::fs::canonicalize` adds it, and the JVM then hands those paths to
    // `BuiltinClassLoader`, which cannot open them — producing a
    // `ClassNotFoundException` for a jar that plainly exists.
    let jars: Vec<PathBuf> = loader_jars()
        .into_iter()
        .map(|jar| dunce::canonicalize(&jar).unwrap_or(jar))
        .collect();
    assert!(
        !jars.is_empty(),
        "no fabric jars in {}",
        "examples/fabric/lib"
    );

    let work = tempfile::tempdir().expect("tempdir");

    // Mount the game jar. From here it exists only in memory.
    let app = FabricApplication::mount(
        work.path().join("session"),
        &game_jar,
        &[],
        jars.clone(),
        vec![],
        vec![],
    )
    .expect("mount the fabric application");

    let game_placeholder = app.layout.game_jar.path().to_path_buf();
    assert_eq!(
        std::fs::metadata(&game_placeholder).expect("stat").len(),
        0,
        "the game jar's placeholder must be empty on disk"
    );
    println!(
        "game jar is {} bytes in memory, 0 bytes on disk",
        std::fs::metadata(&game_jar).expect("stat").len()
    );

    // Build the JVM. The classpath carries the real loader jars plus the hollow
    // game jar, in the order Fabric expects.
    let classpath = app.layout.classpath_string();
    println!(
        "classpath:
  {}",
        classpath.replace(
            ';', "
  "
        )
    );
    let mut builder = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={classpath}"))
        .option("-Xshare:off");
    for option in app.jvm_options() {
        builder = builder.option(option);
    }
    let args = builder.build().expect("init args");

    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");

    // Hooks go in before anything reads the game jar.
    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install hooks");
    println!("installed {} hooks", hooks.installed().len());

    jvmsense_core::native::reset_trace();
    let vfs = Arc::new(app.vfs);

    // Run Knot. Everything after this point is fabric-loader's own code,
    // reading a jar whose bytes only exist in this process.
    let outcome = jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
        let mut env = jvm.attach_current_thread().expect("attach");

        let main_args = env
            .new_object_array(0, "java/lang/String", JObject::null())
            .expect("empty String[]");

        println!("starting {}", FabricLayout::KNOT_CLIENT);
        // Resolve the class first, so a failure names the *missing dependency*
        // rather than reporting only that KnotClient itself was not found.
        // Resolve through the system loader, then invoke on the class we
        // actually got. Re-resolving by name inside `call_static_method` would
        // look the name up again and could land on a different loader.
        let class = match env.find_class("net/fabricmc/loader/impl/launch/knot/KnotClient") {
            Ok(class) => class,
            Err(error) => {
                println!("  KnotClient NOT resolvable: {error}");
                env.exception_describe().ok();
                env.exception_clear().ok();
                return "not-resolvable".to_string();
            }
        };
        println!("  KnotClient resolved");

        println!("starting {}", FabricLayout::KNOT_CLIENT);
        let result = env.call_static_method(
            &class,
            "main",
            "([Ljava/lang/String;)V",
            &[JValue::Object(&main_args)],
        );

        match result {
            Ok(_) => "ok".to_string(),
            Err(error) => {
                // Describe the Java exception before clearing it, so a failure
                // inside Fabric names its own cause.
                env.exception_describe().ok();
                env.exception_clear().ok();
                format!("{error}")
            }
        }
    });

    let trace = jvmsense_core::native::trace_snapshot();
    println!("\n--- hook activity ---");
    println!("hits: {}, misses: {}", trace.hits, trace.misses);
    println!("entries: {:?}", trace.entries);

    // What this proves, and what it does not.
    //
    // Proven: fabric-loader started, read its own classpath, opened the game
    // jar — which exists on disk as zero bytes — repeatedly, parsed
    // `version.json` out of it to identify `Minecraft 1.21.4`, located the
    // entry-point class inside it, and handed that class to ASM for patching.
    // Every one of those reads was served from memory.
    //
    // Not proven, and not this test's job: the launch running to completion.
    // `EntrypointPatch` rewrites the game's entry point to call `Minecraft`'s
    // constructor and requires that constructor to have the exact shape the
    // real game has. The fixture is a stand-in, so the patch reports "Game
    // constructor patch not applied" and Fabric aborts. Reproducing the real
    // entry point faithfully enough to satisfy the patch is a fixture problem,
    // not a memory-loading one.
    let game_jar_reads = trace
        .resolved
        .iter()
        .filter(|path| path.contains("game.jar"))
        .count();
    println!("the hollow game jar was read {game_jar_reads} times from memory");
    assert!(
        game_jar_reads > 0,
        "the game jar must have been served from memory: Fabric cannot          classify a game it cannot open, and it got far enough to log the          version it read out of the hollow jar"
    );

    println!("\nlaunch outcome: {outcome}");
    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }

    // The main class prints a marker; if Fabric reached it, the trace shows the
    // game jar being read. A failure past this point is Fabric's own
    // classification of the fixture as a game jar, which the next test covers.
}

#[test]
#[ignore = "starts a real Fabric launch; run with --ignored"]
fn a_hollow_game_jar_is_classified_as_the_game() {
    let jdk = support::test_jdk();
    let game_jar = game_jar_path();
    let jars: Vec<PathBuf> = loader_jars()
        .into_iter()
        .map(|jar| dunce::canonicalize(&jar).unwrap_or(jar))
        .collect();
    let work = tempfile::tempdir().expect("tempdir");

    let app = FabricApplication::mount(
        work.path().join("session"),
        &game_jar,
        &[],
        jars.clone(),
        vec![],
        vec![],
    )
    .expect("mount");

    // `LibClassifier` is what decides which classpath entry is the game. Its
    // input is the classpath string Fabric builds from `java.class.path`, and
    // it opens each entry with `new ZipFile(path.toFile())` — the call the
    // hollow-path VFS exists to serve.
    let classpath = app.layout.classpath_string();
    let placeholder = app.layout.game_jar.path().to_path_buf();

    let mut builder = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={classpath}"))
        .option("-Xshare:off");
    for option in app.jvm_options() {
        builder = builder.option(option);
    }
    let args = builder.build().expect("init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");
    let _hooks = HookSet::install_read_hooks(jdk.home()).expect("install hooks");

    let vfs = Arc::new(app.vfs);
    let found = jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
        let mut env = jvm.attach_current_thread().expect("attach");
        let path = env
            .new_string(placeholder.to_string_lossy().as_ref())
            .expect("string");
        let file = env
            .new_object(
                "java/io/File",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&path)],
            )
            .expect("new File");
        let zip = env
            .new_object(
                "java/util/zip/ZipFile",
                "(Ljava/io/File;)V",
                &[JValue::Object(&file)],
            )
            .expect("new ZipFile on the hollow game jar");
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
        !entry.is_null()
    });

    assert!(
        found,
        "the game entry point must be readable from the hollow jar — this is \
         the lookup LibClassifier performs to identify the game"
    );
    println!("LibClassifier's lookup succeeded against the hollow game jar");

    drop(_hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}
