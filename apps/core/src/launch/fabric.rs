//! Launching Fabric, with the game and the mods served from memory.
//!
//! Fabric's launcher (`Knot`) treats the JVM's classpath as the game classpath:
//! it scans `java.class.path`, opens each entry with
//! `new ZipFile(path.toFile())`, and classifies the result into the Minecraft
//! game jar, its libraries, and the loader itself. That design is what the
//! hollow-path VFS exists to satisfy — every one of those entries is a
//! zero-length placeholder whose bytes live in this process.
//!
//! # What this module decides
//!
//! Three things, all of them verified against fabric-loader's source rather
//! than assumed (see `spikes/FINDINGS.md`):
//!
//! 1. **Which system properties to set.** Several of Fabric's are load-bearing
//!    in ways that are not obvious — setting `fabric.development` silently
//!    turns on a runtime remapper that writes to disk, and setting
//!    `mixin.service` breaks Fabric's own Mixin bootstrap.
//!
//! 2. **Where each jar goes on the classpath.** Loader dependencies come first
//!    so `LoaderUtil.verifyClasspath()` sees exactly one `FabricLoader.class`
//!    and one `ClassReader.class`. The loader root can also be a hollow image
//!    after its nested modules are flattened and mounted explicitly.
//!
//! 3. **That the game jar already carries the namespace Fabric expects.** A
//!    production launch remaps `official` → `intermediary` at runtime by
//!    writing through `OutputConsumerPath` to `gameDir/.fabric/remappedJars/`.
//!    Shipping a jar that is *already* intermediary, and fixing
//!    `fabric.runtimeMappingNamespace=intermediary`, makes that whole step a
//!    no-op, which is the only way to keep the launch disk-free.
//!
//! The resulting default pipeline is deliberately pre-JVM: parse each mod,
//! flatten its nested jars, remap/bake when required, mount every image behind
//! a zero-byte placeholder, and register the explicit mod list before Java code
//! starts. Fabric and Mixin then handle discovery, AccessWideners, and class
//! transformation at class-load time. JVMTI is not part of this path.

use std::collections::{BTreeMap, HashSet};
use std::io::{Cursor, Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::runtime::{parse_runtime_mod, read_mod_entry, RuntimeModError};
use crate::vfs::{ArtifactRole, MountError, VirtualFileSystem};
use zip::ZipArchive;

/// The system property Fabric reads the game jar from.
pub const GAME_JAR_PROPERTY: &str = "fabric.gameJarPath.client";
/// The system property naming extra mod jars to load.
pub const ADD_MODS_PROPERTY: &str = "fabric.addMods";
/// The system property naming the directory Fabric scans for mods.
pub const MODS_FOLDER_PROPERTY: &str = "fabric.modsFolder";
/// The system property naming the namespace the pre-remapped game uses.
pub const RUNTIME_MAPPING_NAMESPACE_PROPERTY: &str = "fabric.runtimeMappingNamespace";

/// The system property that makes Fabric treat the launch as a development one.
///
/// **Never set this.** It switches the default runtime namespace to `named`,
/// which forces the game jar through a runtime deobfuscation step that writes
/// to disk, and it enables a second remapper for mods. Both are exactly what
/// this crate exists to avoid.
pub const DEVELOPMENT_PROPERTY: &str = "fabric.development";

/// One complete mod jar image, held in memory before the JVM exists.
///
/// The bytes may be the distributed jar or the output of an offline remap. In
/// either case, bringing them into `FabricModImage` before mounting makes the
/// namespace decision explicit and keeps transformed mod bytes off disk.
#[derive(Debug, Clone)]
pub struct FabricModImage {
    /// The name Fabric and diagnostics should see for this jar.
    pub file_name: String,
    /// The complete jar image.
    pub bytes: Vec<u8>,
}

impl FabricModImage {
    /// Read a distributed or pre-remapped mod jar into memory.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the file cannot be read.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, std::io::Error> {
        let path = path.into();
        let bytes = std::fs::read(&path)?;
        let file_name = path.file_name().map_or_else(
            || "mod.jar".to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Ok(Self { file_name, bytes })
    }

    /// Adopt bytes that were produced in this process, such as by TinyRemapper.
    #[must_use]
    pub fn from_memory(file_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            file_name: file_name.into(),
            bytes,
        }
    }
}

/// Mod jars prepared for a disk-free Fabric launch.
///
/// `mods` are explicit Fabric mod candidates. `libraries` are nested jars which
/// do not carry `fabric.mod.json`; they still need a hollow classpath entry, but
/// must not be registered as mods.
#[derive(Debug, Clone, Default)]
pub struct PreparedFabricMods {
    /// Fabric mod candidates, roots first and nested mods in declaration order.
    pub mods: Vec<FabricModImage>,
    /// Nested non-mod libraries, in discovery order.
    pub libraries: Vec<FabricModImage>,
}

