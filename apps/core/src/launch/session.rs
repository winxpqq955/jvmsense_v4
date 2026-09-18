//! Driving a complete launch: session, JVM, hooks, and the application.
//!
//! Everything up to here has been tested a layer at a time. This module is the
//! assembly: it takes a layout, sets up a [`crate::vfs::VirtualFileSystem`],
//! creates the JVM, installs the hooks, and runs the application's main class.
//!
//! The ordering is the substance. Three constraints shape it, and each was
//! measured rather than assumed:
//!
//! - The VFS must exist before the JVM, because the classpath is built from its
//!   placeholder paths.
//! - The JVM must exist before the hooks, because `java.dll` and `nio.dll` are
//!   loaded by (or lazily after) JVM creation — `GetProcAddress` on a module
//!   that is not loaded yet simply fails.
//! - The session must be published to the hooks before any application class
//!   loads, because the first `ZipFile` on a placeholder is what needs it.

use std::path::PathBuf;
use std::sync::Arc;

use jni::objects::{JObject, JValue};
use jni::{InitArgsBuilder, JNIVersion, JavaVM};

use crate::jdk::Jdk;
use crate::launch::fabric::FabricLayout;
use crate::native::HookSet;
use crate::vfs::VirtualFileSystem;

/// What a launch needs to know before it starts.
#[derive(Debug)]
pub struct LaunchRequest {
    /// The JDK to run.
    pub jdk: Jdk,
    /// The application's main class.
    pub main_class: String,
    /// Arguments passed to that class's `main`.
    pub arguments: Vec<String>,
    /// Every classpath entry, in order.
    pub classpath: Vec<crate::launch::ClasspathEntry>,
    /// System properties to set.
    pub system_properties: Vec<(String, String)>,
    /// A writable directory the application may use.
    pub working_directory: PathBuf,
}

/// The outcome of a launch.
#[derive(Debug)]
pub struct LaunchOutcome {
    /// True when `main` returned without throwing.
    pub completed: bool,
    /// The exception's description when it did not.
    pub error: Option<String>,
    /// Reads served from memory.
    pub served_reads: u64,
    /// Reads that fell through to the real filesystem.
    pub passed_through: u64,
}

impl LaunchOutcome {
    /// Whether the launch is a success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.completed && self.error.is_none()
    }
}

