//! Loong Reader 的内部元数据接口实现。
//!
//! 原始私有实现读取《激战2》的运行时内容表。本实现把恢复出来的运行时表与
//! 官方 API 的技能/法印元数据结合起来，并为游戏效果 id 未通过 API 暴露的
//! 标准增益与症状保留一张内置表。

mod catalog;
pub mod game;

use loong_core::{
    Texture, Ui,
    context::{ItemInfo, SkillInfo},
    error::Result,
};

pub use loong_core::Interface;

/// Open replacement for the previously private internal API.
pub type Internal = OpenInternal;

/// Internal metadata provider backed by Guild Wars 2's public API.
#[derive(Debug)]
pub struct OpenInternal;

impl Interface for OpenInternal {
    fn init() {
        game::state::init();
        catalog::init();
    }

    fn deinit() {
        catalog::deinit();
        game::state::deinit();
    }

    fn get_item_info(id: u32) -> Result<ItemInfo> {
        catalog::get_item_info(id)
    }

    fn get_item_from_buff(id: u32) -> Result<u32> {
        catalog::get_item_from_buff(id)
    }

    fn get_skill_info(id: u32) -> Result<SkillInfo> {
        catalog::get_skill_info(id)
    }

    fn get_skill_icon(_ui: &Ui, id: u32) -> Option<Texture> {
        catalog::get_skill_icon(id)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn installs_capture_before_starting_metadata_workers() {
        let source = include_str!("lib.rs");
        let implementation = source
            .split_once("impl Interface for OpenInternal {")
            .expect("OpenInternal implementation")
            .1;
        let init = implementation
            .split_once("fn init() {")
            .and_then(|(_, rest)| rest.split_once("    }\n\n    fn deinit"))
            .expect("init body")
            .0;
        let deinit = implementation
            .split_once("fn deinit() {")
            .and_then(|(_, rest)| rest.split_once("    }\n\n    fn get_item_info"))
            .expect("deinit body")
            .0;

        assert!(init.find("game::state::init();") < init.find("catalog::init();"));
        assert!(deinit.find("catalog::deinit();") < deinit.find("game::state::deinit();"));
    }
}
