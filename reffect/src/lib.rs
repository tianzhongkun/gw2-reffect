mod action;
mod addon;
mod assets;
mod clipboard;
mod export;
mod math;
mod render;
mod texture;

pub mod elements;
pub mod schema;
pub mod settings;
pub mod tree;
pub mod trigger;

pub use reffect_core::*;
pub use reffect_internal as internal;

use addon::Addon;
use nexus::{AddonFlags, UpdateProvider};

/// Identity reserved for this independently loadable, open reader build.
pub const ADDON_SIGNATURE: i32 = -0x4F50_454E;
pub const ADDON_DISPLAY_NAME: &str = "Reffect Open";
pub const ADDON_DIRECTORY: &str = "reffect_open";

nexus::export! {
    name: "Reffect Open",
    signature: ADDON_SIGNATURE,
    flags: AddonFlags::IsVolatile,
    load: Addon::load,
    unload: Addon::unload,
    provider: UpdateProvider::None,
}

#[cfg(test)]
mod tests {
    use super::{ADDON_DIRECTORY, ADDON_DISPLAY_NAME, ADDON_SIGNATURE};

    #[test]
    fn open_build_identity_is_distinct_from_upstream_reffect() {
        assert_ne!(ADDON_SIGNATURE, -188_061_056);
        assert_ne!(ADDON_DISPLAY_NAME, "Reffect (alpha)");
        assert_ne!(ADDON_DIRECTORY, "reffect");
    }

    #[test]
    fn development_build_disables_nexus_auto_updates() {
        let source = include_str!("lib.rs");
        let manifest = source
            .split_once("nexus::export! {")
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map(|(manifest, _)| manifest)
            .expect("addon export manifest must be present");

        assert!(manifest.contains("provider: UpdateProvider::None"));
        assert!(!manifest.contains("update_link:"));
    }
}
