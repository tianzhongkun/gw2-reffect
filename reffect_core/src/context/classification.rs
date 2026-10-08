//! Classification of the current target.
//!
//! Guild Wars 2 describes a non-player character along two independent axes,
//! see the [NPC rank] article on the wiki:
//!
//! - **Rank** — `Ambient` (harmless critter, no level, no portrait border),
//!   `Normal` (no border), `Veteran` (bronze border), `Elite` (silver border),
//!   `Champion` (golden border, what players usually call a "boss"),
//!   `Legendary` (purple starburst border) and `Epic` (the world bosses, drawn
//!   with a lengthened target monitor; called a "prop boss" internally).
//!   There is no "Boss" rank: boss is a colloquialism for `Champion`/`Epic`.
//! - **Disposition** — allied (green name), neutral (yellow name, only fights
//!   back once attacked) or hostile (red name). The wiki keeps this separate
//!   from rank and has no category for it; it is conveyed by the nameplate
//!   colour only.
//!
//! Gathering nodes follow the three gathering disciplines: `Mining` yields ore,
//! `Logging` yields wood and `Harvesting` yields plants. Chests are
//! environmental objects and are distinct from inventory containers.
//!
//! [NPC rank]: https://wiki.guildwars2.com/wiki/NPC_rank

use crate::{
    colors::{self, Color, Colored, rgb},
    named::Named,
};
use enumflags2::{BitFlags, bitflags, make_bitflags};
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumIter, IntoStaticStr, VariantArray};

/// Category of the current target.
///
/// The flags form four independent axes: entity kind, disposition, NPC rank and
/// gathering discipline. A target carries one flag per axis it could be
/// classified on, so an unknown axis simply contributes no flag.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    AsRefStr,
    IntoStaticStr,
    Display,
    EnumIter,
    VariantArray,
    Serialize,
    Deserialize,
)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[repr(u32)]
#[bitflags]
pub enum TargetCategory {
    /// Player controlled character.
    Player = 1 << 0,

    /// Pet of a player controlled character.
    Pet = 1 << 1,

    /// Non-player character that is not hostile.
    #[strum(serialize = "NPC")]
    Npc = 1 << 2,

    /// Hostile or neutral non-player character.
    Monster = 1 << 3,

    /// Gathering node.
    Gatherable = 1 << 4,

    /// Chest.
    Chest = 1 << 5,

    /// Other environmental object.
    Gadget = 1 << 6,

    /// Attack target object: the prop of a world boss or a destructible
    /// structure such as a fort wall. Hostile, but not a creature.
    AttackTarget = 1 << 20,

    /// Allied, green nameplate.
    Friendly = 1 << 7,

    /// Neutral, yellow nameplate. Only fights back once attacked.
    Neutral = 1 << 8,

    /// Hostile, red nameplate.
    Hostile = 1 << 9,

    /// Ambient creature ("critter"), no level and no portrait border.
    Ambient = 1 << 10,

    /// Normal rank, no portrait border.
    Normal = 1 << 11,

    /// Veteran rank, bronze portrait border.
    Veteran = 1 << 12,

    /// Elite rank, silver portrait border.
    Elite = 1 << 13,

    /// Champion rank, golden portrait border with crossed swords. What players
    /// usually call a boss.
    #[serde(alias = "Boss")]
    Champion = 1 << 14,

    /// Legendary rank, purple starburst portrait border with crossed swords.
    Legendary = 1 << 15,

    /// Epic rank, the world bosses. Drawn with a lengthened target monitor and
    /// an expanded portrait.
    #[serde(alias = "Large")]
    #[serde(alias = "WorldBoss")]
    Epic = 1 << 16,

    /// Mining node, yields ore.
    Ore = 1 << 17,

    /// Logging node, yields wood.
    Wood = 1 << 18,

    /// Harvesting node, yields plants.
    #[serde(alias = "Herb")]
    Plant = 1 << 19,
}

impl TargetCategory {
    /// Entity kind axis.
    pub const KIND: BitFlags<Self> = make_bitflags!(
        Self::{Player | Pet | Npc | Monster | Gatherable | Chest | Gadget | AttackTarget}
    );

    /// Disposition axis.
    pub const AFFINITY: BitFlags<Self> = make_bitflags!(Self::{Friendly | Neutral | Hostile});

    /// NPC rank axis.
    pub const RANK: BitFlags<Self> = make_bitflags!(
        Self::{Ambient | Normal | Veteran | Elite | Champion | Legendary | Epic}
    );

    /// Gathering discipline axis.
    pub const NODE: BitFlags<Self> = make_bitflags!(Self::{Ore | Wood | Plant});

    /// All axes, in the order they are evaluated by [`Self::matches`].
    pub const AXES: [BitFlags<Self>; 4] = [Self::KIND, Self::AFFINITY, Self::RANK, Self::NODE];

    /// Checks whether the categories of a target satisfy a filter.
    ///
    /// Within an axis the filter is a disjunction, across axes a conjunction:
    /// `Champion | Legendary` matches either rank, while `Monster | Champion`
    /// only matches champion monsters. Axes without a selection are ignored, so
    /// selecting only `Champion` matches champions of any kind. An empty filter
    /// never matches, which keeps a freshly added trigger inactive.
    #[inline]
    pub fn matches(target: BitFlags<Self>, filter: BitFlags<Self>) -> bool {
        if filter.is_empty() {
            return false;
        }

        Self::AXES.iter().all(|axis| {
            let selected = filter & *axis;
            selected.is_empty() || target.intersects(selected)
        })
    }
}

