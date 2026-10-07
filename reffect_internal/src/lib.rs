//! Open implementation of Reffect's internal metadata interface.
//!
//! The original private crate reads Guild Wars 2's runtime content tables. This
//! implementation combines recovered runtime tables with the public API for
//! skill and sigil metadata, and keeps a small built-in catalogue for standard
//! boons and conditions whose game effect ids are not exposed through the API.

mod catalog;
pub mod game;

use reffect_core::{
    Texture, Ui,
    context::{ItemInfo, SkillInfo},
    error::Result,
};

pub use reffect_core::Interface;

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
