//! JNI helpers for attaching an in-memory mod to a live Knot instance.

use std::path::Path;

use jni::objects::{JObject, JObjectArray, JValue};
use jni::JNIEnv;

/// Add a hollow placeholder path to `Knot`'s target class path.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] if any JNI constructor or method call fails.
pub fn add_to_knot_classpath(
    env: &mut JNIEnv<'_>,
    knot: &JObject<'_>,
    path: &Path,
) -> Result<(), jni::errors::Error> {
    let path_obj = path_object(env, path)?;
    let empty: JObjectArray<'_> = env.new_object_array(0, "java/lang/String", JObject::null())?;
    env.call_method(
        knot,
        "addToClassPath",
        "(Ljava/nio/file/Path;[Ljava/lang/String;)V",
        &[JValue::Object(&path_obj), JValue::Object(&empty)],
    )?;
    Ok(())
}

/// Register a mod candidate with the already-loaded Fabric loader.
///
/// This mirrors Fabric's own discovery result for one mod: metadata parsed by
/// `ModMetadataParser`, a `ModCandidateImpl` carrying the hollow jar path, and
/// `FabricLoaderImpl.addMod`. The candidate's path is read through the same
/// memory-backed VFS as the mod classes.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] if metadata parsing, candidate construction,
/// or registration fails.
pub fn register_mod_candidate(
    env: &mut JNIEnv<'_>,
    hollow_path: &Path,
    metadata_json: &[u8],
    source_name: &str,
) -> Result<(), jni::errors::Error> {
    let metadata_bytes = env.byte_array_from_slice(metadata_json)?;
    let metadata_stream = env.new_object(
        "java/io/ByteArrayInputStream",
        "([B)V",
        &[JValue::Object(&metadata_bytes)],
    )?;
    let source = env.new_string(source_name)?;
    let warnings = env.new_object("java/util/ArrayList", "()V", &[])?;
    let version_overrides = env.new_object(
        "net/fabricmc/loader/impl/metadata/VersionOverrides",
        "()V",
        &[],
    )?;
    let missing_overrides = hollow_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("jvmsense-no-dependency-overrides");
    let missing_overrides_path = path_object(env, &missing_overrides)?;
    let dependency_overrides = env.new_object(
        "net/fabricmc/loader/impl/metadata/DependencyOverrides",
        "(Ljava/nio/file/Path;)V",
        &[JValue::Object(&missing_overrides_path)],
    )?;

    let metadata = env
        .call_static_method(
            "net/fabricmc/loader/impl/metadata/ModMetadataParser",
            "parseMetadata",
            "(Ljava/io/InputStream;Ljava/lang/String;Ljava/util/List;Lnet/fabricmc/loader/impl/metadata/VersionOverrides;Lnet/fabricmc/loader/impl/metadata/DependencyOverrides;Z)Lnet/fabricmc/loader/impl/metadata/LoaderModMetadata;",
            &[
                JValue::Object(&metadata_stream),
                JValue::Object(&source),
                JValue::Object(&warnings),
                JValue::Object(&version_overrides),
                JValue::Object(&dependency_overrides),
                JValue::Bool(false.into()),
            ],
        )?
        .l()?;

    let paths = env.new_object("java/util/ArrayList", "()V", &[])?;
    let path = path_object(env, hollow_path)?;
    env.call_method(
        &paths,
        "add",
        "(Ljava/lang/Object;)Z",
        &[JValue::Object(&path)],
    )?;
    let empty_nested = env
        .call_static_method(
            "java/util/Collections",
            "emptyList",
            "()Ljava/util/List;",
            &[],
        )?
        .l()?;
    let candidate = env
        .call_static_method(
            "net/fabricmc/loader/impl/discovery/ModCandidateImpl",
            "createPlain",
            "(Ljava/util/List;Lnet/fabricmc/loader/impl/metadata/LoaderModMetadata;ZLjava/util/Collection;)Lnet/fabricmc/loader/impl/discovery/ModCandidateImpl;",
            &[
                JValue::Object(&paths),
                JValue::Object(&metadata),
                JValue::Bool(false.into()),
                JValue::Object(&empty_nested),
            ],
        )?
        .l()?;
    let loader = env
        .get_static_field(
            "net/fabricmc/loader/impl/FabricLoaderImpl",
            "INSTANCE",
            "Lnet/fabricmc/loader/impl/FabricLoaderImpl;",
        )?
        .l()?;
    env.call_method(
        &loader,
        "addMod",
        "(Lnet/fabricmc/loader/impl/discovery/ModCandidateImpl;)V",
        &[JValue::Object(&candidate)],
    )?;
    Ok(())
}

