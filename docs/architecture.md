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
create 0-byte placeholders and classpath entries
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

This is why Fabric's worker threads can read memory-backed jars instead of seeing empty placeholders.

## Correctness invariants

The launch-time path must preserve all of the following:

1. Every on-disk artifact placeholder is exactly 0 bytes.
2. No complete game, loader, mod, or library jar is written to disk.
3. Fabric Loader and Mod Menu are accepted from in-memory images.
4. The target class has Mixin-added members the first time Java resolves it.
5. No nested mod is extracted under `.fabric/processedMods`.
6. `fabric.development` is not enabled.
7. The launch path does not call JVMTI `RedefineClasses`.

The authoritative test is `modmenu_is_mixed_in_at_target_class_load_time` in `apps/core/tests/launch_fabric_mixin.rs`.

## JVMTI policy

Controlling JVM creation means jvmsense can put mods on the classpath and in Fabric's explicit mod list before class loading begins. That is the better solution for Mixin: it uses Fabric's own transformation lifecycle and avoids the state hazards of replacing classes after initialization.

JVMTI redefine remains useful only when a target class was already loaded before jvmsense could intervene. In that case it is an explicit fallback, not the default launch mechanism.