/// Loader and mod images for one pre-JVM Fabric launch.
#[derive(Debug, Clone, Default)]
pub struct FabricLaunchImages {
    /// Fabric Loader roots to flatten and mount behind loader placeholders.
    pub loaders: Vec<FabricModImage>,
    /// Fabric mod roots to flatten and register through `fabric.addMods`.
    pub mods: Vec<FabricModImage>,
}
/// Maximum recursive `jars` nesting accepted before launch.
const MAX_NESTED_MOD_DEPTH: usize = 8;

/// Parse and flatten one Fabric mod before the JVM exists.
///
/// Fabric normally extracts nested `jars` beneath `.fabric/processedMods`.
/// That would put nested mod bytes on disk. Preparation instead removes each
/// declared nested-jar entry from its parent, removes the now-stale `jars`
/// declaration from `fabric.mod.json`, and mounts every nested Fabric mod as an
/// explicit launch-time mod. Fabric therefore sees a flat, complete mod set via
/// `fabric.addMods` and has nothing to extract.
///
/// Unsupported runtime-injection features are intentionally *not* rejected
/// here: at launch time Fabric itself can apply AccessWideners and
/// accessor/invoker Mixins. Those restrictions belong to the post-launch JVMTI
/// fallback, not to this path.
///
/// # Errors
///
/// Returns [`FabricMountError`] if a mod image, nested jar, or metadata is
/// malformed, or nesting exceeds the launch-time limit.
pub fn prepare_fabric_mod(image: FabricModImage) -> Result<PreparedFabricMods, FabricMountError> {
    let mut prepared = PreparedFabricMods::default();
    let mut mod_names = HashSet::new();
    let mut library_names = HashSet::new();
    prepare_mod_image(image, 0, &mut prepared, &mut mod_names, &mut library_names)?;
    Ok(prepared)
}

fn prepare_mod_images(
    images: impl IntoIterator<Item = FabricModImage>,
) -> Result<PreparedFabricMods, FabricMountError> {
    let mut prepared = PreparedFabricMods::default();
    let mut mod_names = HashSet::new();
    let mut library_names = HashSet::new();
    for image in images {
        prepare_mod_image(image, 0, &mut prepared, &mut mod_names, &mut library_names)?;
    }
    Ok(prepared)
}

fn prepare_mod_image(
    image: FabricModImage,
    depth: usize,
    prepared: &mut PreparedFabricMods,
    mod_names: &mut HashSet<String>,
    library_names: &mut HashSet<String>,
) -> Result<(), FabricMountError> {
    if depth >= MAX_NESTED_MOD_DEPTH {
        return Err(FabricMountError::ModNestingTooDeep {
            name: image.file_name.clone(),
            depth,
        });
    }

    let plan = parse_runtime_mod(&image.bytes).map_err(|source| FabricMountError::Mod {
        name: image.file_name.clone(),
        source,
    })?;
    let nested = plan
        .nested_jars
        .iter()
        .map(|path| {
            read_mod_entry(&image.bytes, path).map(|bytes| {
                let file_name = Path::new(path)
                    .file_name()
                    .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned());
                FabricModImage { file_name, bytes }
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| FabricMountError::Mod {
            name: image.file_name.clone(),
            source,
        })?;

    let root_bytes = strip_nested_jars(&image.bytes, &plan.nested_jars).map_err(|source| {
        FabricMountError::Mod {
            name: image.file_name.clone(),
            source,
        }
    })?;
    let file_name = unique_file_name(&image.file_name, mod_names);
    prepared.mods.push(FabricModImage {
        file_name,
        bytes: root_bytes,
    });

    for image in nested {
        if is_fabric_mod(&image.bytes).map_err(|source| FabricMountError::Mod {
            name: image.file_name.clone(),
            source,
        })? {
            prepare_mod_image(image, depth + 1, prepared, mod_names, library_names)?;
        } else {
            let file_name = unique_file_name(&image.file_name, library_names);
            prepared.libraries.push(FabricModImage {
                file_name,
                bytes: image.bytes,
            });
        }
    }
    Ok(())
}

fn is_fabric_mod(bytes: &[u8]) -> Result<bool, RuntimeModError> {
    match parse_runtime_mod(bytes) {
        Ok(_) => Ok(true),
        Err(RuntimeModError::MissingEntry(entry)) if entry == "fabric.mod.json" => Ok(false),
        Err(source) => Err(source),
    }
}

fn strip_nested_jars(bytes: &[u8], nested: &[String]) -> Result<Vec<u8>, RuntimeModError> {
    let skipped: HashSet<&str> = nested.iter().map(String::as_str).collect();
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(RuntimeModError::Zip)?;
    let mut out = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut out);

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(RuntimeModError::Zip)?;
        let name = entry.name().to_string();
        if skipped.contains(name.as_str()) {
            continue;
        }

        let compression = entry.compression();
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(compression);
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|source| RuntimeModError::Read {
                path: name.clone(),
                source,
            })?;
        if name == "fabric.mod.json" {
            let mut metadata: serde_json::Value =
                serde_json::from_slice(&data).map_err(RuntimeModError::MetadataJson)?;
            if let Some(object) = metadata.as_object_mut() {
                object.remove("jars");
            }
            data = serde_json::to_vec(&metadata).map_err(RuntimeModError::MetadataJson)?;
        }

        if entry.is_dir() {
            writer
                .add_directory(&name, options)
                .map_err(RuntimeModError::Zip)?;
        } else {
            writer
                .start_file(&name, options)
                .map_err(RuntimeModError::Zip)?;
            writer
                .write_all(&data)
                .map_err(|source| RuntimeModError::Read {
                    path: name.clone(),
                    source,
                })?;
        }
    }

    writer.finish().map_err(RuntimeModError::Zip)?;
    Ok(out.into_inner())
}

