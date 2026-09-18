//! JVMTI redefinition of an already-loaded Minecraft class with Mod Menu Mixins.
//!
//! This is the hard V6 case: Mixin adds handler methods that cannot be
//! introduced by `RedefineClasses`. The ASM helper moves those methods into a
//! generated static-handler class and rewrites the target call sites before
//! the JVMTI redefinition.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jni::objects::{JClass, JObject, JValue};
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use jvmsense_core::native::jvmti::{ClassRedefinition, Jvmti};
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
        .filter_map(|line| {
            let path = PathBuf::from(line);
            let file = path.file_name()?.to_string_lossy();
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
        if name.to_string_lossy().contains("adjustRealmsHeight")
            || name.to_string_lossy().contains("onRender")
        {
            return true;
        }
    }
    false
}

#[test]
#[ignore = "remaps Minecraft, initializes Fabric/Knot, and redefines a loaded class"]
fn loaded_title_screen_is_redefined_with_static_handlers() {
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
    let loader_jars = support::ensure_fabric_jars();
    assert!(!loader_jars.is_empty(), "Fabric loader jars are required");

    let helper_bytes = jvmsense_core::runtime::helper::compile_runtime_injector(
        jdk.home(),
        &loader_jars,
        &work.path().join("inject-helper"),
    )
    .expect("compile runtime injector helper");

    let app = jvmsense_core::launch::FabricApplication::mount_remapped(
        work.path().join("session"),
        "client-intermediary.jar",
        remapped,
        &[],
        loader_jars,
        classpath.clone(),
        Vec::new(),
    )
    .expect("mount game");
    assert!(
        !app.layout
            .system_properties()
            .contains_key(jvmsense_core::launch::fabric::DEVELOPMENT_PROPERTY),
        "fabric.development must remain disabled for in-memory injection"
    );

    let mut builder = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!(
            "-Djava.class.path={}",
            app.layout.classpath_string()
        ))
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
    let hooks = HookSet::install_read_hooks(jdk.home()).expect("install hooks");

    let mod_path = support::ensure_modmenu().expect("Mod Menu");
    let mod_bytes = std::fs::read(&mod_path).expect("read Mod Menu");
    let mod_plan = jvmsense_core::runtime::parse_runtime_mod(&mod_bytes).expect("parse Mod Menu");
    let mod_metadata = jvmsense_core::runtime::read_mod_entry(&mod_bytes, "fabric.mod.json")
        .expect("fabric.mod.json");
    let nested: Vec<(String, Vec<u8>)> = mod_plan
        .nested_jars
        .iter()
        .map(|path| {
            (
                path.clone(),
                jvmsense_core::runtime::read_mod_entry(&mod_bytes, path).expect("nested"),
            )
        })
        .collect();

    jvmsense_core::native::reset_trace();
    let vfs = Arc::new(app.vfs);
    let redefined = jvmsense_core::native::with_vfs_for_test(Arc::clone(&vfs), || {
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
            .expect("Knot.init")
            .l()
            .expect("class loader");

        let runtime_mod = vfs
            .mount_memory_runtime(
                jvmsense_core::vfs::ArtifactRole::Mod,
                "modmenu-runtime.jar",
                mod_bytes.clone(),
                true,
            )
            .expect("mount Mod Menu");
        let refmap = vfs
            .read_resource("modmenu-refmap.json")
            .expect("read refmap")
            .expect("refmap present");
        assert!(
            String::from_utf8_lossy(&refmap).contains("\"mappings\""),
            "the Mixin refmap must be served from the in-memory jar overlaid into the VFS"
        );
        jvmsense_core::runtime::jni::add_to_knot_classpath(
            &mut env,
            &knot,
            &runtime_mod.path().to_path_buf(),
        )
        .expect("add Mod Menu to classpath");
        for (name, bytes) in &nested {
            let mount = vfs
                .mount_memory_runtime(
                    jvmsense_core::vfs::ArtifactRole::Library,
                    name,
                    bytes.clone(),
                    true,
                )
                .expect("mount nested jar");
            jvmsense_core::runtime::jni::add_to_knot_classpath(
                &mut env,
                &knot,
                &mount.path().to_path_buf(),
            )
            .expect("add nested jar to classpath");
        }
        let helper_mount = vfs
            .mount_memory_runtime(
                jvmsense_core::vfs::ArtifactRole::Library,
                "jvmsense-runtime-helper.jar",
                helper_bytes.clone(),
                true,
            )
            .expect("mount runtime helper");
        jvmsense_core::runtime::jni::add_to_knot_classpath(
            &mut env,
            &knot,
            &helper_mount.path().to_path_buf(),
        )
        .expect("add runtime helper to classpath");
        let helper_name = env
            .new_string("jvmsense.runtime.RuntimeInjector")
            .expect("helper class name");
        let helper_class = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&helper_name),
                    JValue::Bool(false.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("load runtime helper")
            .l()
            .expect("helper class");
        let helper_class = unsafe { JClass::from_raw(helper_class.as_raw()) };
        let relaxed_config = jvmsense_core::runtime::relaxed_mixin_config_jar(
            &mod_plan.mixin_configs[0],
            "jvmsense-runtime/modmenu.mixins.json",
            &["com.terraformersmc.modmenu.mixin.MixinTitleScreen"],
        )
        .expect("build relaxed Mixin config");
        let config_mount = vfs
            .mount_memory_runtime(
                jvmsense_core::vfs::ArtifactRole::Library,
                "jvmsense-modmenu-config.jar",
                relaxed_config,
                true,
            )
            .expect("mount relaxed Mixin config");
        jvmsense_core::runtime::jni::add_to_knot_classpath(
            &mut env,
            &knot,
            &config_mount.path().to_path_buf(),
        )
        .expect("add relaxed Mixin config to classpath");
        jvmsense_core::runtime::jni::register_mod_candidate(
            &mut env,
            &runtime_mod.path().to_path_buf(),
            &mod_metadata,
            "modmenu",
        )
        .expect("register Mod Menu");

        // Load the target first, so this test cannot pass through ordinary
        // class-load-time Mixin application.
        let target_name = "net.minecraft.class_442";
        let target_name_java = env.new_string(target_name).expect("target name");
        let target = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&target_name_java),
                    JValue::Bool(false.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("preload target")
            .l()
            .expect("target class");
        let before = vfs
            .read_class(target_name)
            .expect("read target")
            .expect("target present");

        jvmsense_core::runtime::jni::register_mixin_config(
            &mut env,
            "jvmsense-runtime/modmenu.mixins.json",
        )
        .expect("register Mixin config");
        let after =
            jvmsense_core::runtime::jni::get_post_mixin_bytes(&mut env, &class_loader, target_name)
                .expect("transform target");

        let before_array = env.byte_array_from_slice(&before).expect("before array");
        let after_array = env.byte_array_from_slice(&after).expect("after array");
        let prepared = env
            .call_static_method(
                &helper_class,
                "prepareRedefinition",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;[B[B)[B",
                &[
                    JValue::Object(&class_loader),
                    JValue::Object(&target_name_java),
                    JValue::Object(&before_array),
                    JValue::Object(&after_array),
                ],
            )
            .expect("prepare redefinition")
            .l()
            .expect("prepared bytes");
        let prepared = jvmsense_core::runtime::jni::read_byte_array(&mut env, &prepared)
            .expect("read prepared bytes");

        let target_class = unsafe { JClass::from_raw(target.as_raw()) };
        let jvmti = unsafe { Jvmti::from_vm(&jvm) }.expect("JVMTI");
        jvmti
            .enable_class_redefinition()
            .expect("redefine capability");
        jvmti
            .redefine_classes(&[ClassRedefinition {
                class: &target_class,
                bytes: &prepared,
            }])
            .expect("redefine loaded target");

        let handler_name = env
            .new_string("net.minecraft.JvmsenseStaticHandlers_1")
            .expect("handler class name");
        let handler = env
            .call_static_method(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    JValue::Object(&handler_name),
                    JValue::Bool(false.into()),
                    JValue::Object(&class_loader),
                ],
            )
            .expect("load static handler class")
            .l()
            .expect("handler class");
        let has_mixin_method = class_has_mixin_method(&mut env, &handler);
        let footprint = vfs.disk_footprint();
        (has_mixin_method, footprint)
    });

    let (has_mixin_method, footprint) = redefined;
    assert!(
        has_mixin_method,
        "the redefined class must contain a Mod Menu Mixin handler"
    );
    assert!(
        footprint.is_empty(),
        "no runtime or startup artifact may be materialized on disk: {footprint:?}"
    );
    assert!(
        !jvmsense_core::native::trace_snapshot().entries.is_empty(),
        "runtime reads must be served through the VFS"
    );

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }
}