/// Read the post-Mixin bytecode for a class through the live Knot delegate.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] if the delegate or transformed bytes cannot
/// be reached.
pub fn get_post_mixin_bytes(
    env: &mut JNIEnv<'_>,
    class_loader: &JObject<'_>,
    binary_name: &str,
) -> Result<Vec<u8>, jni::errors::Error> {
    let delegate = env
        .get_field(
            class_loader,
            "delegate",
            "Lnet/fabricmc/loader/impl/launch/knot/KnotClassDelegate;",
        )?
        .l()?;
    let name = env.new_string(binary_name)?;
    let bytes = env
        .call_method(
            &delegate,
            "getPostMixinClassByteArray",
            "(Ljava/lang/String;Z)[B",
            &[JValue::Object(&name), JValue::Bool(false.into())],
        )?
        .l()?;
    read_byte_array(env, &bytes)
}

/// Define a class into a live class loader through `defineClassFwd`.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] if the class loader rejects the definition.
pub fn define_class(
    env: &mut JNIEnv<'_>,
    class_loader: &JObject<'_>,
    binary_name: &str,
    bytes: &[u8],
) -> Result<(), jni::errors::Error> {
    let name = env.new_string(binary_name.replace('.', "/"))?;
    let data = env.byte_array_from_slice(bytes)?;
    let length = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    env.call_method(
        class_loader,
        "defineClassFwd",
        "(Ljava/lang/String;[BIILjava/security/CodeSource;)Ljava/lang/Class;",
        &[
            JValue::Object(&name),
            JValue::Object(&data),
            JValue::Int(0),
            JValue::Int(length),
            JValue::Object(&JObject::null()),
        ],
    )?;
    Ok(())
}

/// Read a Java byte array into Rust.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] if the array cannot be converted.
pub fn read_byte_array(
    env: &mut JNIEnv<'_>,
    array: &JObject<'_>,
) -> Result<Vec<u8>, jni::errors::Error> {
    let array = unsafe { jni::objects::JByteArray::from_raw(array.as_raw()) };
    let len = env.get_array_length(&array)? as usize;
    let mut bytes = vec![0i8; len];
    env.get_byte_array_region(&array, 0, &mut bytes)?;
    Ok(bytes.into_iter().map(|value| value as u8).collect())
}
/// Register a Mixin configuration by resource name.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] if Mixin rejects the configuration or fails
/// to locate its resource.
pub fn register_mixin_config(
    env: &mut JNIEnv<'_>,
    config_name: &str,
) -> Result<(), jni::errors::Error> {
    let name = env.new_string(config_name)?;
    env.call_static_method(
        "org/spongepowered/asm/mixin/Mixins",
        "addConfiguration",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&name)],
    )?;
    Ok(())
}

/// Load a class through a specific loader, initializing it.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] when the class cannot be loaded.
pub fn load_class_init<'local>(
    env: &mut JNIEnv<'local>,
    loader: &JObject<'_>,
    binary_name: &str,
) -> Result<JObject<'local>, jni::errors::Error> {
    let name = env.new_string(binary_name)?;
    env.call_static_method(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
        &[
            JValue::Object(&name),
            JValue::Bool(true.into()),
            JValue::Object(loader),
        ],
    )?
    .l()
}

/// Instantiate a class and invoke its no-argument method.
///
/// # Errors
///
/// Returns [`jni::errors::Error`] if construction or invocation fails.
pub fn invoke_no_arg_method(
    env: &mut JNIEnv<'_>,
    class: &JObject<'_>,
    method: &str,
) -> Result<(), jni::errors::Error> {
    let empty_classes: JObjectArray<'_> =
        env.new_object_array(0, "java/lang/Class", JObject::null())?;
    let constructor = env
        .call_method(
            class,
            "getDeclaredConstructor",
            "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            &[JValue::Object(&empty_classes)],
        )?
        .l()?;
    let empty_objects: JObjectArray<'_> =
        env.new_object_array(0, "java/lang/Object", JObject::null())?;
    let instance = env
        .call_method(
            &constructor,
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
            &[JValue::Object(&empty_objects)],
        )?
        .l()?;
    env.call_method(&instance, method, "()V", &[])?;
    Ok(())
}

fn path_object<'local>(
    env: &mut JNIEnv<'local>,
    path: &Path,
) -> Result<JObject<'local>, jni::errors::Error> {
    let text = env.new_string(path.to_string_lossy())?;
    let file = env.new_object(
        "java/io/File",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&text)],
    )?;
    env.call_method(&file, "toPath", "()Ljava/nio/file/Path;", &[])?
        .l()
}