fn unique_file_name(base: &str, used: &mut HashSet<String>) -> String {
    let original = Path::new(base).file_name().map_or_else(
        || base.to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let sanitized = crate::vfs::sanitize_file_name(&original);
    if used.insert(sanitized.clone()) {
        return sanitized;
    }
    for index in 2.. {
        let stem = sanitized.strip_suffix(".jar").unwrap_or(&sanitized);
        let candidate = format!("{stem}-{index}.jar");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("a bounded integer sequence cannot be exhausted")
}

/// One entry on the launch classpath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClasspathEntry {
    /// A real file on disk. Used for the loader and its dependencies, which
    /// Fabric requires to be visible to its own classloader.
    Real(PathBuf),
    /// A hollow placeholder whose bytes the VFS serves from memory.
    Hollow(PathBuf),
}

impl ClasspathEntry {
    /// The path this entry contributes to `java.class.path`.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Real(path) | Self::Hollow(path) => path,
        }
    }

    /// True when this entry's bytes come from memory.
    #[must_use]
    pub fn is_hollow(&self) -> bool {
        matches!(self, Self::Hollow(_))
    }
}

/// The jars a Fabric launch needs, grouped by what they are.
///
/// Kept as an explicit struct rather than a bag of paths so the ordering rule
/// (loader first, then game and libraries) is expressed once, in
/// [`FabricLayout::classpath`], rather than at each call site.
#[derive(Debug, Clone)]
pub struct FabricLayout {
    /// The Minecraft game jar, already in the `intermediary` namespace.
    pub game_jar: ClasspathEntry,
    /// Mod jars, also already intermediary, with refmaps applied.
    pub mods: Vec<ClasspathEntry>,
    /// Fabric Loader and its own dependencies (ASM, Mixin).
    ///
    /// Real dependencies precede hollow loader images, preserving loader-first
    /// classpath order without preserving nested extraction hints.
    pub loader_jars: Vec<ClasspathEntry>,
    /// Classpath libraries the game needs.
    pub libraries: Vec<ClasspathEntry>,
    /// The real, empty directory Fabric should scan for mods.
    pub mods_folder: PathBuf,
    /// The application's main class.
    pub main_class: String,
    /// Arguments passed to the application's `main`.
    pub arguments: Vec<String>,
}

impl FabricLayout {
    /// The main class a modern Fabric launch starts from.
    pub const KNOT_CLIENT: &'static str = "net.fabricmc.loader.impl.launch.knot.KnotClient";

    /// The classpath in the order Fabric expects it.
    ///
    /// Loader jars come first, and that ordering is load-bearing twice over:
    /// `Knot.init()` reads `java.class.path` and uses it as the game classpath,
    /// so the loader must be present; and `LibClassifier` identifies the loader
    /// jars by comparing paths against `LoaderLibrary.FABRIC_LOADER`'s code
    /// source, so they must be at the positions it expects.
    #[must_use]
    pub fn classpath(&self) -> Vec<ClasspathEntry> {
        // Loader jars are stored as plain paths and wrapped here, so the struct
        // does not carry two representations of the same thing.
        let mut entries: Vec<ClasspathEntry> = self.loader_jars.clone();
        entries.push(self.game_jar.clone());
        entries.extend(self.mods.iter().cloned());
        entries.extend(self.libraries.iter().cloned());
        entries
    }

