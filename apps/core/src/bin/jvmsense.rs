//! `jvmsense` command-line entry point.
//!
//! The binary exists so a launch can be driven from a shell, not only from a
//! test. That matters for the case the crate is for: starting a real
//! application, watching it run, and being able to see what the hooks did.

use std::path::PathBuf;
use std::process::ExitCode;

use jvmsense_core::jdk::{Jdk, JdkProvisioner};
use jvmsense_core::launch::{
    create_session_directory, request_from_fabric, run, FabricApplication, LaunchRequest,
};
use jvmsense_core::native::trace_snapshot;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("--help" | "-h" | "help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some("--version" | "-V") => {
            println!("jvmsense {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("launch") => match launch(args.collect()) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("jvmsense: {message}");
                ExitCode::from(2)
            }
        },
        Some(other) => {
            eprintln!("jvmsense: unknown command {other:?}");
            eprintln!();
            print_usage();
            ExitCode::from(2)
        }
    }
}

/// Options for `launch`.
struct LaunchArgs {
    /// The game jar: served from memory, never read from disk.
    game_jar: PathBuf,
    /// Mod jars, in load order.
    mods: Vec<PathBuf>,
    /// Everything else on the classpath, which stays a real file.
    classpath: Vec<PathBuf>,
    /// Arguments handed to the application's `main`.
    application_args: Vec<String>,
    /// Where the application may write.
    working_directory: PathBuf,
    /// A JDK to use instead of the provisioned one.
    jdk_home: Option<PathBuf>,
    /// The application's main class, overriding the layout's default.
    main_class: Option<String>,
    /// Remap the official game jar to intermediary before mounting it.
    remap_official: bool,
    /// Tiny v2 official-to-intermediary mappings for offline remapping.
    mappings: Option<PathBuf>,
    /// TinyRemapper fat jar used by the offline remapper.
    remapper: Option<PathBuf>,
    /// Extra system properties, as `key=value`.
    properties: Vec<(String, String)>,
}

/// Read a classpath from a file of one path per line.
///
/// A real Minecraft classpath is 80-odd jars. Passing those as repeated flags
/// runs into command-line length limits on Windows, and the failure mode is
/// that entries are silently dropped rather than an error — which shows up much
/// later as a `ClassNotFoundException` for a library that was in the file.
///
/// # Errors
///
/// Returns a message if the file cannot be read.
fn read_classpath_file(path: &std::path::Path) -> Result<Vec<PathBuf>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|source| format!("cannot read {}: {source}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn parse_launch(args: Vec<String>) -> Result<LaunchArgs, String> {
    let mut parsed = LaunchArgs {
        game_jar: PathBuf::new(),
        mods: Vec::new(),
        classpath: Vec::new(),
        application_args: Vec::new(),
        working_directory: std::env::current_dir().map_err(|e| e.to_string())?,
        jdk_home: std::env::var_os("JVMSENSE_JRE").map(PathBuf::from),
        main_class: None,
        properties: Vec::new(),
        remap_official: false,
        mappings: None,
        remapper: None,
    };

    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let mut value = || iter.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--game-jar" => parsed.game_jar = PathBuf::from(value()?),
            "--mod" => parsed.mods.push(PathBuf::from(value()?)),
            "--classpath" => parsed.classpath.push(PathBuf::from(value()?)),
            "--classpath-file" => {
                parsed
                    .classpath
                    .extend(read_classpath_file(&PathBuf::from(value()?))?);
            }
            "--jdk" => parsed.jdk_home = Some(PathBuf::from(value()?)),
            "--main" => parsed.main_class = Some(value()?),
            "--remap-official" => parsed.remap_official = true,
            "--mappings" => parsed.mappings = Some(PathBuf::from(value()?)),
            "--remapper" => parsed.remapper = Some(PathBuf::from(value()?)),
            "-D" => {
                let pair = value()?;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("-D needs key=value, got {pair:?}"))?;
                parsed.properties.push((key.to_string(), value.to_string()));
            }
            "--workdir" => parsed.working_directory = PathBuf::from(value()?),
            "--arg" => parsed.application_args.push(value()?),
            other => return Err(format!("unknown option {other:?}")),
        }
    }

    if parsed.game_jar.as_os_str().is_empty() {
        return Err("--game-jar is required".into());
    }
    Ok(parsed)
}

/// Whether the classpath includes a Fabric loader.
///
/// Detected rather than declared: a file named `fabric-loader-*.jar` is
/// unambiguous, and a user who gave a Fabric classpath but forgot a flag would
/// otherwise get a launch that fails in a confusing way.
fn classpath_has_fabric(classpath: &[PathBuf]) -> bool {
    classpath.iter().any(|entry| {
        entry
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("fabric-loader-"))
    })
}

