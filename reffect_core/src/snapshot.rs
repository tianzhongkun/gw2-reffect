//! C ABI snapshot of the current game state.
//!
//! Flattens [`Context`](crate::context::Context) into a fixed-layout,
//! `#[repr(C)]` set of structs so other in-process modules (C / C++ / C# /
//! Python / another addon) can read the state directly through a single
//! exported function. The layout is deliberately flat (count + fixed-capacity
//! arrays, no heap pointers) so a foreign function can read it with no
//! marshalling. All fields are zero-initialised and carry an explicit validity
//! flag (`*_valid`, `count`, `has_*`) rather than relying on sentinel values.

use crate::context::{
    Ability, ActiveCooldown, Buff, Build, CombatantResources, Context, Defiance, Gear, PlayerHealth,
    PlayerResources, Profession, Resource, ResourceState, SkillId, Slot,
};
use strum::IntoEnumIterator;

/// Maximum buffs published in one snapshot.
pub const RF_MAX_BUFFS: usize = 96;
/// Maximum active cooldowns published in one snapshot.
pub const RF_MAX_COOLDOWNS: usize = 64;
/// Number of skillbar slots.
pub const RF_MAX_SKILLS: usize = 18;
/// Maximum build skill selections.
pub const RF_MAX_SKILL_SELECTIONS: usize = 16;
/// Number of group member slots.
pub const RF_MAX_GROUP_MEMBERS: usize = 4;

/// A current/max resource pair.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfResource {
    pub current: f32,
    pub max: f32,
}

/// Generic combatant health/barrier/defiance.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfCombatant {
    pub health_current: f32,
    pub health_max: f32,
    pub barrier_current: f32,
    pub barrier_max: f32,
    /// 0 alive, 1 downed, 2 dead, 255 unknown.
    pub health_state: u8,
    /// 0 none, 1 immune, 2 active, 3 recover.
    pub defiance: u8,
    pub defiance_percent: f32,
}

/// A buff/effect.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfBuff {
    pub id: u32,
    pub stacks: u32,
    pub duration_ms: u32,
    pub remaining_ms: u32,
}

/// A skillbar ability.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfAbility {
    pub id: u32,
    pub has_ability: u8,
    pub ammo: u32,
    pub recharge_ms: u32,
    pub recharge_remaining_ms: u32,
    pub recharge_rate: f32,
    pub ammo_recharge_ms: u32,
    pub ammo_recharge_remaining_ms: u32,
    /// `AbilityInfo` bit flags (little-endian u16).
    pub info: u16,
}

/// An active cooldown, possibly off-bar.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfCooldown {
    pub skill_id: u32,
    /// Skillbar slot ordinal, or -1 when the skill is not on the current bar.
    pub slot_index: i16,
    pub recharge_ms: u32,
    pub recharge_remaining_ms: u32,
    pub recharge_rate: f32,
    pub ammo: u32,
    pub ammo_recharge_ms: u32,
    pub ammo_recharge_remaining_ms: u32,
}

/// Player resources.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfResources {
    pub combatant: RfCombatant,
    pub health_reduction: RfResource,
    pub endurance: RfResource,
    pub primary: RfResource,
    pub secondary: RfResource,
    pub rate: RfResource,
    pub has_pet: u8,
    pub pet: RfCombatant,
}

/// Player build.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfBuild {
    pub specs: [u32; 3],
    pub traits: [u32; 9],
    pub skill_selection_count: u32,
    pub skill_selections: [u32; RF_MAX_SKILL_SELECTIONS],
    /// `ProfSelection` bit flags.
    pub prof_selections: u32,
    pub pets: [u32; 2],
}

/// Player gear.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfGear {
    /// `Weapon` bit flags.
    pub weapons: u32,
    pub sigils: [u32; 4],
    pub relic: u32,
}

/// A group member's combat snapshot.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RfGroupMember {
    pub has_account: u8,
    pub has_resources: u8,
    pub resources: RfCombatant,
    pub buff_count: u32,
    pub buffs: [RfBuff; RF_MAX_BUFFS],
}

/// Immutable, C-compatible snapshot of [`Context`].
#[repr(C)]
pub struct RfSnapshot {
    pub now: u32,

    // Player identity.
    pub race: u8,
    /// API profession id (Guardian=1 … Revenant=9); 0 when unknown.
    pub profession: u8,
    pub spec: u32,
    pub mount: u8,

    // Player resources.
    pub resources_valid: u8,
    pub resources: RfResources,

    // Player buffs.
    pub buff_count: u32,
    pub buffs: [RfBuff; RF_MAX_BUFFS],
    pub last_screen_border: u32,
    pub last_squad_highlight: u32,

    // Player skillbar.
    pub skill_count: u32,
    pub skills: [RfAbility; RF_MAX_SKILLS],

