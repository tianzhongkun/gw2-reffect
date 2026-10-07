use crate::{
    context::{Affinity, CombatantResources, TargetCategory, skill::BuffMap},
    error::Error,
};
use enumflags2::{BitFlags, make_bitflags};

/// Target info.
#[derive(Debug, Clone)]
pub struct TargetInfo {
    /// Affinity.
    pub affinity: Result<Affinity, Error>,

    /// Classification of the target.
    ///
    /// Contains one flag per axis that could be determined (entity kind,
    /// disposition, NPC rank, gathering discipline). An axis the reader could
    /// not resolve contributes no flag, so a partially classified target still
    /// matches filters on the axes that are known.
    pub category: Result<BitFlags<TargetCategory>, Error>,

    /// Generic combatant resources.
    pub resources: Result<CombatantResources, Error>,

    /// Target buffs.
    pub buffs: Result<BuffMap, Error>,
}

impl TargetInfo {
    #[inline]
    pub const fn disabled() -> Self {
        Self {
            affinity: Err(Error::Disabled),
            category: Err(Error::Disabled),
            resources: Err(Error::Disabled),
            buffs: Err(Error::Disabled),
        }
    }

    #[inline]
    pub const fn empty() -> Self {
        Self {
            affinity: Ok(Affinity::Hostile),
            category: Ok(make_bitflags!(TargetCategory::{
                Monster | Hostile | Normal
            })),
            resources: Ok(CombatantResources::empty()),
            buffs: Ok(BuffMap::new()),
        }
    }

    #[inline]
    pub fn set_error(&mut self, error: Error) {
        self.affinity = Err(error.clone());
        self.category = Err(error.clone());
        self.resources = Err(error.clone());
        self.buffs = Err(error);
    }
}
