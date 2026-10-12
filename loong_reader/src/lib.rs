//! 龙之读取器（Loong Reader）
//!
//! 独立的内存读取插件：读取《激战2》客户端运行时状态与官方 API 元数据，
//! 并通过 C ABI（loong_snapshot）把角色状态快照交给进程内的其它模块（例如 ACR）。
//!
//! 本插件只保留内存读取、API 元数据和一个调试窗口，
//! 不包含任何显示元素、编辑器或触发器。

mod addon;
mod export;

pub use loong_core::*;
pub use loong_internal as internal;

use addon::Addon;
use nexus::{AddonFlags, UpdateProvider};

/// 本插件在 Nexus 中的唯一签名（负数表示非 Raidcore 官方发布）。
pub const ADDON_SIGNATURE: i32 = -0x4C4F_4F4E;
/// Nexus 中显示的插件名。
pub const ADDON_DISPLAY_NAME: &str = "Loong Reader";
/// 插件数据目录名。
pub const ADDON_DIRECTORY: &str = "loong_reader";

nexus::export! {
    name: "Loong Reader",
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
    fn identity_is_independent() {
        assert_eq!(ADDON_DISPLAY_NAME, "Loong Reader");
        assert_eq!(ADDON_DIRECTORY, "loong_reader");
        assert!(ADDON_SIGNATURE < 0);
    }

    #[test]
    fn build_does_not_auto_update() {
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
