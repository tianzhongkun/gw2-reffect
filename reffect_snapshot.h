/* reffect_snapshot.h
 *
 * C ABI snapshot of the current Guild Wars 2 character state, exported by the
 * reffect_open addon DLL. Include this in a C / C++ / C# / Rust FFI consumer and
 * call `reffect_snapshot()` to get a pointer to the current state.
 *
 * The layout is fixed (`#[repr(C)]`, plain-old-data) and identical to the Rust
 * definitions in `reffect_core::snapshot`. All arrays carry an explicit count /
 * validity flag, so a value of `*_valid == 0` or `count == 0` simply means the
 * data is not currently available.
 *
 * The returned pointer is valid on the calling thread until the next call to
 * `reffect_snapshot()` on that same thread; read (or memcpy) it before calling
 * again. It is never NULL.
 */
#ifndef REFFECT_SNAPSHOT_H
#define REFFECT_SNAPSHOT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define RF_MAX_BUFFS 96
#define RF_MAX_COOLDOWNS 64
#define RF_MAX_SKILLS 18
#define RF_MAX_SKILL_SELECTIONS 16
#define RF_MAX_GROUP_MEMBERS 4

/* A current/max resource pair. */
typedef struct RfResource {
    float current;
    float max;
} RfResource;

/* Generic combatant health / barrier / defiance. */
typedef struct RfCombatant {
    float health_current;
    float health_max;
    float barrier_current;
    float barrier_max;
    uint8_t defiance;        /* 0 none, 1 immune, 2 active, 3 recover */
    float defiance_percent;
} RfCombatant;

/* A buff/effect. duration_ms/remaining_ms == UINT32_MAX means infinite. */
typedef struct RfBuff {
    uint32_t id;
    uint32_t stacks;
    uint32_t duration_ms;
    uint32_t remaining_ms;
} RfBuff;

/* A skillbar ability. */
typedef struct RfAbility {
    uint32_t id;
    uint8_t has_ability;
    uint32_t ammo;
    uint32_t recharge_ms;
    uint32_t recharge_remaining_ms;
    float recharge_rate;
    uint32_t ammo_recharge_ms;
    uint32_t ammo_recharge_remaining_ms;
    uint16_t info;           /* AbilityInfo bit flags */
} RfAbility;

/* An active cooldown, possibly off-bar. */
typedef struct RfCooldown {
    uint32_t skill_id;
    int16_t slot_index;      /* -1 when not on the current bar */
    uint32_t recharge_ms;
    uint32_t recharge_remaining_ms;
    float recharge_rate;
    uint32_t ammo;
    uint32_t ammo_recharge_ms;
    uint32_t ammo_recharge_remaining_ms;
} RfCooldown;

/* Player resources. */
typedef struct RfResources {
    RfCombatant combatant;
    RfResource health_reduction;
    RfResource endurance;
    RfResource primary;
    RfResource secondary;
    RfResource rate;
    uint8_t has_pet;
    RfCombatant pet;
} RfResources;

/* Player build. */
typedef struct RfBuild {
    uint32_t specs[3];
    uint32_t traits[9];
    uint32_t skill_selection_count;
    uint32_t skill_selections[RF_MAX_SKILL_SELECTIONS];
    uint32_t prof_selections;  /* ProfSelection bit flags */
    uint32_t pets[2];
} RfBuild;

/* Player gear. */
typedef struct RfGear {
    uint32_t weapons;          /* Weapon bit flags */
    uint32_t sigils[4];
    uint32_t relic;
} RfGear;

/* A group member's combat snapshot. */
typedef struct RfGroupMember {
    uint8_t has_account;
    uint8_t has_resources;
    RfCombatant resources;
    uint32_t buff_count;
    RfBuff buffs[RF_MAX_BUFFS];
} RfGroupMember;

/* Snapshot of the current game state. */
typedef struct RfSnapshot {
    uint32_t now;

    /* Player identity. `race`/`profession`/`spec`/`mount` are 0 when unknown. */
    uint8_t race;
    uint8_t profession;
    uint32_t spec;
    uint8_t mount;

    /* Player resources. */
    uint8_t resources_valid;
    RfResources resources;

    /* Player buffs. */
    uint32_t buff_count;
    RfBuff buffs[RF_MAX_BUFFS];
    uint32_t last_screen_border;
    uint32_t last_squad_highlight;

    /* Player skillbar (slot index == array index). */
    uint32_t skill_count;
    RfAbility skills[RF_MAX_SKILLS];

    /* Active cooldowns (incl. off-bar). */
    uint32_t cooldown_count;
    RfCooldown cooldowns[RF_MAX_COOLDOWNS];

    /* Player build. */
    uint8_t build_valid;
    RfBuild build;

    /* Player gear. */
    uint8_t gear_valid;
    RfGear gear;

    /* Target. */
    uint8_t target_valid;
    RfCombatant target;
    uint32_t target_buff_count;
    RfBuff target_buffs[RF_MAX_BUFFS];

    /* Group. `group_type`: 0 solo, 1 party, 2 squad. */
    uint8_t group_type;
    uint32_t group_member_count;
    RfGroupMember group_members[RF_MAX_GROUP_MEMBERS];
} RfSnapshot;

/* Get a pointer to this thread's snapshot of the current game state. */
const RfSnapshot* reffect_snapshot(void);

/* Size in bytes of RfSnapshot, for consumers that allocate their own buffer. */
size_t reffect_snapshot_size(void);

/* Example (C):
 *
 *   #include "reffect_snapshot.h"
 *
 *   const RfSnapshot* snap = reffect_snapshot();
 *   if (snap->resources_valid) {
 *       printf("HP %.0f/%.0f  Spec %u\n",
 *              snap->resources.combatant.health_current,
 *              snap->resources.combatant.health_max,
 *              snap->spec);
 *   }
 *   for (uint32_t i = 0; i < snap->cooldown_count; ++i) {
 *       const RfCooldown* cd = &snap->cooldowns[i];
 *       printf("cd %u rem %.1fs (off-bar=%d)\n",
 *              cd->skill_id, cd->recharge_remaining_ms / 1000.0f,
 *              cd->slot_index < 0);
 *   }
 */

#ifdef __cplusplus
}
#endif

#endif /* REFFECT_SNAPSHOT_H */
