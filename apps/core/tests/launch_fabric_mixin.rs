//! Real-JVM proof that launch-time mods are discovered by Fabric and processed
//! by Mixin before the target class is loaded.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jni::objects::{JObject, JValue};
use jni::{InitArgsBuilder, JNIVersion, JavaVM};
use jvmsense_core::launch::{FabricApplication, FabricModImage};
use jvmsense_core::native::HookSet;
use jvmsense_core::remap::{official_to_intermediary, TinyRemapRequest};
use jvmsense_core::vfs::ArtifactRole;

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
        .filter_map(|line| {
            let path = PathBuf::from(line);
            let file = path.file_name()?.to_string_lossy();
            // Fabric Loader rejects duplicate ASM versions. Its loader jars
            // already carry the newer ASM version required by Mod Menu.
            (!file.starts_with("asm-")).then_some(path)
        })
        .collect()
}

fn class_has_mixin_method(env: &mut jni::JNIEnv<'_>, class: &JObject<'_>) -> bool {
    let methods = env
        .call_method(
            class,
            "getDeclaredMethods",
            "()[Ljava/lang/reflect/Method;",
            &[],
        )
        .expect("target methods")
        .l()
        .expect("method array");
    let methods = jni::objects::JObjectArray::from(methods);
    let count = env.get_array_length(&methods).expect("method count");
    for index in 0..count {
        let method = env
            .get_object_array_element(&methods, index)
            .expect("method element");
        let name = env
            .call_method(&method, "getName", "()Ljava/lang/String;", &[])
            .expect("method name")
            .l()
            .expect("method name value");
        let name = env.get_string((&name).into()).expect("decode method name");
        let name = name.to_string_lossy();
        if name.contains("adjustRealmsHeight") || name.contains("onRender") {
            return true;
        }
    }
    false
}

