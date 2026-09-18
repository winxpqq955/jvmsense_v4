# jvmsense launch-time Fabric architecture

## Goal

The default Fabric path must complete all mod preparation before the JVM exists. Fabric and Mixin must see the mods during their normal initialization, so target classes are transformed on their first load. JVMTI class redefinition is not part of this path.

## End-to-end flow

```text
caller supplies/remaps images
        |
        v
parse fabric.mod.json
        |
        v
read META-INF/jars/* recursively
        |
        v
flatten Fabric mods and hollow libraries
        |
        v
remove nested jar entries and "jars" metadata
        |
        v
mount bytes in the in-process VFS
        |
        v
register virtual paths and build classpath entries
        |
        v
create JVM with Fabric properties
        |
        v
Fabric/Knot discovers explicit mods
        |
        v
Mixin transforms target classes on first load
```

The important boundary is the JVM creation call. Everything above it happens in Rust and owns the complete image bytes. Everything below it sees ordinary paths and jar files, while native hooks serve the actual bytes from memory.

## Native open/read layer

The Windows native layer is an open/read implementation for virtual paths, not a writer of special disk entries:

- **Path classification.** The VFS answers each normalized path as a virtual regular file, a virtual ancestor directory, or no virtual node. Unknown paths go to the stock JDK/Windows implementation.
- **Synthetic handles.** Read-only opens allocate unique negative handles from a process-local table. Each handle owns its byte buffer and cursor, so independent opens of one artifact cannot share file offsets. Closing removes the table entry and invalidates the Java file descriptor.
- **Legacy I/O.** `java.io.RandomAccessFile` open, read, bulk read, length, seek, pointer, and close are connected to the synthetic handle table. Write modes use the original JDK implementation and therefore receive normal read/write errors.
- **NIO.** `WindowsNativeDispatcher.CreateFile0` serves read-only `OPEN_EXISTING` requests, while NIO read, positional read, seek, size, and close operations use per-handle state. Other access and creation modes pass through.
- **Metadata and traversal.** Attribute, file-information, size, find-first, find-close, final-path, and `File` attribute/length requests are synthesized so `Path.toRealPath()`, `Files`, `JarFile`, and `ZipFile` observe a consistent filesystem shape.
- **Audit.** The session root is scanned recursively after launch; any regular file, empty or not, is a materialization failure.

This keeps Fabric on ordinary Java archive APIs while all registered game, loader, mod, and library bytes remain in process memory.

## Preparation rules

Launch-time preparation is intentionally more capable than the post-launch fallback:

- Fabric Loader roots and mod roots are represented by `FabricModImage`.
- Nested Fabric mods are recursively flattened into explicit mods.
- Nested non-Fabric libraries become hollow classpath entries rather than Fabric mods.
- Parent archives lose both the physical nested-jar entries and their `"jars"` metadata.
- The flattened list is registered through Fabric's explicit `fabric.addMods` property.
- `fabric.runtimeMappingNamespace` is set to `intermediary` because the supplied game and mod images are already remapped to that namespace.
- `fabric.development` remains unset; enabling it would invite Fabric's disk-writing runtime processing path.

AccessWidener, Accessor, and Invoker handling is delegated to Fabric and Mixin during launch. The corresponding restrictions in the runtime module apply only to post-launch JVMTI fallback transformations.

## VFS visibility

The controlling Rust thread has a thread-local VFS scope, but Fabric discovery also uses ForkJoin worker threads. A launch therefore installs a scoped process-wide fallback VFS:

- a thread-local VFS still takes precedence;
- Java worker threads can resolve paths from the active launch VFS;
- the previous process-wide value is restored after the launch closure exits, including unwinds.

This is why Fabric's worker threads can read memory-backed jars without requiring disk files under those paths.

## Correctness invariants

The launch-time path must preserve all of the following:

1. No artifact file exists under any registered virtual path.
2. A recursive session-root audit finds zero regular files.
3. No complete game, loader, mod, or library jar is written to disk.
4. Fabric Loader and Mod Menu are accepted from in-memory images.
5. The target class has Mixin-added members the first time Java resolves it.
6. No nested mod is extracted under `.fabric/processedMods`.
7. `fabric.development` is not enabled.
8. The launch path does not call JVMTI `RedefineClasses`.

The authoritative test is `modmenu_is_mixed_in_at_target_class_load_time` in `apps/core/tests/launch_fabric_mixin.rs`.

## JVMTI policy

Controlling JVM creation means jvmsense can put mods on the classpath and in Fabric's explicit mod list before class loading begins. That is the better solution for Mixin: it uses Fabric's own transformation lifecycle and avoids the state hazards of replacing classes after initialization.

JVMTI redefine remains useful only when a target class was already loaded before jvmsense could intervene. In that case it is an explicit fallback, not the default launch mechanism.