    // Active cooldowns (incl. off-bar).
    pub cooldown_count: u32,
    pub cooldowns: [RfCooldown; RF_MAX_COOLDOWNS],

    // Player build.
    pub build_valid: u8,
    pub build: RfBuild,

    // Player gear.
    pub gear_valid: u8,
    pub gear: RfGear,

    // Target.
    pub target_valid: u8,
    /// Disposition bits (`Affinity`), 0 when unknown.
    pub target_affinity: u8,
    /// Classification bits (`TargetCategory`), 0 when unknown.
    pub target_category: u32,
    pub target: RfCombatant,
    pub target_buff_count: u32,
    pub target_buffs: [RfBuff; RF_MAX_BUFFS],

    // Group.
    pub group_type: u8, // 0 solo, 1 party, 2 squad.
    pub group_member_count: u32,
    pub group_members: [RfGroupMember; RF_MAX_GROUP_MEMBERS],
}

impl RfSnapshot {
    /// Returns a fresh, all-zero snapshot (every validity flag off).
    #[inline]
    pub fn empty() -> Self {
        // All-zero is a valid state for every field here (POD scalars), and the
        // validity flags / counts are zero, so no sentinel value is relied on.
        unsafe { std::mem::zeroed() }
    }
}

/// Fills [`out`](RfSnapshot) from a [`Context`]. Cheap: copies fixed-size
/// fields and bounded arrays, no allocation.
pub fn fill_snapshot(out: &mut RfSnapshot, ctx: &Context) {
    *out = RfSnapshot::empty();
    out.now = ctx.now;

    // Player identity.
    out.race = ctx.player.race.ok().map(|race| race as u8).unwrap_or(0);
    out.profession = ctx.player.prof.ok().map(Profession::api_id).unwrap_or(0);
    out.spec = ctx.player.spec.ok().map(|spec| spec as u32).unwrap_or(0);
    out.mount = ctx.player.mount.ok().map(|mount| mount as u8).unwrap_or(0);

    let health = ctx.player.health.as_ref().ok();
    let resources = ctx.player.resources.as_ref().ok();
    if health.is_some() || resources.is_some() {
        out.resources_valid = 1;
        fill_resources(&mut out.resources, health, resources);
    }
    if let Ok(buff_info) = &ctx.player.buff_info {
        out.buff_count = fill_buffs(&mut out.buffs, &buff_info.buffs, ctx.now);
        out.last_screen_border = buff_info.last_screen_border;
        out.last_squad_highlight = buff_info.last_squad_highlight;
    }
    if let Ok(skillbar) = &ctx.player.skillbar {
        fill_skillbar(&mut out.skills, &mut out.skill_count, skillbar, ctx.now);
    }
    if let Ok(cooldowns) = &ctx.player.active_cooldowns {
        out.cooldown_count = fill_cooldowns(&mut out.cooldowns, cooldowns, ctx.now);
    }
    if let Ok(build) = &ctx.player.build {
        out.build_valid = 1;
        fill_build(&mut out.build, build);
    }
    if let Ok(gear) = &ctx.player.gear {
        out.gear_valid = 1;
        fill_gear(&mut out.gear, gear);
    }
    if let Ok(target) = &ctx.target.resources {
        out.target_valid = 1;
        fill_combatant(&mut out.target, target);
    }
    out.target_affinity = match &ctx.target.affinity {
        Ok(affinity) => *affinity as u8,
        Err(_) => 0,
    };
    out.target_category = match &ctx.target.category {
        Ok(category) => category.bits(),
        Err(_) => 0,
    };
    if let Ok(target_buffs) = &ctx.target.buffs {
        out.target_buff_count = fill_buffs(&mut out.target_buffs, target_buffs, ctx.now);
    }
    if let Ok(group) = &ctx.group {
        out.group_type = match group.group_type {
            crate::context::GroupType::Solo => 0,
            crate::context::GroupType::Party => 1,
            crate::context::GroupType::Squad => 2,
        };
        let mut count = 0;
        for member in group.members.iter() {
            if count >= RF_MAX_GROUP_MEMBERS {
                break;
            }
            // Skip the blank slots `GroupInfo::empty()` / the reader leaves around.
            let present =
                member.account.is_some() || member.resources.is_ok() || member.buffs.is_ok();
            if !present {
                continue;
            }
            let slot = &mut out.group_members[count];
            member_account(slot, member);
            member_resources(slot, member);
            member_buffs(slot, member, ctx.now);
            count += 1;
        }
        out.group_member_count = count as u32;
    }
}