#[test]
#[ignore = "remaps Minecraft and proves launch-time Fabric/Mixin application without JVMTI"]
fn modmenu_is_mixed_in_at_target_class_load_time() {
    let source = example_path("minecraft/client.jar");
    let mappings = support::ensure_minecraft_intermediary_mapping().expect("mappings");
    let remapper = support::ensure_tiny_remapper().expect("TinyRemapper");
    let jdk = support::test_jdk();
    let java = jdk.home().join("bin").join("java.exe");
    let classpath = classpath_file();
    let work = tempfile::tempdir().expect("tempdir");

    let remapped = official_to_intermediary(&TinyRemapRequest {
        java: &java,
        remapper_jar: &remapper,
        mappings: &mappings,
        source_jar: &source,
        classpath: &classpath,
        scratch_dir: &work.path().join("game-remap-helper"),
    })
    .expect("remap game");

    let all_loader_jars = support::ensure_fabric_jars();
    assert!(
        !all_loader_jars.is_empty(),
        "Fabric loader jars are required"
    );
    let mut loader_image = None;
    let mut loader_dependencies = Vec::new();
    for jar in all_loader_jars {
        let is_loader = jar
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("fabric-loader-"));
        if is_loader {
            let bytes = std::fs::read(&jar).expect("read Fabric Loader");
            loader_image = Some(FabricModImage::from_memory(
                "fabric-loader-0.18.4.jar",
                bytes,
            ));
        } else {
            loader_dependencies.push(jar);
        }
    }
    let loader_image = loader_image.expect("Fabric Loader image");
    let mod_bytes = support::ensure_modmenu_without_placeholder_api().expect("Mod Menu");

    let app = FabricApplication::mount_remapped_launch_images(
        work.path().join("session"),
        "client-intermediary.jar",
        remapped,
        jvmsense_core::launch::FabricLaunchImages {
            loaders: vec![loader_image],
            mods: vec![FabricModImage::from_memory("modmenu-13.0.4.jar", mod_bytes)],
        },
        loader_dependencies,
        classpath,
        Vec::new(),
    )
    .expect("mount Fabric Loader and Mod Menu before JVM creation");

    assert!(
        app.layout
            .loader_jars
            .iter()
            .any(jvmsense_core::launch::ClasspathEntry::is_hollow),
        "Fabric Loader itself must be memory-backed"
    );
    assert!(
        !app.vfs
            .with_role(ArtifactRole::Mod)
            .any(|mounted| mounted.file_name().starts_with("placeholder-api-")),
        "the optional Placeholder API bundle must not be mounted for this focused fixture"
    );
    assert!(
        app.layout.mods.len() > 2,
        "MixinExtras, Mod Menu, and its nested modules must all be explicit launch-time mods"
    );
    assert!(
        !app.layout
            .system_properties()
            .contains_key(jvmsense_core::launch::fabric::DEVELOPMENT_PROPERTY),
        "fabric.development must remain disabled"
    );
    let properties = app.layout.system_properties();
    let add_mods = properties
        .get(jvmsense_core::launch::fabric::ADD_MODS_PROPERTY)
        .expect("fabric.addMods");
    let classpath_string = app.layout.classpath_string();
    for entry in &app.layout.mods {
        let path = entry.path().to_string_lossy();
        assert!(
            add_mods.contains(path.as_ref()),
            "missing from addMods: {path}"
        );
        assert!(
            classpath_string.contains(path.as_ref()),
            "missing from classpath: {path}"
        );
        assert!(!entry.path().exists(), "mod artifact leaked to {path}");
    }

    // Keep Fabric's working directory inside the test directory so an
    // extraction leak cannot be hidden by repository-local state.
    let previous_dir = std::env::current_dir().expect("current directory");
    std::env::set_current_dir(work.path()).expect("enter test working directory");

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
    let jvm: JavaVM = JavaVM::with_libjvm(builder.build().expect("JVM args"), {
        let jvm_dll = jdk.jvm_dll();
        move || Ok(jvm_dll.clone())
    })
    .expect("create JVM");
    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install read hooks");

    jvmsense_core::native::reset_trace();
    let vfs = Arc::new(app.vfs);
    let loaded = jvmsense_core::native::with_launch_vfs(Arc::clone(&vfs), || {
        let mut env = jvm.attach_current_thread().expect("attach");

        let client = env
            .get_static_field(
                "net/fabricmc/api/EnvType",
                "CLIENT",
                "Lnet/fabricmc/api/EnvType;",
            )
            .expect("EnvType.CLIENT")
            .l()
            .expect("EnvType");
        let knot = env
            .new_object(
                "net/fabricmc/loader/impl/launch/knot/Knot",
                "(Lnet/fabricmc/api/EnvType;)V",
                &[JValue::Object(&client)],
            )
            .expect("Knot");
        let args = env
            .new_object_array(0, "java/lang/String", JObject::null())
            .expect("Knot args");
        let class_loader = env
            .call_method(
                &knot,
                "init",
                "([Ljava/lang/String;)Ljava/lang/ClassLoader;",
                &[JValue::Object(&args)],
            )
            .expect("initialize Knot")
            .l()
            .expect("Knot class loader");

        let loader = env
            .get_static_field(
                "net/fabricmc/loader/FabricLoader",
                "INSTANCE",
                "Lnet/fabricmc/loader/FabricLoader;",
            )
            .expect("FabricLoader.INSTANCE")
            .l()
            .expect("FabricLoader");
        let mod_id = env.new_string("modmenu").expect("Mod Menu id");
        let modmenu_loaded = env
            .call_method(
                &loader,
                "isModLoaded",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&mod_id)],
            )
            .expect("FabricLoader.isModLoaded")
            .z()
            .expect("isModLoaded result");

        // This is the first load of the target. If Fabric's normal Mixin setup
        // did not happen before class loading, these methods will be absent.
        let target_name = env
            .new_string("net.minecraft.class_442")
            .expect("target name");
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
            .expect("load target through Knot")
            .l()
            .expect("target class");

        let has_mixin_method = class_has_mixin_method(&mut env, &target);
        (modmenu_loaded, has_mixin_method)
    });

    let (modmenu_loaded, has_mixin_method) = loaded;
    assert!(modmenu_loaded, "Fabric must discover launch-time Mod Menu");
    assert!(
        has_mixin_method,
        "Mixin must transform class_442 during its first class load"
    );
    assert!(
        vfs.disk_footprint().is_empty(),
        "no launch-time artifact may be materialized on disk"
    );
    assert!(
        !jvmsense_core::native::trace_snapshot().entries.is_empty(),
        "Fabric reads must be served through the VFS"
    );

    let processed_mods = work.path().join(".fabric").join("processedMods");
    let extracted = processed_mods
        .read_dir()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.file_type().is_ok())
        })
        .unwrap_or(false);
    assert!(
        !extracted,
        "Fabric extracted a nested mod to {}",
        processed_mods.display()
    );

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }
    std::env::set_current_dir(previous_dir).expect("restore working directory");
}