fn launch(args: Vec<String>) -> Result<ExitCode, String> {
    let options = parse_launch(args)?;

    // Provision the JDK if one was not supplied, so a first run needs no setup.
    let jdk = match options.jdk_home {
        Some(home) => Jdk::from_home(home),
        None => {
            let provisioner = JdkProvisioner::with_default_cache().map_err(|e| e.to_string())?;
            println!(
                "provisioning a JDK into {}",
                provisioner.install_dir().display()
            );
            provisioner.ensure().map_err(|e| e.to_string())?
        }
    };

    let session_root =
        create_session_directory().map_err(|e| format!("cannot prepare a session: {e}"))?;
    println!("session root: {}", session_root.display());

    let app = if options.remap_official {
        let mappings = options.mappings.as_ref().expect("checked by parse_launch");
        let remapper = options.remapper.as_ref().expect("checked by parse_launch");
        let java = jdk.home().join("bin").join("java.exe");
        let remapped = jvmsense_core::remap::official_to_intermediary(
            &jvmsense_core::remap::TinyRemapRequest {
                java: &java,
                remapper_jar: remapper,
                mappings,
                source_jar: &options.game_jar,
                classpath: &options.classpath,
                scratch_dir: &session_root.join("remap-helper"),
            },
        )
        .map_err(|e| format!("cannot remap the game jar: {e}"))?;

        FabricApplication::mount_remapped(
            &session_root,
            "client-intermediary.jar",
            remapped,
            &options.mods,
            options.classpath.clone(),
            Vec::new(),
            options.application_args,
        )
        .map_err(|e| format!("cannot mount the application: {e}"))?
    } else {
        FabricApplication::mount(
            &session_root,
            &options.game_jar,
            &options.mods,
            options.classpath.clone(),
            Vec::new(),
            options.application_args,
        )
        .map_err(|e| format!("cannot mount the application: {e}"))?
    };

    let game_on_disk = std::fs::metadata(&options.game_jar)
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "game jar: {game_on_disk} bytes in memory, {} bytes on disk",
        std::fs::metadata(app.layout.game_jar.path())
            .map(|m| m.len())
            .unwrap_or(0)
    );
    let fabric = classpath_has_fabric(&options.classpath);
    let mut request: LaunchRequest =
        request_from_fabric(jdk, &app.layout, options.working_directory, fabric);
    // A vanilla launch has no Fabric loader, so the entry point is the game's
    // own class rather than the loader's.
    if let Some(main_class) = &options.main_class {
        request.main_class.clone_from(main_class);
    }
    request
        .system_properties
        .extend(options.properties.iter().cloned());
    println!("main class: {}", request.main_class);
    println!("classpath:  {} entries", request.classpath.len());

    // The layout borrows from `app`, so the VFS is moved out after the request
    // is built; the request holds only owned data.
    let vfs = app.vfs;
    println!("launching...\n");

    let outcome = run(&request, vfs).map_err(|e| format!("launch failed: {e}"))?;

    println!("\n--- result ---");
    println!("served from memory: {}", outcome.served_reads);
    println!("fell through:       {}", outcome.passed_through);

    let trace = trace_snapshot();
    println!("detour activity:    {:?}", trace.entries);

    match &outcome.error {
        Some(error) => {
            println!("application error:  {error}");
            Ok(ExitCode::from(1))
        }
        None if outcome.completed => {
            println!("completed");
            Ok(ExitCode::SUCCESS)
        }
        None => {
            println!("did not complete");
            Ok(ExitCode::from(1))
        }
    }
}

fn print_usage() {
    println!(
        "jvmsense {} — in-memory Java application launcher

USAGE:
    jvmsense launch --game-jar <PATH> [OPTIONS]
    jvmsense --version

LAUNCH OPTIONS:
    --game-jar <PATH>   The application jar, served from memory (required)
    --mod <PATH>        A mod jar, served from memory (repeatable)
    --classpath <PATH>  An additional real classpath entry (repeatable)
    --classpath-file <PATH>  A file listing classpath entries, one per line
    --arg <VALUE>       An argument for the application's main (repeatable)
    --workdir <PATH>    Working directory for the application
    --jdk <PATH>        A JDK home to use instead of provisioning one
    --remap-official    Remap official Minecraft to intermediary before mounting
    --mappings <PATH>   Tiny v2 official-to-intermediary mappings
    --remapper <PATH>   TinyRemapper fat jar used for offline remapping
    --main <CLASS>      The main class, when it is not the Fabric entry point
    JVMSENSE_JRE        A JDK home, equivalent to --jdk

The game jar's bytes are read into memory and never re-read from disk: the path
the JVM sees is a virtual path whose reads the launcher answers.
",
        env!("CARGO_PKG_VERSION")
    );
}