impl Named for TargetCategory {
    fn name(&self) -> &'static str {
        self.into()
    }

    fn short_name(&self) -> &'static str {
        match self {
            Self::Player => "Player",
            Self::Pet => "Pet",
            Self::Npc => "NPC",
            Self::Monster => "Mob",
            Self::Gatherable => "Node",
            Self::Chest => "Chest",
            Self::Gadget => "Object",
            Self::AttackTarget => "Attack Target",
            Self::Friendly => "Ally",
            Self::Neutral => "Neutral",
            Self::Hostile => "Hostile",
            Self::Ambient => "Ambient",
            Self::Normal => "Normal",
            Self::Veteran => "Veteran",
            Self::Elite => "Elite",
            Self::Champion => "Champion",
            Self::Legendary => "Legendary",
            Self::Epic => "Epic",
            Self::Ore => "Ore",
            Self::Wood => "Wood",
            Self::Plant => "Plant",
        }
    }
}

impl Colored for TargetCategory {
    fn colored(&self) -> Option<Color> {
        match self {
            // Nameplate disposition colours, as drawn by the game.
            Self::Friendly => Some(colors::GREEN),
            Self::Neutral => Some(colors::YELLOW),
            Self::Hostile => Some(colors::RED),

            // Portrait border colours.
            Self::Ambient => Some(colors::GREY),
            Self::Normal => Some(colors::WHITE),
            Self::Veteran => Some(rgb(0.80, 0.50, 0.20)),
            Self::Elite => Some(rgb(0.80, 0.80, 0.85)),
            Self::Champion => Some(rgb(1.00, 0.80, 0.20)),
            Self::Legendary => Some(rgb(0.70, 0.40, 0.95)),
            Self::Epic => Some(colors::ORANGE),

            Self::Player => Some(colors::CYAN),
            Self::Pet => Some(rgb(0.60, 0.85, 0.95)),
            Self::Npc => Some(colors::GREEN),
            Self::Monster => Some(colors::LIGHT_GREY),
            Self::Gatherable => Some(rgb(0.55, 0.85, 0.55)),
            Self::Chest => Some(rgb(1.00, 0.85, 0.35)),
            Self::Gadget => Some(rgb(0.70, 0.70, 0.75)),
            Self::AttackTarget => Some(colors::RED),
            Self::Ore => Some(rgb(0.75, 0.75, 0.80)),
            Self::Wood => Some(rgb(0.65, 0.45, 0.25)),
            Self::Plant => Some(rgb(0.45, 0.85, 0.35)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(categories: BitFlags<TargetCategory>) -> BitFlags<TargetCategory> {
        categories
    }

    #[test]
    fn axes_cover_every_variant_exactly_once() {
        let mut all = BitFlags::empty();
        for axis in TargetCategory::AXES {
            assert!(all.is_empty() || (all & axis).is_empty(), "axes overlap");
            all |= axis;
        }

        for variant in <TargetCategory as VariantArray>::VARIANTS {
            assert!(all.contains(*variant), "{variant} is in no axis");
        }
    }

    #[test]
    fn empty_filter_never_matches() {
        assert!(!TargetCategory::matches(
            flags(make_bitflags!(TargetCategory::{Monster | Hostile | Champion})),
            BitFlags::empty(),
        ));
    }

    #[test]
    fn selection_within_an_axis_is_a_disjunction() {
        let champion = flags(make_bitflags!(TargetCategory::{Monster | Hostile | Champion}));
        let legendary = flags(make_bitflags!(TargetCategory::{Monster | Hostile | Legendary}));

        let filter = flags(make_bitflags!(TargetCategory::{Champion | Legendary}));
        assert!(TargetCategory::matches(champion, filter));
        assert!(TargetCategory::matches(legendary, filter));
    }

    #[test]
    fn selection_across_axes_is_a_conjunction() {
        let champion = flags(make_bitflags!(TargetCategory::{Monster | Hostile | Champion}));
        let player = flags(make_bitflags!(TargetCategory::{Player | Friendly}));

        let filter = flags(make_bitflags!(TargetCategory::{Monster | Champion}));
        assert!(TargetCategory::matches(champion, filter));
        assert!(!TargetCategory::matches(player, filter));
    }

    #[test]
    fn unselected_axes_are_ignored() {
        let friendly_champion = flags(make_bitflags!(
            TargetCategory::{Npc | Friendly | Champion}
        ));

        // Only the rank axis is constrained, so an allied champion matches too.
        assert!(TargetCategory::matches(
            friendly_champion,
            flags(make_bitflags!(TargetCategory::{Champion})),
        ));
        // Constraining the disposition axis excludes it again.
        assert!(!TargetCategory::matches(
            friendly_champion,
            flags(make_bitflags!(TargetCategory::{Champion | Hostile})),
        ));
    }

    #[test]
    fn gathering_disciplines_are_independent_of_rank() {
        let ore = flags(make_bitflags!(TargetCategory::{Gatherable | Ore}));
        assert!(TargetCategory::matches(
            ore,
            flags(make_bitflags!(TargetCategory::{Gatherable | Ore})),
        ));
        assert!(!TargetCategory::matches(
            ore,
            flags(make_bitflags!(TargetCategory::{Gatherable | Wood})),
        ));
    }
}