/// Run a launch to completion.
///
/// `vfs` is taken by value and held for the duration, so every placeholder
/// lives exactly as long as the launch that needs it.
///
/// # Errors
///
/// Returns [`LaunchError`] if the JVM cannot be created or the hooks cannot be
/// installed. A Java exception raised by the application is *not* an error
/// here: it is reported in [`LaunchOutcome`], because an application failing is
/// a normal outcome rather than a launcher malfunction.
pub fn run(request: &LaunchRequest, vfs: VirtualFileSystem) -> Result<LaunchOutcome, LaunchError> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    let classpath = request
        .classpath
        .iter()
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(&separator.to_string());

    // A JVM resolves relative paths against the *process* working directory,
    // not against `user.dir` — that property is only what `System.getProperty`
    // reports. Minecraft writes `logs/latest.log` relative to its run
    // directory, so the process must actually be there. The previous directory
    // is restored when the launch returns.
    let previous_directory = std::env::current_dir().ok();
    if std::env::var_os("JVMSENSE_NO_CHDIR").is_none() && request.working_directory.is_dir() {
        std::env::set_current_dir(&request.working_directory).map_err(|source| {
            LaunchError::WorkingDirectory {
                path: request.working_directory.clone(),
                message: source.to_string(),
            }
        })?;
    }

    let mut builder = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option(format!("-Djava.class.path={classpath}"))
        .option(format!(
            "-Duser.dir={}",
            request.working_directory.display()
        ));

    for (key, value) in &request.system_properties {
        builder = builder.option(format!("-D{key}={value}"));
    }

    let args = builder.build().map_err(|source| LaunchError::Arguments {
        message: source.to_string(),
    })?;

    let jvm_dll = request.jdk.jvm_dll();
    let jvm: JavaVM = JavaVM::with_libjvm(args, move || Ok(jvm_dll.clone())).map_err(|source| {
        LaunchError::Jvm {
            message: source.to_string(),
        }
    })?;

    // Hooks go in now: `java.dll` exists because the JVM does, and installing
    // any earlier would fail to resolve the symbols.
    //
    // `JVMSENSE_NO_HOOKS` disables them, which exists so a failure can be
    // attributed to the hooks or to everything else by running the same launch
    // twice. Without that switch, diagnosing a graphics or audio failure means
    // editing and rebuilding the launcher.
    let hooks = if std::env::var_os("JVMSENSE_NO_HOOKS").is_some() {
        eprintln!("jvmsense: hooks disabled by JVMSENSE_NO_HOOKS");
        None
    } else {
        Some(
            HookSet::install_read_hooks(request.jdk.home()).map_err(|source| {
                LaunchError::Hooks {
                    message: source.to_string(),
                }
            })?,
        )
    };

    let vfs = Arc::new(vfs);
    let main_class = request.main_class.clone();
    let arguments = request.arguments.clone();

    let (completed, error) = crate::native::with_vfs(Arc::clone(&vfs), || {
        // The launching thread must be named `main`.
        //
        // The JVM names the thread that called `JNI_CreateJavaVM` `main` and
        // then, as `jni`'s `with_libjvm` does, detaches it. The thread we
        // attach here therefore arrives as `Thread-0`. Minecraft's
        // `RenderSystem` records whichever thread runs it as the render thread
        // and verifies later that it is still on that thread, so a name the
        // game does not expect is not itself fatal — but a *different* thread
        // is, and matching the conventional name keeps diagnostics readable
        // and any name-based check satisfied.
        let mut env = match jvm.attach_current_thread() {
            Ok(env) => env,
            Err(source) => return (false, Some(format!("cannot attach to the JVM: {source}"))),
        };

        if let Err(source) = name_current_thread(&mut env, "main") {
            return (
                false,
                Some(format!("cannot name the launching thread: {source}")),
            );
        }

        let class = match env.find_class(main_class.replace('.', "/").as_str()) {
            Ok(class) => class,
            Err(source) => {
                let described = describe_exception(&mut env);
                return (
                    false,
                    Some(
                        described.unwrap_or_else(|| format!("cannot find {main_class}: {source}")),
                    ),
                );
            }
        };

        // The application's `main` takes a `String[]`.
        let java_args = match build_string_array(&mut env, &arguments) {
            Ok(array) => array,
            Err(source) => return (false, Some(source)),
        };

        match env.call_static_method(
            &class,
            "main",
            "([Ljava/lang/String;)V",
            &[JValue::Object(&java_args)],
        ) {
            Ok(_) => (true, None),
            Err(source) => {
                let described = describe_exception(&mut env);
                (
                    false,
                    Some(described.unwrap_or_else(|| format!("main threw: {source}"))),
                )
            }
        }
    });

    let (served_reads, passed_through) = crate::native::hit_miss_counts();

    // Nothing was written: assert the invariant here rather than only in tests,
    // because a launch that leaks an artifact silently defeats the whole crate.
    let footprint = vfs.disk_footprint();
    if !crate::vfs::hollow::all_placeholders_empty(&footprint) {
        eprintln!("jvmsense: WARNING an artifact byte reached disk: {footprint:?}");
    }

    drop(hooks);
    unsafe {
        let _ = jvm.destroy();
    }

    if let Some(previous) = previous_directory {
        let _ = std::env::set_current_dir(previous);
    }

    Ok(LaunchOutcome {
        completed,
        error,
        served_reads,
        passed_through,
    })
}

