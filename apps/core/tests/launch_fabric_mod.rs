//! Real-Mod Menu validation of the pre-JVM Fabric mod preparation path.
#![cfg(windows)]

use jvmsense_core::launch::{prepare_fabric_mod, FabricModImage};
use jvmsense_core::runtime::parse_runtime_mod;

mod support;

#[test]
fn modmenu_is_flattened_for_pre_jvm_fabric_discovery() {
    let jar = support::ensure_modmenu().expect("obtain Mod Menu");
    let bytes = std::fs::read(jar).expect("read Mod Menu");

    let prepared = prepare_fabric_mod(FabricModImage::from_memory("modmenu-13.0.4.jar", bytes))
        .expect("prepare Mod Menu before JVM creation");

    assert!(
        prepared.mods.len() > 1,
        "Mod Menu's declared nested Fabric mods must become explicit launch-time mods"
    );
    assert!(
        prepared.libraries.is_empty(),
        "Mod Menu's declared nested jars are Fabric modules, not plain libraries"
    );

    let root = parse_runtime_mod(&prepared.mods[0].bytes).expect("reparse prepared Mod Menu");
    assert_eq!(root.id, "modmenu");
    assert!(
        root.nested_jars.is_empty(),
        "Fabric must have no reason to extract nested jars after launch starts"
    );

    for image in &prepared.mods {
        let plan = parse_runtime_mod(&image.bytes)
            .unwrap_or_else(|error| panic!("reparse {}: {error}", image.file_name));
        assert!(
            plan.nested_jars.is_empty(),
            "{} still asks Fabric to extract nested jars",
            image.file_name
        );
    }
}
