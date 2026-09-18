//! Authoritative V5 integration: official Minecraft -> intermediary in memory,
//! then load the renamed `af$2` class through Fabric's Knot class loader.
//!
//! This test intentionally uses the real 1.21.4 client, the real Fabric loader
//! jars, and the real game libraries. It is ignored by default because it
//! remaps roughly eight thousand classes and creates a JVM.
//!
//! Run with:
//!
//! ```text
//! cargo test --locked --test minecraft_official_to_intermediary -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jni::objects::{JClass, JObject, JValue};
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::HookSet;
use jvmsense_core::remap::{official_to_intermediary, TinyRemapRequest};

mod support;

fn example_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join(relative)
}

fn classpath_file() -> Vec<PathBuf> {
    let text =
        std::fs::read_to_string(example_path("minecraft/classpath.txt")).expect("read classpath");
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[test]
#[ignore = "remaps the real Minecraft client and initializes Fabric/Knot"]
fn official_client_remaps_in_memory_and_knot_loads_the_af_inner_class() {
    let source = example_path("minecraft/client.jar");
    let mappings = support::ensure_minecraft_intermediary_mapping()
        .expect("obtain official-to-intermediary mapping");
    let remapper = support::ensure_tiny_remapper().expect("obtain TinyRemapper");
    let jdk = support::test_jdk();
    let java = jdk.home().join("bin").join("java.exe");
    // Fabric Loader rejects a classpath containing two ASM copies. Its own
    // loader jars already supply ASM 9.7, so Minecraft's 9.6 jar is excluded.
    let classpath = classpath_file()
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_none_or(|name| !name.to_string_lossy().starts_with("asm-"))
        })
        .collect::<Vec<_>>();
    assert!(
        source.is_file(),
        "client fixture missing: {}",
        source.display()
    );

    let work = tempfile::tempdir().expect("tempdir");
    let scratch = work.path().join("remap-helper");
    let remapped = official_to_intermediary(&TinyRemapRequest {
        java: &java,
        remapper_jar: &remapper,
        mappings: &mappings,
        source_jar: &source,
        classpath: &classpath,
        scratch_dir: &scratch,
    })
    .expect("remap official client to intermediary");
    assert!(
        remapped.len() > 1_000_000,
        "the intermediary game image is implausibly small"
    );
    println!(
        "official client: {} bytes; intermediary image: {} bytes in memory",
        std::fs::metadata(&source).expect("stat client").len(),
        remapped.len()
    );
    assert!(
        !scratch.join("RemapToStdout.java").exists(),
        "the helper source must be removed after remapping"
    );

    {
        let names: Vec<String> = zip::ZipArchive::new(std::io::Cursor::new(&remapped))
            .expect("open intermediary image in memory")
            .file_names()
            .filter(|name| {
                name.contains("class_156") || name.starts_with("af$") || *name == "af.class"
            })
            .map(str::to_string)
            .collect();
        println!("renamed af entries: {names:?}");
    }
    let modmenu = support::ensure_modmenu().expect("obtain Mod Menu");
    let mod_bytes = std::fs::read(&modmenu).expect("read Mod Menu into memory");
    let mod_plan = jvmsense_core::runtime::parse_runtime_mod(&mod_bytes).expect("parse Mod Menu");
    assert_eq!(mod_plan.id, "modmenu");
    let mod_metadata = jvmsense_core::runtime::read_mod_entry(&mod_bytes, "fabric.mod.json")
        .expect("read fabric.mod.json");
    let nested_jars = mod_plan
        .nested_jars
        .iter()
        .map(|path| {
            (
                path.clone(),
                jvmsense_core::runtime::read_mod_entry(&mod_bytes, path).expect("read nested jar"),
            )
        })
        .collect::<Vec<_>>();
    let loader_jars = support::ensure_fabric_jars();
    assert!(!loader_jars.is_empty(), "Fabric loader jars are required");
    let run_dir = work.path().join("run");
    std::fs::create_dir_all(&run_dir).expect("create run directory");
    let app = jvmsense_core::launch::FabricApplication::mount_remapped(
        work.path().join("session"),
        "client-intermediary.jar",
        remapped,
        &[],
        loader_jars,
        classpath,
        Vec::new(),
    )
    .expect("mount intermediary game image");
    assert!(
        !app.layout
            .system_properties()
            .contains_key(jvmsense_core::launch::fabric::DEVELOPMENT_PROPERTY),
        "fabric.development must remain disabled for in-memory injection"
    );

    let game_path = app.layout.game_jar.path().to_path_buf();
    let intermediary_name = "net.minecraft.class_156$2";
    let (official_present, intermediary_present) = {
        let mounted = app
            .vfs
            .with_role(jvmsense_core::vfs::ArtifactRole::Game)
            .next()
            .expect("mounted intermediary game");
        let index = mounted.index().expect("index intermediary game");
        (
            index.class_entry("af$2").is_some(),
            index.class_entry(intermediary_name).is_some(),
        )
    };
    assert!(
        !official_present,
        "official af$2 must not remain in the game image"
    );
    assert!(
        intermediary_present,
        "intermediary {intermediary_name} must exist in the game image"
    );
    assert!(
        !game_path.exists(),
        "the intermediary game image must not be written to disk"
    );

    let classpath_string = app.layout.classpath_string();
    let mut builder = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={classpath_string}"))
        .option("-Xshare:off")
        .option(format!(
            "-Djava.library.path={}",
            example_path("minecraft/natives-extracted").display()
        ));
    for option in app.jvm_options() {
        builder = builder.option(option);
    }
    let args = builder.build().expect("JVM init args");
    let dll = jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(dll.clone())).expect("create JVM");
    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install read hooks");

    jvmsense_core::native::reset_trace();
    let vfs = Arc::new(app.vfs);
    let (
        loaded_name,
        runtime_mod_class,
        runtime_mod_path,
        entrypoint_reached_modmenu,
        entrypoint_completed,
        mixin_effect,
    ) = jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
        let mut env = jvm.attach_current_thread().expect("attach JVM thread");
        let client = env
            .get_static_field(
                "net/fabricmc/api/EnvType",
                "CLIENT",
                "Lnet/fabricmc/api/EnvType;",
            )
            .expect("EnvType.CLIENT")
            .l()
            .expect("EnvType object");
        let knot = env
            .new_object(
                "net/fabricmc/loader/impl/launch/knot/Knot",
                "(Lnet/fabricmc/api/EnvType;)V",
                &[JValue::Object(&client)],
            )
            .expect("construct Knot");
        let empty_args = env
            .new_object_array(0, "java/lang/String", JObject::null())
            .expect("empty Knot args");
        let class_loader = env
            .call_method(
                &knot,
                "init",
                "([Ljava/lang/String;)Ljava/lang/ClassLoader;",
                &[JValue::Object(&empty_args)],
            )
            .expect("initialize Knot")
            .l()
            .expect("Knot class loader");

        let mounted = vfs
            .mount_memory_runtime(
                jvmsense_core::vfs::ArtifactRole::Mod,
                "modmenu-runtime.jar",
                mod_bytes.clone(),
                true,
            )
            .expect("mount runtime mod");
        let runtime_mod_path = mounted.path().to_path_buf();
        jvmsense_core::runtime::jni::add_to_knot_classpath(&mut env, &knot, &runtime_mod_path)
            .expect("add runtime mod to Knot classpath");
        for (name, bytes) in &nested_jars {
            let nested = vfs
                .mount_memory_runtime(
                    jvmsense_core::vfs::ArtifactRole::Library,
                    name,
                    bytes.clone(),
                    true,
                )
                .expect("mount nested Fabric API jar");
            jvmsense_core::runtime::jni::add_to_knot_classpath(
                &mut env,
                &knot,
                &nested.path().to_path_buf(),
            )
            .expect("add nested jar to Knot classpath");
        }
        let nested_api_name = env
            .new_string("net.fabricmc.fabric.api.client.screen.v1.ScreenEvents")
            .expect("nested API class name");
        env.call_static_method(
            "java/lang/Class",
            "forName",
            "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
            &[
                JValue::Object(&nested_api_name),
                JValue::Bool(false.into()),
                JValue::Object(&class_loader),
            ],
        )
        .expect("load nested Fabric API class through Knot");
        let runtime_mod_name = env
            .new_string("com.terraformersmc.modmenu.ModMenu")
            .expect("runtime mod class name");
        let runtime_mod_class = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&runtime_mod_name),
                    JValue::Bool(false.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("load runtime mod class through Knot")
            .l()
            .expect("runtime mod class");
        let runtime_mod_class = env
            .call_method(&runtime_mod_class, "getName", "()Ljava/lang/String;", &[])
            .expect("read runtime mod class name")
            .l()
            .expect("runtime mod class name");
        let runtime_mod_class = {
            let name = env
                .get_string((&runtime_mod_class).into())
                .expect("decode runtime mod class name");
            name.to_string_lossy().into_owned()
        };

        jvmsense_core::runtime::jni::register_mod_candidate(
            &mut env,
            &runtime_mod_path,
            &mod_metadata,
            "modmenu",
        )
        .expect("register Mod Menu with FabricLoader");

        jvmsense_core::runtime::jni::register_mixin_config(&mut env, "mixins.modmenu.json")
            .expect("register Mod Menu Mixin config");
        jvmsense_core::runtime::jni::register_mixin_config(
            &mut env,
            "fabric-key-binding-api-v1.mixins.json",
        )
        .expect("register Fabric key-binding Mixin config");
        let target_name = env
            .new_string("net.minecraft.class_442")
            .expect("target class name");
        let target = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&target_name),
                    JValue::Bool(false.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("load Mixin target through Knot")
            .l()
            .expect("target class");
        let methods = env
            .call_method(
                &target,
                "getDeclaredMethods",
                "()[Ljava/lang/reflect/Method;",
                &[],
            )
            .expect("target methods")
            .l()
            .expect("method array");
        let methods = jni::objects::JObjectArray::from(methods);
        let method_count = env.get_array_length(&methods).expect("method count");
        let mut mixin_effect = false;
        for index in 0..method_count {
            let method = env
                .get_object_array_element(&methods, index)
                .expect("method element");
            let name = env
                .call_method(&method, "getName", "()Ljava/lang/String;", &[])
                .expect("method name")
                .l()
                .expect("method name value");
            let name = env
                .get_string((&name).into())
                .expect("decode method name")
                .to_string_lossy()
                .into_owned();
            if name.contains("adjustRealmsHeight") || name.contains("onRender") {
                mixin_effect = true;
                break;
            }
        }
        // Initialize and invoke the real Mod Menu client entrypoint. The
        // class has already been admitted to Knot above; this step now
        // exercises static initialization and the entrypoint call.
        let entrypoint_class_name = env
            .new_string("com.terraformersmc.modmenu.ModMenu")
            .expect("entrypoint class name");
        let entrypoint_class = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&entrypoint_class_name),
                    JValue::Bool(true.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("initialize runtime mod entrypoint class")
            .l()
            .expect("entrypoint class");
        // The test stops at Knot initialization rather than launching the
        // full client. Mod Menu's entrypoint only requires that
        // `MinecraftClient.getInstance()` be non-null before GameOptions is
        // initialized, so provide an uninitialized client shell for this
        // isolated entrypoint check.
        let client_name = env
            .new_string("net.minecraft.class_310")
            .expect("client class name");
        let client_class = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&client_name),
                    JValue::Bool(false.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("load client class")
            .l()
            .expect("client class");
        let client_class = unsafe { JClass::from_raw(client_class.as_raw()) };
        let client_shell = env
            .alloc_object(&client_class)
            .expect("allocate client shell");
        env.set_static_field(
            &client_class,
            (&client_class, "field_1700", "Lnet/minecraft/class_310;"),
            JValue::Object(&client_shell),
        )
        .expect("install client shell");
        let entrypoint_result = jvmsense_core::runtime::jni::invoke_no_arg_method(
            &mut env,
            &entrypoint_class,
            "onInitializeClient",
        );
        let (entrypoint_reached_modmenu, entrypoint_completed) = match entrypoint_result {
            Ok(()) => (true, true),
            Err(jni::errors::Error::JavaException) => {
                let throwable = env.exception_occurred().expect("entrypoint throwable");
                env.exception_clear().expect("clear entrypoint exception");
                let detail =
                    match env.call_method(&throwable, "toString", "()Ljava/lang/String;", &[]) {
                        Ok(value) => match value.l() {
                            Ok(text) => match env.get_string((&text).into()) {
                                Ok(text) => text.to_string_lossy().into_owned(),
                                Err(_) => "unprintable Java exception".to_string(),
                            },
                            Err(_) => "non-object Java exception".to_string(),
                        },
                        Err(_) => "unprintable Java exception".to_string(),
                    };
                eprintln!("Mod Menu entrypoint exception: {detail}");
                let _ = env.call_method(&throwable, "printStackTrace", "()V", &[]);
                let stack = env
                    .call_method(
                        &throwable,
                        "getStackTrace",
                        "()[Ljava/lang/StackTraceElement;",
                        &[],
                    )
                    .expect("entrypoint stack trace")
                    .l()
                    .expect("stack trace array");
                let stack = jni::objects::JObjectArray::from(stack);
                let len = env.get_array_length(&stack).expect("stack trace length");
                let mut found = false;
                for index in 0..len {
                    let frame = env
                        .get_object_array_element(&stack, index)
                        .expect("stack frame");
                    let class_name = env
                        .call_method(&frame, "getClassName", "()Ljava/lang/String;", &[])
                        .expect("stack frame class")
                        .l()
                        .expect("class name");
                    let class_name = env
                        .get_string((&class_name).into())
                        .expect("decode frame class")
                        .to_string_lossy()
                        .into_owned();
                    if class_name == "com.terraformersmc.modmenu.ModMenu" {
                        found = true;
                        break;
                    }
                }
                (found, false)
            }
            Err(error) => panic!("entrypoint invocation failed before Java: {error}"),
        };

        let class_name = env
            .new_string(intermediary_name)
            .expect("intermediary class name");
        let loaded = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&class_name),
                    JValue::Bool(true.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("load class through Knot")
            .l()
            .expect("Class value");
        let loaded = env
            .call_method(&loaded, "getName", "()Ljava/lang/String;", &[])
            .expect("read loaded class name")
            .l()
            .expect("class name value");
        let name = env.get_string((&loaded).into()).expect("decode class name");
        (
            name.to_string_lossy().into_owned(),
            runtime_mod_class,
            runtime_mod_path.to_path_buf(),
            entrypoint_reached_modmenu,
            entrypoint_completed,
            mixin_effect,
        )
    });

    let trace = jvmsense_core::native::trace_snapshot();
    println!("Knot loaded: {loaded_name}");
    println!(
        "trace: hits={}, misses={}, entries={:?}",
        trace.hits, trace.misses, trace.entries
    );
    assert_eq!(loaded_name, intermediary_name);
    assert_eq!(
        runtime_mod_class, "com.terraformersmc.modmenu.ModMenu",
        "the runtime mod jar must be visible through Knot"
    );
    assert!(
        !runtime_mod_path.exists(),
        "the runtime mod must not be written to disk"
    );
    assert!(
        entrypoint_reached_modmenu,
        "Mod Menu's client entrypoint must execute through the runtime class loader"
    );
    assert!(
        entrypoint_completed,
        "Mod Menu's client entrypoint must return successfully"
    );
    assert!(
        mixin_effect,
        "at least one Mod Menu Mixin must affect the loaded target class"
    );
    assert!(
        trace.hits > 0,
        "Knot must have read the intermediary game through hollow VFS"
    );
    assert!(
        !game_path.exists(),
        "the intermediary game image must have no file on disk after launch"
    );

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}
