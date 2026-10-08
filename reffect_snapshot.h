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

/* High level kind of the current target, published in `target_kind`.
 *
 * Derived from the kind axis of `target_category`, so a consumer that only needs
 * to decide whether a target is worth attacking can switch on one byte:
 *
 *     if (snap->target_kind == RF_TARGET_MONSTER ||
 *         snap->target_kind == RF_TARGET_ATTACK_TARGET) { ... }
 */
#define RF_TARGET_UNKNOWN       0
#define RF_TARGET_PLAYER        1
#define RF_TARGET_NPC           2   /* friendly / neutral non-player character */
#define RF_TARGET_MONSTER       3   /* hostile or indifferent creature */
#define RF_TARGET_ATTACK_TARGET 4   /* world boss prop, destructible structure */
#define RF_TARGET_GATHERABLE    5   /* mining, logging or harvesting node */
#define RF_TARGET_CHEST         6
#define RF_TARGET_GADGET        7   /* other environmental object */
#define RF_TARGET_PET           8

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
    uint8_t health_state;    /* 0 alive, 1 downed, 2 dead, 255 unknown */
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

    /* Player world position in game units; all zero when unknown. */
    float player_position[3];

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

    /* High level kind of the target, one of the RF_TARGET_* codes below.
     * RF_TARGET_UNKNOWN when the target could not be classified. */
    uint8_t target_kind;

    /* Target classification. Both are bit sets, 0 when unknown.
     *
     * target_affinity (disposition):
     *   1 << 0 friendly, 1 << 1 hostile, 1 << 2 neutral
     *
     * target_category (four independent axes; at most one flag per axis is set,
     * and an axis the reader could not resolve contributes no flag):
     *   kind:   1 << 0  player          1 << 1  pet
     *           1 << 2  npc             1 << 3  monster
     *           1 << 4  gatherable      1 << 5  chest
     *           1 << 6  gadget          1 << 20 attack target
     *   affin.: 1 << 7  friendly        1 << 8  neutral
     *           1 << 9  hostile
     *   rank:   1 << 10 ambient         1 << 11 normal
     *           1 << 12 veteran         1 << 13 elite
     *           1 << 14 champion        1 << 15 legendary
     *           1 << 16 epic
     *   node:   1 << 17 ore             1 << 18 wood
     *           1 << 19 plant
     */
    uint8_t target_affinity;
    uint32_t target_category;

    /* Target world position in game units, and the distance from the player.
     * The distance is -1.0 when either position is unknown. */
    float target_position[3];
    float target_distance;

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
 *
 *   // Gate a rotation on the current target being attackable and in range.
 *   int attackable = snap->target_kind == RF_TARGET_MONSTER ||
 *                    snap->target_kind == RF_TARGET_ATTACK_TARGET;
 *   if (attackable && snap->target_distance >= 0.0f &&
 *       snap->target_distance <= skill_range) {
 *       use_skill();
 *   }
 *
 *   // The full classification is in target_category, for example a champion
 *   // monster is RF_CAT_MONSTER | RF_CAT_HOSTILE | RF_CAT_CHAMPION.
 *   if (snap->target_category & RF_CAT_CHAMPION) {
 *       use_defiance_break();
 *   }
 */

/* Category bits, matching `target_category`. */
#define RF_CAT_PLAYER         (1u << 0)
#define RF_CAT_PET            (1u << 1)
#define RF_CAT_NPC            (1u << 2)
#define RF_CAT_MONSTER        (1u << 3)
#define RF_CAT_GATHERABLE     (1u << 4)
#define RF_CAT_CHEST          (1u << 5)
#define RF_CAT_GADGET         (1u << 6)
#define RF_CAT_FRIENDLY       (1u << 7)
#define RF_CAT_NEUTRAL        (1u << 8)
#define RF_CAT_HOSTILE        (1u << 9)
#define RF_CAT_AMBIENT        (1u << 10)
#define RF_CAT_NORMAL         (1u << 11)
#define RF_CAT_VETERAN        (1u << 12)
#define RF_CAT_ELITE          (1u << 13)
#define RF_CAT_CHAMPION       (1u << 14)
#define RF_CAT_LEGENDARY      (1u << 15)
#define RF_CAT_EPIC           (1u << 16)
#define RF_CAT_ORE            (1u << 17)
#define RF_CAT_WOOD           (1u << 18)
#define RF_CAT_PLANT          (1u << 19)
#define RF_CAT_ATTACK_TARGET  (1u << 20)

#ifdef __cplusplus
}
#endif

#endif /* REFFECT_SNAPSHOT_H */
