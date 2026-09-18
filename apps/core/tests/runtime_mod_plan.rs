//! Real-Mod Menu validation for the in-memory runtime mod plan.

#![cfg(windows)]

use jvmsense_core::runtime::{parse_runtime_mod, UnsupportedFeatureKind};

mod support;

#[test]
fn modmenu_plan_keeps_metadata_and_reports_unsupported_accessor() {
    let jar = support::ensure_modmenu().expect("obtain Mod Menu");
    let bytes = std::fs::read(jar).expect("read Mod Menu");
    let plan = parse_runtime_mod(&bytes).expect("parse Mod Menu");

    assert_eq!(plan.id, "modmenu");
    assert_eq!(plan.version, "13.0.4");
    assert_eq!(
        plan.client_entrypoints
            .iter()
            .map(|entry| entry.value.as_str())
            .collect::<Vec<_>>(),
        ["com.terraformersmc.modmenu.ModMenu"]
    );
    assert_eq!(plan.mixin_configs.len(), 1);
    assert!(plan
        .nested_jars
        .iter()
        .any(|path| path == "META-INF/jars/fabric-screen-api-v1-2.0.38+7feeb73304.jar"));
    assert!(plan.mixin_configs[0]
        .client
        .iter()
        .any(|class| class == "com.terraformersmc.modmenu.mixin.MixinTitleScreen"));
    assert!(plan.unsupported.iter().any(|feature| {
        feature.kind == UnsupportedFeatureKind::AccessorMixin
            && feature.path == "com/terraformersmc/modmenu/mixin/AccessorGridWidget.class"
    }));
}