/// Rename the current Java thread.
///
/// Done through `Thread.currentThread().setName`, which is the only way to
/// change a thread's name once it exists.
fn name_current_thread(env: &mut jni::JNIEnv, name: &str) -> Result<(), String> {
    let thread = env
        .call_static_method(
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        )
        .map_err(|source| source.to_string())?
        .l()
        .map_err(|source| source.to_string())?;

    let java_name = env.new_string(name).map_err(|source| source.to_string())?;
    env.call_method(
        &thread,
        "setName",
        "(Ljava/lang/String;)V",
        &[jni::objects::JValue::Object(&java_name)],
    )
    .map_err(|source| source.to_string())?;
    Ok(())
}

/// Describe and clear a pending Java exception.
fn describe_exception(env: &mut jni::JNIEnv) -> Option<String> {
    if !env.exception_check().unwrap_or(false) {
        return None;
    }
    // Print the full stack, including suppressed exceptions: a
    // `NoClassDefFoundError` from a static initializer hides the real cause
    // behind a suppressed throwable, and without it the report says only that
    // some class could not initialize.
    if let Ok(throwable) = env.exception_occurred() {
        let _ = env.call_method(&throwable, "printStackTrace", "()V", &[]);
    }
    // A `NoClassDefFoundError` from a static initializer hides the real
    // failure in its cause; Minecraft's own crash report omits that chain.
    if let Ok(throwable) = env.exception_occurred() {
        let _ = env.call_method(&throwable, "getCause", "()Ljava/lang/Throwable;", &[]);
    }
    env.exception_describe().ok();
    let throwable = env.exception_occurred().ok();
    let message = throwable.and_then(|throwable| {
        let text = env
            .call_method(&throwable, "toString", "()Ljava/lang/String;", &[])
            .ok()?
            .l()
            .ok()?;
        let text = jni::objects::JString::from(text);
        env.get_string(&text)
            .ok()
            .map(|s| s.to_string_lossy().into_owned())
    });
    env.exception_clear().ok();
    message
}

/// Build a Java `String[]` from Rust strings.
fn build_string_array(
    env: &mut jni::JNIEnv,
    values: &[String],
) -> Result<JObject<'static>, String> {
    let array = env
        .new_object_array(values.len() as i32, "java/lang/String", JObject::null())
        .map_err(|source| format!("cannot allocate the argument array: {source}"))?;

    for (index, value) in values.iter().enumerate() {
        let string = env
            .new_string(value)
            .map_err(|source| format!("cannot allocate argument {index}: {source}"))?;
        env.set_object_array_element(&array, index as i32, &string)
            .map_err(|source| format!("cannot store argument {index}: {source}"))?;
    }

    Ok(unsafe { JObject::from_raw(array.into_raw()) })
}

/// Build a [`LaunchRequest`] from a Fabric layout.
///
/// `fabric` decides whether the Fabric system properties are set. They must
/// **not** be set for a vanilla launch: Fabric's game provider watches
/// `fabric.gameJarPath.client` and takes over the launch when it sees it, which
/// turns a vanilla start into a half-initialised Fabric one. The properties are
/// only correct when the loader is actually on the classpath.
#[must_use]
pub fn request_from_fabric(
    jdk: Jdk,
    layout: &FabricLayout,
    working_directory: PathBuf,
    fabric: bool,
) -> LaunchRequest {
    let system_properties = if fabric {
        layout.system_properties().into_iter().collect()
    } else {
        Vec::new()
    };

    LaunchRequest {
        jdk,
        main_class: layout.main_class.clone(),
        arguments: layout.arguments.clone(),
        classpath: layout.classpath(),
        system_properties,
        working_directory,
    }
}