    /// The character separating classpath entries on this platform.
    #[must_use]
    pub fn classpath_separator() -> char {
        if cfg!(windows) {
            ';'
        } else {
            ':'
        }
    }

    /// Build the `java.class.path` string.
    #[must_use]
    pub fn classpath_string(&self) -> String {
        self.classpath()
            .iter()
            .map(|entry| entry.path().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(&Self::classpath_separator().to_string())
    }

    /// The system properties a Fabric launch requires.
    ///
    /// The runtime namespace is fixed to `intermediary`: the game and mods are
    /// remapped before the JVM exists, so Fabric must not infer `official` or
    /// enter its development-mode remapping path.
    ///
    /// Ordered so the output is stable, which makes a failing launch's
    /// environment reproducible from a log.
    #[must_use]
    pub fn system_properties(&self) -> BTreeMap<String, String> {
        let mut properties = BTreeMap::new();

        // Pin the game jar explicitly. Without it, `MinecraftGameProvider`
        // falls back to scanning the classpath, which works but makes a
        // mis-ordered classpath fail with a confusing "cannot find game jar".
        properties.insert(
            GAME_JAR_PROPERTY.to_string(),
            self.game_jar.path().to_string_lossy().into_owned(),
        );

        properties.insert(
            RUNTIME_MAPPING_NAMESPACE_PROPERTY.to_string(),
            "intermediary".to_string(),
        );

        // Point the directory scan at a real, empty directory. Fabric creates
        // it if missing, and creating it through a hollow path would be a
        // filesystem write this crate does not control.
        properties.insert(
            MODS_FOLDER_PROPERTY.to_string(),
            self.mods_folder.to_string_lossy().into_owned(),
        );

        // Mods come from the explicit list rather than a directory scan, since
        // the jars are memory-backed placeholders with generated names.
        if !self.mods.is_empty() {
            let separator = Self::classpath_separator();
            properties.insert(
                ADD_MODS_PROPERTY.to_string(),
                self.mods
                    .iter()
                    .map(|entry| entry.path().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(&separator.to_string()),
            );
        }

        // `fabric.development` is deliberately absent. See the constant's docs.
        properties
    }
}

/// A mounted Fabric application, ready to launch.
#[derive(Debug)]
pub struct FabricApplication {
    /// The layout describing what to launch.
    pub layout: FabricLayout,
    /// The VFS holding the artifacts, kept alive for the launch.
    pub vfs: VirtualFileSystem,
}

impl FabricApplication {
    /// Mount an already-intermediary game jar and optional mods for Fabric.
    ///
    /// `loader_jars` and `libraries` are taken as already-materialized real
    /// files: the loader must be a real jar (Fabric verifies its own classpath),
    /// and libraries are read by Fabric through paths it resolves itself.
    ///
    /// # Errors
    ///
    /// Returns [`MountError`] if the game jar or a mod cannot be verified,
    /// indexed, or given a placeholder.
    pub fn mount(
        session_root: impl AsRef<Path>,
        game_jar: &Path,
        mods: &[PathBuf],
        loader_jars: Vec<PathBuf>,
        libraries: Vec<PathBuf>,
        arguments: Vec<String>,
    ) -> Result<Self, FabricMountError> {
        let images = mods
            .iter()
            .map(|path| FabricModImage::from_path(path.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| FabricMountError::ModIo {
                name: "mod jar".to_string(),
                source,
            })?;
        Self::mount_mod_images(
            session_root,
            game_jar,
            images,
            loader_jars,
            libraries,
            arguments,
        )
    }

    /// Mount an already-intermediary game jar and in-memory mod images.
    ///
    /// This is the disk-free entry point for mods produced by an offline
    /// remapper: their transformed bytes never need a payload path.
    ///
    /// # Errors
    ///
    /// Returns [`MountError`] if the game jar or a mod cannot be verified,
    /// indexed, or given a placeholder.
    pub fn mount_mod_images(
        session_root: impl AsRef<Path>,
        game_jar: &Path,
        mods: Vec<FabricModImage>,
        loader_jars: Vec<PathBuf>,
        libraries: Vec<PathBuf>,
        arguments: Vec<String>,
    ) -> Result<Self, FabricMountError> {
        let mut vfs = VirtualFileSystem::create(session_root)?;
        let game_name = game_jar.file_name().map_or_else(
            || "game.jar".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        vfs.mount(
            ArtifactRole::Game,
            &crate::vfs::sanitize_file_name(&game_name),
            &crate::vfs::identity_spec(&game_name, game_jar.to_path_buf())?,
            true,
        )?;

        let prepared = prepare_mod_images(mods)?;
        let loader_entries = loader_jars.into_iter().map(ClasspathEntry::Real).collect();
        Self::finish_mount(vfs, prepared, loader_entries, libraries, arguments)
    }

    /// Mount a game image that was remapped in this process.
    ///
    /// The bytes have no payload file by construction; the VFS indexes them and
    /// exposes only its usual zero-length placeholder to Fabric.
    ///
    /// # Errors
    ///
    /// Returns [`MountError`] if the remapped image, or a mod, cannot be
    /// indexed or given a placeholder.
    pub fn mount_remapped(
        session_root: impl AsRef<Path>,
        game_name: &str,
        game_bytes: Vec<u8>,
        mods: &[PathBuf],
        loader_jars: Vec<PathBuf>,
        libraries: Vec<PathBuf>,
        arguments: Vec<String>,
    ) -> Result<Self, FabricMountError> {
        let images = mods
            .iter()
            .map(|path| FabricModImage::from_path(path.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| FabricMountError::ModIo {
                name: "mod jar".to_string(),
                source,
            })?;
        Self::mount_remapped_mod_images(
            session_root,
            game_name,
            game_bytes,
            images,
            loader_jars,
            libraries,
            arguments,
        )
    }

    /// Mount a remapped game image and in-memory, pre-remapped mod images.
    ///
    /// # Errors
    ///
    /// Returns [`MountError`] if an image cannot be prepared, indexed, or given
    /// a placeholder.
    pub fn mount_remapped_mod_images(
        session_root: impl AsRef<Path>,
        game_name: &str,
        game_bytes: Vec<u8>,
        mods: Vec<FabricModImage>,
        loader_jars: Vec<PathBuf>,
        libraries: Vec<PathBuf>,
        arguments: Vec<String>,
    ) -> Result<Self, FabricMountError> {
        let mut vfs = VirtualFileSystem::create(session_root)?;
        vfs.mount_memory(
            ArtifactRole::Game,
            &crate::vfs::sanitize_file_name(game_name),
            game_bytes,
            true,
        )?;

        let prepared = prepare_mod_images(mods)?;
        let loader_entries = loader_jars.into_iter().map(ClasspathEntry::Real).collect();
        Self::finish_mount(vfs, prepared, loader_entries, libraries, arguments)
    }

    /// Mount remapped game/mod images and a memory-backed Fabric Loader image.
    ///
    /// Loader dependencies remain real files and precede the hollow loader root.
    /// The loader image is flattened like a mod image: its root loses nested-jar
    /// metadata and entries, while nested Fabric modules become explicit mods.
    /// This keeps Fabric from extracting its bundled modules to
    /// `.fabric/processedMods` during a normal launch.
    ///
    /// # Errors
    ///
    /// Returns [`FabricMountError`] if any image cannot be prepared, indexed, or
    /// given a zero-byte placeholder.
    pub fn mount_remapped_launch_images(
        session_root: impl AsRef<Path>,
        game_name: &str,
        game_bytes: Vec<u8>,
        images: FabricLaunchImages,
        loader_jars: Vec<PathBuf>,
        libraries: Vec<PathBuf>,
        arguments: Vec<String>,
    ) -> Result<Self, FabricMountError> {
        let mut vfs = VirtualFileSystem::create(session_root)?;
        vfs.mount_memory(
            ArtifactRole::Game,
            &crate::vfs::sanitize_file_name(game_name),
            game_bytes,
            true,
        )?;

        let FabricLaunchImages {
            loaders: loader_images,
            mods,
        } = images;
        let mut prepared = prepare_mod_images(mods)?;
        let mut loader_entries = loader_jars
            .into_iter()
            .map(ClasspathEntry::Real)
            .collect::<Vec<_>>();

        for loader_image in loader_images {
            let loader_prepared = prepare_fabric_mod(loader_image)?;
            let mut roots = loader_prepared.mods;
            let root = roots.remove(0);
            prepared.mods.extend(roots);
            prepared.libraries.extend(loader_prepared.libraries);

            vfs.mount_memory(
                ArtifactRole::Library,
                &crate::vfs::sanitize_file_name(&root.file_name),
                root.bytes,
                true,
            )?;
            let mounted = vfs
                .with_role(ArtifactRole::Library)
                .last()
                .ok_or_else(|| FabricMountError::Internal("loader vanished after mount".into()))?;
            loader_entries.push(ClasspathEntry::Hollow(mounted.path().to_path_buf()));
        }

        Self::finish_mount(vfs, prepared, loader_entries, libraries, arguments)
    }
    fn finish_mount(
        mut vfs: VirtualFileSystem,
        mods: PreparedFabricMods,
        loader_jars: Vec<ClasspathEntry>,
        libraries: Vec<PathBuf>,
        arguments: Vec<String>,
    ) -> Result<Self, FabricMountError> {
        let mut mod_entries = Vec::with_capacity(mods.mods.len());
        for mod_image in mods.mods {
            vfs.mount_memory(
                ArtifactRole::Mod,
                &crate::vfs::sanitize_file_name(&mod_image.file_name),
                mod_image.bytes,
                true,
            )?;
            let mounted = vfs
                .with_role(ArtifactRole::Mod)
                .last()
                .ok_or_else(|| FabricMountError::Internal("mod vanished after mount".into()))?;
            mod_entries.push(ClasspathEntry::Hollow(mounted.path().to_path_buf()));
        }

        let mut library_entries = libraries
            .into_iter()
            .map(ClasspathEntry::Real)
            .collect::<Vec<_>>();
        for library in mods.libraries {
            vfs.mount_memory(
                ArtifactRole::Library,
                &crate::vfs::sanitize_file_name(&library.file_name),
                library.bytes,
                true,
            )?;
            let mounted = vfs
                .with_role(ArtifactRole::Library)
                .last()
                .ok_or_else(|| FabricMountError::Internal("library vanished after mount".into()))?;
            library_entries.push(ClasspathEntry::Hollow(mounted.path().to_path_buf()));
        }

        let game_mounted = vfs
            .with_role(ArtifactRole::Game)
            .last()
            .ok_or_else(|| FabricMountError::Internal("game jar vanished after mount".into()))?;
        let game_entry = ClasspathEntry::Hollow(game_mounted.path().to_path_buf());

        let mods_folder = vfs.create_mods_directory()?.path().to_path_buf();

        Ok(Self {
            layout: FabricLayout {
                game_jar: game_entry,
                mods: mod_entries,
                loader_jars,
                libraries: library_entries,
                mods_folder,
                main_class: FabricLayout::KNOT_CLIENT.to_string(),
                arguments,
            },
            vfs,
        })
    }
    /// The JVM arguments that set the launch up.
    ///
    /// Returned as `-D` options rather than as a map so the caller can append
    /// them to an `InitArgsBuilder` without re-implementing the formatting.
    #[must_use]
    pub fn jvm_options(&self) -> Vec<String> {
        self.layout
            .system_properties()
            .into_iter()
            .map(|(key, value)| format!("-D{key}={value}"))
            .collect()
    }
}

/// Errors from preparing a Fabric application.
#[derive(Debug, thiserror::Error)]
pub enum FabricMountError {
    #[error(transparent)]
    Path(#[from] crate::vfs::pathkey::PathError),

    #[error(transparent)]
    Mount(#[from] MountError),

    #[error(transparent)]
    Artifact(#[from] crate::artifact::ArtifactError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("cannot prepare Fabric mod {name}: {source}")]
    Mod {
        name: String,
        #[source]
        source: RuntimeModError,
    },

    #[error("cannot read Fabric mod {name}: {source}")]
    ModIo {
        name: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Fabric mod {name} nests other mods more than {depth} levels deep")]
    ModNestingTooDeep { name: String, depth: usize },

    #[error("internal error: {0}")]
    Internal(String),
}

/// True when a jar looks like a Fabric Loader jar.
///
/// Used to sort a directory of jars into "loader" and "everything else" without
/// asking the user, by checking for the one class Fabric's own verification
/// requires to be unique.
#[must_use]
pub fn is_loader_jar(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    // `archive` borrows `file`, so the lookup must be the last thing that uses
    // it rather than being returned from the block that owns the borrow.
    let found = archive
        .by_name("net/fabricmc/loader/api/FabricLoader.class")
        .is_ok();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jar(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create jar");
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            zw.start_file(*name, opts).expect("start");
            zw.write_all(data).expect("write");
        }
        zw.finish().expect("finish");
    }

    fn jar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut out);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(*name, options).expect("start in memory");
            writer.write_all(data).expect("write in memory");
        }
        writer.finish().expect("finish in memory");
        out.into_inner()
    }

    fn entry_bytes(jar: &[u8], name: &str) -> Option<Vec<u8>> {
        let mut archive = zip::ZipArchive::new(Cursor::new(jar)).expect("open in-memory jar");
        let mut entry = archive.by_name(name).ok()?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).expect("read in-memory entry");
        Some(bytes)
    }

    /// A minimal game jar: the entry point Fabric looks for, plus a version.
    fn write_game_jar(path: &Path) {
        write_jar(
            path,
            &[
                ("net/minecraft/client/main/Main.class", b"\xca\xfe\xba\xbe"),
                ("version.json", br#"{"id":"1.21.4","name":"1.21.4"}"#),
                ("assets/.mcassetsroot", b""),
            ],
        );
    }

    #[test]
    fn launch_preparation_flattens_nested_mods_and_removes_extraction_hints() {
        let nested = jar_bytes(&[
            ("fabric.mod.json", br#"{"id":"nested","version":"1.0.0"}"#),
            ("example/Nested.class", b"nested class"),
        ]);
        let root = jar_bytes(&[
            (
                "fabric.mod.json",
                br#"{"id":"root","version":"1.0.0","jars":[{"file":"META-INF/jars/nested.jar"}]}"#,
            ),
            ("example/Root.class", b"root class"),
            ("META-INF/jars/nested.jar", nested.as_slice()),
        ]);
        let image = FabricModImage::from_memory("root.jar", root);

        let prepared = prepare_fabric_mod(image).expect("prepare launch mod");

        assert_eq!(
            prepared.mods.len(),
            2,
            "the root and nested Fabric mod must be explicit"
        );
        assert!(
            prepared.libraries.is_empty(),
            "a Fabric mod is not a plain library"
        );
        assert_eq!(prepared.mods[0].file_name, "root.jar");
        assert_eq!(prepared.mods[1].file_name, "nested.jar");

        let metadata = entry_bytes(&prepared.mods[0].bytes, "fabric.mod.json")
            .expect("root metadata survives preparation");
        let metadata: serde_json::Value = serde_json::from_slice(&metadata).expect("metadata JSON");
        assert!(
            metadata.get("jars").is_none(),
            "Fabric must not be invited to extract the already-flattened nested jar"
        );
        assert!(
            entry_bytes(&prepared.mods[0].bytes, "META-INF/jars/nested.jar").is_none(),
            "the nested jar bytes must not remain inside the parent image"
        );
        assert_eq!(
            prepared.mods[1].bytes, nested,
            "nested mod bytes must be moved, not transformed or dropped"
        );
    }

    #[test]
    fn launch_time_mod_images_are_hollow_and_registered_before_the_jvm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);
        let nested = jar_bytes(&[
            ("fabric.mod.json", br#"{"id":"nested","version":"1.0.0"}"#),
            ("example/Nested.class", b"nested class"),
        ]);
        let root = jar_bytes(&[
            (
                "fabric.mod.json",
                br#"{"id":"root","version":"1.0.0","jars":[{"file":"META-INF/jars/nested.jar"}]}"#,
            ),
            ("META-INF/jars/nested.jar", nested.as_slice()),
        ]);

        let app = FabricApplication::mount_mod_images(
            dir.path().join("session"),
            &game,
            vec![FabricModImage::from_memory("root.jar", root)],
            vec![],
            vec![],
            vec![],
        )
        .expect("mount launch-time mods");

        assert_eq!(app.layout.mods.len(), 2);
        assert!(app.layout.mods.iter().all(ClasspathEntry::is_hollow));
        let properties = app.layout.system_properties();
        assert_eq!(
            properties
                .get(RUNTIME_MAPPING_NAMESPACE_PROPERTY)
                .map(String::as_str),
            Some("intermediary"),
            "the launch is already in intermediary namespace"
        );
        let add_mods = properties
            .get(ADD_MODS_PROPERTY)
            .expect("Fabric explicit mod list");
        for entry in &app.layout.mods {
            assert!(
                add_mods.contains(entry.path().to_string_lossy().as_ref()),
                "Fabric must discover {} before the JVM starts",
                entry.path().display()
            );
            assert_eq!(
                std::fs::metadata(entry.path())
                    .expect("stat mod placeholder")
                    .len(),
                0,
                "launch-time mod bytes must not reach disk"
            );
        }
    }

    #[test]
    fn loader_jars_come_first_on_the_classpath() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);
        let loader = dir.path().join("fabric-loader.jar");
        std::fs::write(&loader, b"loader").expect("write");

        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            &[],
            vec![loader.clone()],
            vec![],
            vec![],
        )
        .expect("mount");

        let classpath = app.layout.classpath_string();
        let loader_at = classpath.find("fabric-loader.jar").expect("loader present");
        let game_at = classpath.find("game").expect("game present");
        assert!(
            loader_at < game_at,
            "the loader must precede the game jar, got: {classpath}"
        );
    }

    #[test]
    fn the_game_jar_is_hollow_and_the_loader_is_real() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);
        let loader = dir.path().join("fabric-loader.jar");
        std::fs::write(&loader, b"loader").expect("write");

        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            &[],
            vec![loader],
            vec![],
            vec![],
        )
        .expect("mount");

