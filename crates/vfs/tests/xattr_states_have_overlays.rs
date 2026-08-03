//! The Dolphin overlay plugin maps `user.onedrive.syncstate` values to emblem
//! names. The two sides live in different languages and build systems, so
//! nothing forces them to agree — and a value the plugin does not recognise
//! renders no emblem at all, which looks exactly like the plugin not being
//! installed. This test fails loudly instead.

use std::path::Path;

/// Every state string the FUSE layer can serve for `user.onedrive.syncstate`.
/// Keep in step with `getxattr` in `src/filesystem.rs`.
const SERVED_STATES: &[&str] = &[
    "synced", "syncing", "cloud", "error", "local", "conflict", "pinned", "partial",
];

fn plugin_source() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/dolphin/onedrive-overlay.cpp");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read overlay plugin at {}: {e}", path.display()))
}

#[test]
fn every_served_state_has_an_overlay_mapping() {
    let source = plugin_source();
    let missing: Vec<_> = SERVED_STATES
        .iter()
        .filter(|state| !source.contains(&format!("QLatin1String(\"{state}\")")))
        .collect();
    assert!(
        missing.is_empty(),
        "the FUSE layer serves these sync states but overlaysForState() in \
         extensions/dolphin/onedrive-overlay.cpp does not map them, so files in \
         those states show no emblem: {missing:?}"
    );
}

#[test]
fn filesystem_still_serves_every_state_the_test_claims() {
    // Guards the list above against drifting from the implementation.
    let fs_source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/filesystem.rs"))
            .expect("read filesystem.rs");
    for state in SERVED_STATES {
        assert!(
            fs_source.contains(&format!("\"{state}\"")),
            "SERVED_STATES lists {state:?}, but filesystem.rs never produces it"
        );
    }
}

#[test]
fn custom_emblems_referenced_by_the_plugin_are_shipped() {
    let source = plugin_source();
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icons");
    for emblem in ["onedrive-cloud", "onedrive-partial", "onedrive-upload"] {
        if source.contains(emblem) {
            let icon = assets.join(format!("{emblem}.svg"));
            assert!(
                icon.exists(),
                "the plugin draws {emblem:?} but {} is missing — the emblem would \
                 silently not render",
                icon.display()
            );
        }
    }
}
