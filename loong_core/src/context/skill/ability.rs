use super::SkillId;
use crate::{
    colors::{Color, Colored},
    named::Named,
};
use enumflags2::{BitFlags, bitflags};
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumIter, IntoStaticStr, VariantArray};

/// Ability.
#[derive(Debug, Clone)]
pub struct Ability {
    /// Ability identifier.
    pub id: SkillId,

    /// Ammunition count.
    pub ammo: u32,

    /// Last update timestamp.
    pub last_update: u32,

    /// Recharge rate.
    pub recharge_rate: f32,

    /// Total recharge in milliseconds.
    pub recharge: u32,

    /// Remaining recharge in milliseconds.
    pub recharge_remaining: u32,

    /// Total ammo recharge in milliseconds.
    pub ammo_recharge: u32,

    /// Remining ammo recharge in milliseconds.
    pub ammo_recharge_remaining: u32,

    /// Ability information.
    pub info: BitFlags<AbilityInfo>,
}

impl Ability {
    /// Creates a new ability without cooldowns.
    #[inline]
    pub fn new(id: impl Into<SkillId>, ammo: u32, last_update: u32, recharge_rate: f32) -> Self {
        Self {
            id: id.into(),
            ammo,
            last_update,
            recharge_rate,
            recharge: 0,
            recharge_remaining: 0,
            ammo_recharge: 0,
            ammo_recharge_remaining: 0,
            info: BitFlags::empty(),
        }
    }

    /// Creates a new ability with simple recharge.
    #[inline]
    pub fn simple(id: impl Into<SkillId>, last_update: u32, recharge: u32) -> Self {
        Self {
            id: id.into(),
            ammo: if recharge == 0 { 1 } else { 0 },
            last_update,
            recharge_rate: 1.0,
            recharge,
            recharge_remaining: recharge,
            ammo_recharge: 0,
            ammo_recharge_remaining: 0,
            info: BitFlags::empty(),
        }
    }

    /// Returns the scaled amount of time passed since the last update.
    #[inline]
    pub fn passed(&self, now: u32) -> u32 {
        let passed = now.saturating_sub(self.last_update);
        (passed as f32 * self.recharge_rate) as u32
    }

    /// Returns the remaining recharge.
    #[inline]
    pub fn recharge_remaining(&self, now: u32) -> u32 {
        self.recharge_remaining.saturating_sub(self.passed(now))
    }

    /// Returns the end time for the recharge.
    #[inline]
    pub fn recharge_end(&self) -> u32 {
        self.last_update + (self.recharge_remaining as f32 / self.recharge_rate) as u32
    }

    /// Returns the remaining fraction of the recharge (`1.0` right after the
    /// cast, `0.0` when ready).
    #[inline]
    pub fn recharge_progress(&self, now: u32) -> f32 {
        Self::progress(self.recharge_remaining(now), self.recharge)
    }

    /// Returns the remaining ammo recharge.
    #[inline]
    pub fn ammo_recharge_remaining(&self, now: u32) -> u32 {
        self.ammo_recharge_remaining
            .saturating_sub(self.passed(now))
    }

    /// Returns the remaining fraction of the ammo recharge (`1.0` right after
    /// the cast, `0.0` when ready).
    #[inline]
    pub fn ammo_recharge_progress(&self, now: u32) -> f32 {
        Self::progress(self.ammo_recharge_remaining(now), self.ammo_recharge)
    }

    /// Returns `remaining / total`, `0.0` when the total is unknown.
    #[inline]
    fn progress(remaining: u32, total: u32) -> f32 {
        if total > 0 {
            remaining.min(total) as f32 / total as f32
        } else {
            0.0
        }
    }
}

/// Ability information flags.
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
#[repr(u16)]
#[bitflags]
pub enum AbilityInfo {
    /// Ability is auto-attack.
    #[strum(serialize = "Auto Attack")]
    AutoAttack = 1 << 0,

    /// Ability is queued or casting.
    Pending = 1 << 1,

    /// Ability is pressed.
    Pressed = 1 << 2,

    /// Ability is active primary.
    #[strum(serialize = "Active Primary")]
    ActivePrimary = 1 << 3,

    /// Ability is active secondary.
    #[strum(serialize = "Active Secondary")]
    ActiveSecondary = 1 << 4,

    /// Missing resource for ability.
    #[strum(serialize = "No Resources")]
    NoResources = 1 << 5,

    /// Not in range for ability.
    #[strum(serialize = "No Range")]
    NoRange = 1 << 6,

    /// Ground targeted.
    #[strum(serialize = "Ground Targeted")]
    GroundTarget = 1 << 7,

    /// Ignore recharge.
    #[strum(serialize = "Ignore Recharge")]
    IgnoreRecharge = 1 << 8,
}

impl Named for AbilityInfo {
    fn name(&self) -> &'static str {
        self.into()
    }

    fn short_name(&self) -> &'static str {
        match self {
            Self::AutoAttack => "Auto",
            Self::Pressed => "Press",
            Self::Pending => "Pend",
            Self::ActivePrimary => "Act1",
            Self::ActiveSecondary => "Act2",
            Self::NoResources => "NoRes",
            Self::NoRange => "NoRange",
            Self::GroundTarget => "Ground",
            Self::IgnoreRecharge => "NoRech",
        }
    }
}

impl Colored for AbilityInfo {
    fn colored(&self) -> Option<Color> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recharge_progress_uses_the_remaining_recharge_at_now() {
        // The progress has to be derived from the remaining recharge at `now`.
        // Feeding the already elapsed time back into `recharge_remaining` double
        // subtracts it (saturating at zero), which froze the displayed
        // percentage at the initial snapshot value for the whole cooldown.
        let mut ability = Ability::simple(1234, 1_000, 10_000);
        ability.recharge_remaining = 8_000;

        assert!((ability.recharge_progress(1_000) - 0.8).abs() < f32::EPSILON);
        assert!((ability.recharge_progress(6_000) - 0.3).abs() < f32::EPSILON);
        assert!((ability.recharge_progress(11_000) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn ammo_recharge_progress_uses_the_remaining_recharge_at_now() {
        let mut ability = Ability::simple(1234, 1_000, 0);
        ability.ammo_recharge = 4_000;
        ability.ammo_recharge_remaining = 3_000;

        assert!((ability.ammo_recharge_progress(1_000) - 0.75).abs() < f32::EPSILON);
        assert!((ability.ammo_recharge_progress(3_000) - 0.25).abs() < f32::EPSILON);
        assert!((ability.ammo_recharge_progress(5_000) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn progress_without_a_total_is_zero() {
        let ability = Ability::simple(1234, 1_000, 0);

        assert_eq!(ability.recharge_progress(2_000), 0.0);
        assert_eq!(ability.ammo_recharge_progress(2_000), 0.0);
    }
}
