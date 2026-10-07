use super::{SkillId, Slot};

/// A cooldown tracked by the game's cooldown container.
///
/// Unlike an [`Ability`](crate::context::Ability) bound to a skillbar slot, this
/// may describe a skill that is not currently on the player's skillbar — e.g. the
/// inactive weapon set's skills, or a kit / elite-spec skill cooling down
/// off-bar. Recovered from the combatant cooldown container (same walk as the
/// skillbar cooldowns), referenced from `sub_1800E2EC0` / `sub_1800E1BB0` in
/// `Reffect.dll` 0.13.1.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveCooldown {
    /// The cooling skill.
    pub skill: SkillId,

    /// The slot the skill is bound to, or `None` when it is not on the current
    /// bar.
    pub slot: Option<Slot>,

    /// Last update timestamp (ms).
    pub last_update: u32,

    /// Recharge rate.
    pub recharge_rate: f32,

    /// Total recharge in ms (0 when unknown).
    pub recharge: u32,

    /// Remaining recharge in ms.
    pub recharge_remaining: u32,

    /// Ammunition count.
    pub ammo: u32,

    /// Total ammo recharge in ms.
    pub ammo_recharge: u32,

    /// Remaining ammo recharge in ms.
    pub ammo_recharge_remaining: u32,
}

impl ActiveCooldown {
    /// Returns the remaining recharge adjusted for the time passed since
    /// [`Self::last_update`].
    #[inline]
    pub fn recharge_remaining(&self, now: u32) -> u32 {
        self.recharge_remaining.saturating_sub(self.passed(now))
    }

    /// Returns the remaining ammo recharge adjusted for the time passed.
    #[inline]
    pub fn ammo_recharge_remaining(&self, now: u32) -> u32 {
        self.ammo_recharge_remaining.saturating_sub(self.passed(now))
    }

    #[inline]
    fn passed(&self, now: u32) -> u32 {
        (now.saturating_sub(self.last_update) as f32 * self.recharge_rate) as u32
    }
}
