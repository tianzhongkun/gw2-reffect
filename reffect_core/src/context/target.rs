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

    /// World position of the target.
    pub position: Result<[f32; 3], Error>,

    /// Distance from the player in game units, when both positions are known.
    pub distance: Result<f32, Error>,

    /// Raw gadget type of the target, when the target is a gadget.
    ///
    /// The client distinguishes many object types that all classify as
    /// [`TargetCategory::Gadget`], and which of them are worth attacking depends
    /// on the situation: a destructible or an attack target object always is,
    /// while a gathering node never is and a prop may be. The value is published
    /// raw so a consumer can decide for itself; see the `RF_GADGET_*` constants
    /// in `reffect_snapshot.h`.
    pub gadget_type: Result<u32, Error>,

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
            position: Err(Error::Disabled),
            distance: Err(Error::Disabled),
            gadget_type: Err(Error::Disabled),
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
            position: Ok([0.0; 3]),
            distance: Ok(0.0),
            gadget_type: Err(Error::TargetState),
            resources: Ok(CombatantResources::empty()),
            buffs: Ok(BuffMap::new()),
        }
    }

    #[inline]
    pub fn set_error(&mut self, error: Error) {
        self.affinity = Err(error.clone());
        self.category = Err(error.clone());
        self.position = Err(error.clone());
        self.distance = Err(error.clone());
        self.gadget_type = Err(error.clone());
        self.resources = Err(error.clone());
        self.buffs = Err(error);
    }
}
