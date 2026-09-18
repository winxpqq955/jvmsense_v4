//! Mixin/refmap baking regression using a real intermediary-namespace mod.
//!
//! Mod Menu is a practical fixture: it is distributed against intermediary,
//! its mixin classes reference Minecraft through `modmenu-refmap.json`, and
//! its bytecode remains named even after normal class remapping. This test
//! verifies that the offline TinyRemapper Mixin extension bakes those targets
//! into named bytecode in memory.

#![cfg(windows)]

use std::io::Read as _;
use std::path::Path;

use jvmsense_core::remap::{intermediary_mixin_mod_to_named, TinyRemapRequest};

mod support;

#[test]
fn intermediary_mixin_mod_bakes_refmap_targets_to_named() {
    let mod_jar = support::ensure_modmenu().expect("obtain Mod Menu");
    let mappings = support::ensure_yarn_named_mapping().expect("obtain Yarn mapping");
    let remapper = support::ensure_tiny_remapper().expect("obtain TinyRemapper");
    let jdk = support::test_jdk();
    let work = tempfile::tempdir().expect("tempdir");

    let baked = intermediary_mixin_mod_to_named(&TinyRemapRequest {
        java: &jdk.home().join("bin").join("java.exe"),
        remapper_jar: &remapper,
        mappings: &mappings,
        source_jar: &mod_jar,
        classpath: &[Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples")
            .join("minecraft")
            .join("client.jar")],
        scratch_dir: &work.path().join("remap-helper"),
    })
    .expect("bake Mixin refmap targets");

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&baked)).expect("open baked mod");
    let mut bytes = Vec::new();
    archive
        .by_name("com/terraformersmc/modmenu/mixin/MixinTitleScreen.class")
        .expect("mixin class survived baking")
        .read_to_end(&mut bytes)
        .expect("read mixin class");

    // A named Mixin annotation no longer addresses the intermediary
    // `class_442`; Yarn names the title screen `net.minecraft.client.gui.screen.TitleScreen`.
    // This is a conservative presence check: the transformed annotation must
    // contain the named descriptor, and its intermediary form must be gone.
    assert!(
        bytes
            .windows("net/minecraft/client/gui/screen/TitleScreen".len())
            .any(|window| window == b"net/minecraft/client/gui/screen/TitleScreen"),
        "Mixin annotation targets must be baked to Yarn names"
    );
    assert!(
        !bytes
            .windows("net/minecraft/class_442".len())
            .any(|window| window == b"net/minecraft/class_442"),
        "the intermediary refmap target must not remain in baked bytecode"
    );
}