/// Create a private directory for one session's placeholders.
///
/// # Why the location matters
///
/// The directory must resolve to itself. Windows redirects many "local app
/// data" paths through a junction — a packaged app's `LocalCache` is the common
/// case, and this machine has one — and a placeholder reached through a
/// junction has a `toRealPath()` that differs from its literal path. The
/// application asserts `path.equals(path.toRealPath())`, so that difference
/// surfaces much later as a file that seemingly exists but cannot be found.
///
/// Candidates are therefore *tried* rather than assumed: each is created and
/// checked, and the first that resolves to itself wins. Checking is the only
/// way to know, since the Windows reparse attribute is not set on a junction
/// that a redirection layer created.
///
/// # Errors
///
/// Returns [`LaunchError::SessionDirectory`] if no candidate directory resolves
/// to itself.
pub fn create_session_directory() -> Result<PathBuf, LaunchError> {
    // Named unpredictably, so two concurrent launches cannot see each other's
    // placeholders — which would let one session serve another's bytes, since
    // the hooks match on path.
    let unique = format!(
        "session-{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64) << 32
    );

    let mut candidates: Vec<PathBuf> = Vec::new();
    // The executable's own directory first: whatever launched this can write
    // there, and it is never a redirection target.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("jvmsense-sessions"));
        }
    }
    if let Some(temp) = std::env::var_os("TEMP") {
        candidates.push(PathBuf::from(temp).join("jvmsense").join("sessions"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("jvmsense").join("sessions"));
    }

    let mut attempts = Vec::new();
    for base in candidates {
        let root = base.join(&unique);
        if let Err(source) = std::fs::create_dir_all(&root) {
            attempts.push(format!("{}: {source}", root.display()));
            continue;
        }
        match crate::vfs::pathkey::verify_session_dir(&root) {
            Ok(_) => return Ok(root),
            Err(source) => {
                attempts.push(format!("{}: {source}", root.display()));
                // Leave nothing behind from a rejected candidate.
                let _ = std::fs::remove_dir_all(&root);
            }
        }
    }

    Err(LaunchError::SessionDirectory {
        message: format!(
            "no candidate directory resolves to itself: {}",
            attempts.join("; ")
        ),
    })
}

/// Errors that prevent a launch from starting.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot build the JVM arguments: {message}")]
    Arguments { message: String },

    #[error("cannot create the JVM: {message}")]
    Jvm { message: String },

    #[error("cannot install the native hooks: {message}")]
    Hooks { message: String },

    #[error("cannot prepare a session directory: {message}")]
    SessionDirectory { message: String },

    #[error("cannot enter the working directory {path}: {message}")]
    WorkingDirectory { path: PathBuf, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outcome_is_successful_only_when_it_completed_without_error() {
        assert!(LaunchOutcome {
            completed: true,
            error: None,
            served_reads: 1,
            passed_through: 0,
        }
        .is_success());

        assert!(!LaunchOutcome {
            completed: false,
            error: Some("boom".into()),
            served_reads: 0,
            passed_through: 0,
        }
        .is_success());
    }

    #[test]
    fn a_request_from_a_layout_keeps_the_loader_first() {
        let layout = FabricLayout {
            game_jar: crate::launch::ClasspathEntry::Hollow(PathBuf::from("game.jar")),
            mods: vec![],
            loader_jars: vec![crate::launch::ClasspathEntry::Real(PathBuf::from(
                "loader.jar",
            ))],
            libraries: vec![crate::launch::ClasspathEntry::Real(PathBuf::from(
                "lib.jar",
            ))],
            mods_folder: PathBuf::from("mods"),
            main_class: FabricLayout::KNOT_CLIENT.to_string(),
            arguments: vec![],
        };

        let request =
            request_from_fabric(Jdk::from_home("jdk"), &layout, PathBuf::from("work"), true);

        assert_eq!(
            request.classpath[0].path(),
            std::path::Path::new("loader.jar")
        );
        assert_eq!(
            request.classpath[1].path(),
            std::path::Path::new("game.jar")
        );
        assert_eq!(request.main_class, FabricLayout::KNOT_CLIENT);
    }
}