        assert!(app.layout.game_jar.is_hollow());
        assert_eq!(
            std::fs::metadata(app.layout.game_jar.path())
                .expect("stat")
                .len(),
            0,
            "the game jar's placeholder must be empty on disk"
        );
        for entry in &app.layout.loader_jars {
            let ClasspathEntry::Real(path) = entry else {
                panic!("plain mount keeps loader jars real: {entry:?}");
            };
            assert!(path.is_file(), "loader jars stay real: {path:?}");
        }
    }

    #[test]
    fn the_game_jar_property_points_at_the_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);

        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            &[],
            vec![],
            vec![],
            vec![],
        )
        .expect("mount");

        let properties = app.layout.system_properties();
        assert_eq!(
            properties.get(GAME_JAR_PROPERTY).map(String::as_str),
            Some(app.layout.game_jar.path().to_string_lossy().as_ref())
        );
    }

    /// The property that would silently turn on a disk-writing remapper.
    #[test]
    fn development_mode_is_never_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);

        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            &[],
            vec![],
            vec![],
            vec![],
        )
        .expect("mount");

        assert!(
            !app.layout
                .system_properties()
                .contains_key(DEVELOPMENT_PROPERTY),
            "fabric.development must never be set: it forces a runtime remap that writes to disk"
        );
    }

    #[test]
    fn the_mods_folder_is_a_real_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);

        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            &[],
            vec![],
            vec![],
            vec![],
        )
        .expect("mount");

        assert!(app.layout.mods_folder.is_dir());
        assert_eq!(
            app.layout.system_properties().get(MODS_FOLDER_PROPERTY),
            Some(&app.layout.mods_folder.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn mods_are_listed_explicitly_and_hollow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);
        let mod_jar = dir.path().join("sodium.jar");
        write_jar(
            &mod_jar,
            &[("fabric.mod.json", br#"{"id":"sodium","version":"0.6.0"}"#)],
        );

        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            std::slice::from_ref(&mod_jar),
            vec![],
            vec![],
            vec![],
        )
        .expect("mount");

        assert_eq!(app.layout.mods.len(), 1);
        assert!(app.layout.mods[0].is_hollow());
        let property = app
            .layout
            .system_properties()
            .get(ADD_MODS_PROPERTY)
            .cloned()
            .expect("addMods set");
        assert!(property.contains("sodium"), "got: {property}");
    }

    #[test]
    fn a_mod_that_is_not_an_archive_is_rejected_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);
        let broken = dir.path().join("broken-mod.jar");
        std::fs::write(&broken, b"not a zip").expect("write");

        let error = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            std::slice::from_ref(&broken),
            vec![],
            vec![],
            vec![],
        )
        .expect_err("must reject");

        // The error must name the offending file, since a launch can involve
        // dozens of mods and "some jar is corrupt" is not actionable.
        let text = error.to_string();
        assert!(
            text.contains("broken-mod.jar"),
            "the error must name the unreadable mod, got: {text}"
        );
    }

    #[test]
    fn the_knot_main_class_is_what_a_modern_launch_uses() {
        assert_eq!(
            FabricLayout::KNOT_CLIENT,
            "net.fabricmc.loader.impl.launch.knot.KnotClient"
        );
    }

    #[test]
    fn system_properties_are_ordered_for_reproducible_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);
        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            &[],
            vec![],
            vec![],
            vec![],
        )
        .expect("mount");

        let keys: Vec<String> = app.layout.system_properties().into_keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "BTreeMap keeps the output stable");
    }

    #[test]
    fn jvm_options_are_rendered_as_minus_d_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let game = dir.path().join("game.jar");
        write_game_jar(&game);
        let app = FabricApplication::mount(
            dir.path().join("session"),
            &game,
            &[],
            vec![],
            vec![],
            vec![],
        )
        .expect("mount");

        let options = app.jvm_options();
        assert!(options.iter().all(|option| option.starts_with("-D")));
        assert!(options
            .iter()
            .any(|option| option.starts_with(&format!("-D{GAME_JAR_PROPERTY}="))));
    }

    #[test]
    fn a_jar_with_the_loader_api_class_is_recognized_as_a_loader() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = dir.path().join("loader.jar");
        write_jar(
            &loader,
            &[(
                "net/fabricmc/loader/api/FabricLoader.class",
                b"\xca\xfe\xba\xbe",
            )],
        );
        let other = dir.path().join("mod.jar");
        write_jar(&other, &[("fabric.mod.json", b"{}")]);

        assert!(is_loader_jar(&loader));
        assert!(!is_loader_jar(&other));
        assert!(!is_loader_jar(&dir.path().join("nonexistent.jar")));
    }
}