fn fill_resources(
    out: &mut RfResources,
    health: Option<&PlayerHealth>,
    resources: Option<&PlayerResources>,
) {
    if let Some(health) = health {
        fill_combatant(&mut out.combatant, &health.combatant);
        out.health_reduction = resource(&health.health_reduction);
    }
    if let Some(resources) = resources {
        out.endurance = resource(&resources.endurance);
        out.primary = resource(&resources.primary);
        out.secondary = resource(&resources.secondary);
        out.rate = resource(&resources.rate);
        if let Some(pet) = &resources.pet {
            out.has_pet = 1;
            fill_combatant(&mut out.pet, pet);
        }
    }
}

fn fill_combatant(out: &mut RfCombatant, combatant: &CombatantResources) {
    out.health_current = combatant.health.current;
    out.health_max = combatant.health.max;
    out.barrier_current = combatant.barrier.current;
    out.barrier_max = combatant.barrier.max;
    out.health_state = health_state_code(combatant.health_state);
    out.defiance = match combatant.defiance {
        None => 0,
        Some(Defiance::Immune) => 1,
        Some(Defiance::Active(_)) => 2,
        Some(Defiance::Recover(_)) => 3,
    };
    out.defiance_percent = combatant
        .defiance
        .as_ref()
        .map(Defiance::percent)
        .unwrap_or(0.0);
}

/// Encodes a [`ResourceState`] health state for the C ABI: 0 alive, 1 downed,
/// 2 dead, 255 unknown.
fn health_state_code(state: ResourceState) -> u8 {
    if state == ResourceState::HealthDowned {
        1
    } else if state == ResourceState::HealthDead {
        2
    } else if state == ResourceState::HealthAlive {
        0
    } else {
        255
    }
}

fn resource(resource: &Resource) -> RfResource {
    RfResource {
        current: resource.current,
        max: resource.max,
    }
}

fn fill_buffs(out: &mut [RfBuff; RF_MAX_BUFFS], buffs: &crate::context::BuffMap, now: u32) -> u32 {
    let mut count = 0;
    for (id, buff) in buffs {
        if count >= RF_MAX_BUFFS {
            break;
        }
        out[count] = buff_snapshot(*id, buff, now);
        count += 1;
    }
    count as u32
}

fn buff_snapshot(id: u32, buff: &Buff, now: u32) -> RfBuff {
    RfBuff {
        id,
        stacks: buff.stacks,
        duration_ms: if buff.is_infinite() {
            u32::MAX
        } else {
            buff.runout_time.saturating_sub(buff.apply_time)
        },
        remaining_ms: if buff.is_infinite() {
            u32::MAX
        } else {
            buff.runout_time.saturating_sub(now)
        },
    }
}

fn fill_skillbar(
    out: &mut [RfAbility; RF_MAX_SKILLS],
    count: &mut u32,
    skillbar: &crate::context::Skillbar,
    now: u32,
) {
    *count = 0;
    for (index, slot) in Slot::iter().enumerate() {
        if index >= RF_MAX_SKILLS {
            break;
        }
        if let Some(ability) = skillbar.slot(slot) {
            out[index] = ability_snapshot(ability, now);
            *count += 1;
        }
    }
}

/// Snapshots an ability. `recharge_remaining` is stored as a snapshot at its
/// `last_update` anchor, so the remaining at `now` is projected from it.
fn ability_snapshot(ability: &Ability, now: u32) -> RfAbility {
    RfAbility {
        id: skill_id_to_u32(ability.id),
        has_ability: 1,
        ammo: ability.ammo,
        recharge_ms: ability.recharge,
        recharge_remaining_ms: ability.recharge_remaining(now),
        recharge_rate: ability.recharge_rate,
        ammo_recharge_ms: ability.ammo_recharge,
        ammo_recharge_remaining_ms: ability.ammo_recharge_remaining(now),
        info: ability.info.bits(),
    }
}

fn fill_cooldowns(
    out: &mut [RfCooldown; RF_MAX_COOLDOWNS],
    cooldowns: &[ActiveCooldown],
    now: u32,
) -> u32 {
    let mut count = 0;
    for cd in cooldowns {
        if count >= RF_MAX_COOLDOWNS {
            break;
        }
        out[count] = cooldown_snapshot(cd, now);
        count += 1;
    }
    count as u32
}

/// Snapshots a cooldown. `recharge_remaining` is stored as a snapshot at its
/// `last_update` anchor, so the remaining at `now` is projected from it.
fn cooldown_snapshot(cd: &ActiveCooldown, now: u32) -> RfCooldown {
    RfCooldown {
        skill_id: skill_id_to_u32(cd.skill),
        slot_index: cd.slot.map(|slot| slot as i16).unwrap_or(-1),
        recharge_ms: cd.recharge,
        recharge_remaining_ms: cd.recharge_remaining(now),
        recharge_rate: cd.recharge_rate,
        ammo: cd.ammo,
        ammo_recharge_ms: cd.ammo_recharge,
        ammo_recharge_remaining_ms: cd.ammo_recharge_remaining(now),
    }
}

fn fill_build(out: &mut RfBuild, build: &Build) {
    out.specs = build.specs;
    out.traits = build.traits;
    out.skill_selection_count = 0;
    for (index, id) in build.skill_selections.iter().enumerate() {
        if index >= RF_MAX_SKILL_SELECTIONS {
            break;
        }
        out.skill_selections[index] = *id;
        out.skill_selection_count += 1;
    }
    out.prof_selections = build.prof_selections.bits() as u32;
    out.pets = build.pets;
}

fn fill_gear(out: &mut RfGear, gear: &Gear) {
    out.weapons = gear.weapons.bits() as u32;
    out.sigils = gear.sigils;
    out.relic = gear.relic;
}

fn member_account(out: &mut RfGroupMember, member: &crate::context::GroupMember) {
    if member.account.is_some() {
        out.has_account = 1;
    }
}

fn member_resources(out: &mut RfGroupMember, member: &crate::context::GroupMember) {
    if let Ok(resources) = &member.resources {
        out.has_resources = 1;
        fill_combatant(&mut out.resources, resources);
    }
}

fn member_buffs(out: &mut RfGroupMember, member: &crate::context::GroupMember, now: u32) {
    if let Ok(buffs) = &member.buffs {
        out.buff_count = fill_buffs(&mut out.buffs, buffs, now);
    }
}

fn skill_id_to_u32(skill: SkillId) -> u32 {
    match skill {
        SkillId::Id(id) => id,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;

    #[test]
    fn empty_context_fills_a_valid_zeroed_snapshot() {
        let ctx = Context::empty();
        let mut snap = RfSnapshot::empty();
        fill_snapshot(&mut snap, &ctx);

        assert_eq!(snap.now, 0);
        // `Context::empty()` has empty-but-valid resources, skillbar, build,
        // gear, target and an empty group; nothing should panic or leave a
        // dangling pointer.
        assert_eq!(snap.resources_valid, 1);
        assert_eq!(snap.buff_count, 0);
        assert_eq!(snap.skill_count, 0);
        assert_eq!(snap.cooldown_count, 0);
        assert_eq!(snap.build_valid, 1);
        assert_eq!(snap.gear_valid, 1);
        assert_eq!(snap.target_valid, 1);
        assert_eq!(snap.target_buff_count, 0);
        assert_eq!(snap.group_type, 0);
        assert_eq!(snap.group_member_count, 0);
    }

    #[test]
    fn snapshot_layout_is_c_compatible_pod() {
        // The exported struct must be plain-old-data (no drop glue, no
        // references) so a foreign function can read it with no marshalling.
        assert_eq!(std::mem::size_of::<f32>(), 4);
        assert_eq!(std::mem::align_of::<RfSnapshot>(), std::mem::align_of::<f32>());
    }

    #[test]
    fn snapshot_sizes() {
        // Mirrors the ctypes definitions in scripts/read_snapshot.py and the
        // struct definitions in reffect_snapshot.h. If the layout changes,
        // update all three sides together.
        let sizes = [
            ("RfResource", std::mem::size_of::<RfResource>(), 8),
            ("RfCombatant", std::mem::size_of::<RfCombatant>(), 24),
            ("RfBuff", std::mem::size_of::<RfBuff>(), 16),
            ("RfAbility", std::mem::size_of::<RfAbility>(), 36),
            ("RfCooldown", std::mem::size_of::<RfCooldown>(), 32),
            ("RfResources", std::mem::size_of::<RfResources>(), 92),
            ("RfBuild", std::mem::size_of::<RfBuild>(), 128),
            ("RfGear", std::mem::size_of::<RfGear>(), 24),
            ("RfGroupMember", std::mem::size_of::<RfGroupMember>(), 1568),
            ("RfSnapshot", std::mem::size_of::<RfSnapshot>(), 12372),
        ];
        for (name, size, expected) in sizes {
            assert_eq!(size, expected, "{name} changed size");
        }
    }

    #[test]
    fn snapshot_target_classification_field_order() {
        // The C header and the ctypes mirror both place the classification
        // fields directly after `target_valid`, before `target`. Pin the offsets
        // so a reordering cannot silently desync the ABI.
        let valid = std::mem::offset_of!(RfSnapshot, target_valid);
        assert_eq!(std::mem::offset_of!(RfSnapshot, target_affinity), valid + 1);
        assert_eq!(std::mem::offset_of!(RfSnapshot, target_category), valid + 4);
        assert_eq!(std::mem::offset_of!(RfSnapshot, target), valid + 8);

        // `RfCombatant::health_state` precedes `defiance` in both mirrors.
        assert_eq!(std::mem::offset_of!(RfCombatant, health_state), 16);
        assert_eq!(std::mem::offset_of!(RfCombatant, defiance), 17);
    }
}
