//! Runtime character-state reader for the current Guild Wars 2 client.
//!
//! This module deliberately consumes only validated, read-only pointers. The
//! worker discovers the current Context tree from the game thread's TLS block,
//! then copies its state before acquiring the public [`Context`] mutex. It
//! therefore never makes the game thread wait on rendering, I/O, or metadata
//! lookups.

use super::{
    GameThreadFinder, capture::CapturedContext, hook, main_module_range, read_process_memory,
};
use enumflags2::{BitFlags, make_bitflags};
use nexus::rtapi::{GroupType as RtapiGroupType, RealTimeApi};
use reffect_core::{
    context::{
        Ability, AbilityInfo, ActiveCooldown, Affinity, Buff, BuffMap, Build, CombatantResources,
        Context, Defiance, Gear, GroupInfo, GroupMember, GroupType, PlayerBuffInfo, PlayerHealth,
        PlayerResources, ProfSelection, Profession, Race, Resource, ResourceState, SkillId,
        Skillbar, Slot, Specialization, TargetCategory, Update, Weapon,
    },
    error::Error,
};

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use strum::IntoEnumIterator;

const CHARACTER_AGENT: usize = 0x98;
const CHARACTER_GROUP_IDENTITY: usize = 0x18;
const CHARACTER_GROUP_MEMBERS: usize = 0x3C0;
const CHARACTER_GROUP_MEMBERS_FALLBACK: usize = 0x3B0;
const CHARACTER_HEALTH: usize = 0x3E8;
const HEALTH_SAMPLE_TICK: usize = 0x08;
const HEALTH_CURRENT: usize = 0x0C;
const HEALTH_MAX: usize = 0x10;
const HEALTH_RATE: usize = 0x14;
const HEALTH_BARRIER: usize = 0x28;
const BARRIER_TICK: usize = 0x24;
const BARRIER_MAX: usize = 0x2C;
const BARRIER_RATE: usize = 0x30;

/// Barrier field offsets inside a health object. The character layout keeps the
/// barrier after the defiance slot; the carrier layout used by target kinds 10
/// and 11 places it directly after the health fields.
#[derive(Debug, Clone, Copy)]
struct BarrierLayout {
    tick: usize,
    current: usize,
    max: usize,
    rate: usize,
}

const BARRIER_LAYOUT_CHARACTER: BarrierLayout = BarrierLayout {
    tick: BARRIER_TICK,
    current: HEALTH_BARRIER,
    max: BARRIER_MAX,
    rate: BARRIER_RATE,
};

const BARRIER_LAYOUT_CARRIER: BarrierLayout = BarrierLayout {
    tick: 0x18,
    current: 0x1C,
    max: 0x20,
    rate: 0x24,
};
const CHARACTER_ENDURANCE: usize = 0x3D0;
const ENDURANCE_CURRENT: usize = 0x10;
const ENDURANCE_MAX: usize = 0x14;
const CHARACTER_PROFESSION_ENERGY: usize = 0x3D8;
const PROFESSION_ENERGY_SAMPLE_TICK: usize = 0x08;
const PROFESSION_ENERGY_CURRENT: usize = 0x0C;
const PROFESSION_ENERGY_MAX: usize = 0x10;
const PROFESSION_ENERGY_RATE: usize = 0x14;
const CHARACTER_PROFESSION: usize = 0x510;
const THIEF_PRIMARY_CURRENT: usize = 0x88;
const THIEF_PRIMARY_MAX: usize = 0x8C;
// The original reader (`sub_1800E26B0` case 5) interpolates initiative from
// `profObj + 0x94` (the sample timestamp) and `profObj + 0x90` (the regen rate
// per second). These two were listed the other way round in an earlier pass,
// which made the elapsed time saturate to zero and froze initiative at its base
// value. Kept as absolute, client-verified offsets.
const THIEF_PRIMARY_SAMPLE_TICK: usize = 0x94;
const THIEF_PRIMARY_RATE: usize = 0x90;
const PROFESSION_PRIMARY_CURRENT: usize = 0x50;
const PROFESSION_PRIMARY_MAX: usize = 0x54;
const REVENANT_RATE_BASE: f32 = 5.0;
const REVENANT_RATE_MAX: f32 = 10.0;
// Elementalist Evoker charges — a REAL feature: the original shows
// "Primary 4/6" (基础技能充能, max 6) and "Secondary 0/3" (强化技能充能, max 3)
// for the Evoker elite spec. Reverse-engineered from the master per-profession
// resource reader `sub_1800E26B0` (case 6 = Elementalist, sub-case elite-spec
// id 80 = Evoker) and confirmed live against Reffect.dll 0.13.1 on 2026-09-23:
// the basic charges are read from the profession object (`character + 0x510`)
// as two u32 counts divided by 5.0 — live `profObj+0x80 = 20`, `+0x84 = 30`
// yields 20/5=4, 30/5=6, exactly matching the plugin's own published 4/6.
// Catalyst (elite-spec id 67) reads the same `profObj+0x80/0x84` pair as raw
// integers (no divide). All other elementalist specs expose no primary here.
//
// The Evoker "Secondary" (enhanced charges, max 3) is derived by counting
// specific effect stacks rather than a fixed field, so the current comes from
// `read_evoker_enhanced_current` and the max is a fixed 3.0.
const ELEMENTALIST_CHARGE_CURRENT: usize = 0x80;
const ELEMENTALIST_CHARGE_MAX: usize = 0x84;
const ELEMENTALIST_EVOKER_CHARGE_DIVISOR: f32 = 5.0;
const ELEMENTALIST_SECONDARY_MAX: f32 = 3.0;
const SPEC_ELEMENTALIST_CATALYST: u32 = 67;
const SPEC_ELEMENTALIST_EVOKER: u32 = 80;
// Evoker "enhanced" (secondary) charges are not a stored field: the original
// (`sub_1800E26B0` case 6 / spec 80) derives the current count from the
// character effect tree. If any of the "full" marker effects is present the
// bar reads full (3); otherwise the current equals the number of
// `EVOKER_ENHANCED_STACK_ID` effects present (live-verified: one 76749 node ->
// 1/3, matching the plugin's own output on 2026-09-23).
const EVOKER_ENHANCED_MARKER_IDS: [u32; 4] = [76884, 77216, 76728, 76992];
const EVOKER_ENHANCED_STACK_ID: u32 = 76749;
// Additional per-elite-spec resources read by the original master reader
// (`sub_1800E26B0`), each gated on the active elite spec id. The effect-count
// secondaries reuse the same confirmed effect-tree walk as the Evoker enhanced
// charge; the integer-pair cases read from the profession object. These are
// derived from the 0.13.1 decompile and are not live-verified on the current
// client (the Revenant case diverged, so re-confirm before shipping).
const SPEC_GUARDIAN_WILLBENDER: u32 = 62;
const SPEC_WARRIOR_BERSERKER: u32 = 68;
const SPEC_WARRIOR_SPELLBREAKER: u32 = 74;
const SPEC_NECROMANCER_HARBINGER: u32 = 60;
const SPEC_REVENANT_VINDICATOR: u32 = 79;
const WILLBENDER_PRIMARY_CURRENT: usize = 0x90;
const WILLBENDER_PRIMARY_MAX: usize = 0x94;
const HARBINGER_CURRENT: usize = 0x58;
const HARBINGER_MAX: usize = 0x5C;
const WARRIOR_BERSERKER_BURST_ID: u32 = 62_810;
const WARRIOR_SPELLBREAKER_STATE_ID: u32 = 76_770;
const VINDICATOR_ALLIANCE_ID: u32 = 77_274;
// The original decides the Berserker burst cap via a trait check
// (`sub_1800E0540` → 5.0 or 10.0); the common, trait-boosted cap is used here.
const WARRIOR_BERSERKER_MAX: f32 = 10.0;
const CHARACTER_CORE_STATS: usize = 0x388;
const CORE_RACE: usize = 0x33;
const CORE_PROFESSION: usize = 0x12C;
const CHARACTER_COMBATANT: usize = 0x520;
const COMBATANT_PRESSED_SLOT: usize = 0x74;
const COMBATANT_AUTO_ATTACK_SLOT: usize = 0x88;
const COMBATANT_PENDING_MASK: usize = 0x128;
const COMBATANT_SKILLDEF_BASE: usize = 0x1D0;
// The current original reader (`sub_1800DD050`) publishes Weapon Swap from
// this independent combatant state rather than a regular skill definition.
const COMBATANT_WEAPON_SWAP_LAST_UPDATE: usize = 0x28C;
const COMBATANT_WEAPON_SWAP_COOLDOWN: usize = 0x290;
const SKILLDEF_ID: usize = 0x28;
const SKILLDEF_FLAGS: usize = 0x38;
// The original ability decoder treats this flag as a ground-targeted action
// when deciding whether the normal target-range test applies.
const SKILLDEF_GROUND_TARGETED: u32 = 0x0008_0000;
const SKILLDEF_COOLDOWN: usize = 0x60;
const SKILLDEF_COOLDOWN_TOTAL: usize = 0x90;
// Maximum ammo/charges a skill can hold. The count is NOT on the skill
// definition itself — it lives on the cooldown sub-object at
// `definition + SKILLDEF_COOLDOWN (0x60)`, at offset `+0xF0`, and 0 means a
// single charge. This mirrors the original reader `sub_1800E1BB0`:
// `result = *(def+0x60); v15 = *(u32*)(result+0xF0); max = (v15==0)+v15`.
// Live-verified 2026-09-24 on the running client: weapon skill 1 (id 5526)
// reads 0→1 and weapon skill 3 (id 5694) reads 2, matching the game. The old
// direct reads (`def+0xF0` = 4/8, `def+0x90` = 2/2) were both wrong — 0xF0 is
// the right offset but on the sub-object, not the bare definition.
const SKILLDEF_MAX_AMMO: usize = 0xF0;
const COMBATANT_COOLDOWNS: usize = 0x150;
const COOLDOWN_ROOTS: [usize; 4] = [0x40, 0x48, 0x60, 0x68];
const COOLDOWN_STRIDE: usize = 0x38;
const COOLDOWN_RATE: usize = 0x30;
const COOLDOWN_NODE_REMAINING: usize = 0x10;
const COOLDOWN_NODE_SKILLDEF: usize = 0x18;
const COOLDOWN_NODE_TOTAL: usize = 0x20;
const COOLDOWN_NODE_AMMO_STATE: usize = 0x24;
const COOLDOWN_NODE_AMMO: usize = 0x28;
const COOLDOWN_NODE_NEXT: usize = 0x08;
const CHARACTER_KENNEL: usize = 0x3F8;
const KENNEL_PRIMARY_PET: usize = 0xF8;
const CHARACTER_BUILD_HOLDER: usize = 0x1E8;
const BUILD_HOLDER_VIEW: usize = 0x90;
const BUILD_VIEW_SLOT_BIAS: usize = 0x50;
const CORE_BUILD_HOLDER: usize = 0xF0;
const CORE_BUILD_HOLDER_VIEW: usize = 0x78;
const CORE_BUILD_VIEW_SLOT_BIAS: usize = 0x30;
const BUILD_SLOT_STRIDE: usize = 0x150;
const BUILD_ACTIVE_INDEX: usize = 0xC58;
const BUILD_SPEC_POINTERS: [usize; 3] = [0x120, 0x128, 0x130];
const BUILD_TRAIT_POINTER_BASE: usize = 0x138;
const BUILD_SLOT_PROFESSION: usize = 0xC0;
const BUILD_SLOT_PROFESSION_SELECTION: usize = 0xC8;
const BUILD_PROFESSION_SELECTION_KIND: usize = 0x08;
const BUILD_RANGER_FIRST_PET: usize = 0x18;
const BUILD_RANGER_SECOND_PET: usize = 0x20;
// Profession-selection skill ids stored on the build slot as a flat `u32`
// array: pointer at `slot+0x1A8`, element count at `slot+0x1B4` (the original
// reader `sub_1800E2EC0` walks `*(slot+0x1A8)` with count `*(slot+0x1B4)` and
// 4-byte stride). For the Evoker these hold the familiar selection ids; the
// original appends them to the published skill-selection list and derives the
// active familiar from them. Live-verified 2026-09-24: `[77225]` → "Otter".
const BUILD_SELECTION_ARRAY: usize = 0x1A8;
const BUILD_SELECTION_COUNT: usize = 0x1B4;
const MAX_SELECTION_SKILLS: usize = 8;
const ELEMENTALIST_EVOKER_SPEC: u32 = 80;
// Evoker familiar profession-selection skill ids, matched in array order (first
// wins); no match falls back to Fire. From `sub_1800E2EC0` (`cmp r8d,12DA9h`
// →Water, `12DAAh`→Earth, `12E3Ah`→Air; default flag 0x100 = Fire).
const EVOKER_FAMILIAR_WATER: u32 = 77_225;
const EVOKER_FAMILIAR_EARTH: u32 = 77_226;
const EVOKER_FAMILIAR_AIR: u32 = 77_370;
// Equipped heal/utility/elite skill tables on the build slot: five container
// pointers at `slot+0xD0` (Heal, Utility1-3, Elite). Each container holds a
// variant table at `container+0x60`, 0x70 stride; the equipped variant has
// `+0x08`/`+0x30` zeroed and a skilldef with the slotted-skill flag.
const BUILD_SELECTED_SLOT_BASE: usize = 0xD0;
const SELECTED_SLOT_COUNT: usize = 5;
const SKILL_VARIANT_BASE: usize = 0x60;
const SKILL_VARIANT_STRIDE: usize = 0x70;
const MAX_SKILL_VARIANTS: usize = 16;
const SKILLDEF_EQUIPPED_FLAG: u32 = 0x0400_0000;
const CHARACTER_SKILLBAR: usize = 0x3F0;
const SKILLBAR_SLOT_BASE: usize = 0x160;
const GEAR_WEAPON_SLOTS: [usize; 4] = [29, 30, 31, 32];
const GEAR_META_KIND: usize = 0x2C;
const GEAR_META_TYPE: usize = 0x30;
const GEAR_TYPE_INTERNAL: usize = 0x0C;
const GEAR_WEAPON_KIND: u32 = 0x18;
const GEAR_ENTRY_EFFECTS: usize = 0xA0;
const GEAR_EFFECT_SLOTS: [usize; 2] = [0x28, 0x30];
const GEAR_RELIC_ENTRY: usize = 0x378;
const GEAR_RELIC_EFFECT: usize = 0x40;
const GEAR_FOREIGN_KIND: usize = 0x2C;
const GEAR_FOREIGN_PAYLOAD: usize = 0x30;
const GEAR_FOREIGN_DIRECT: u32 = 0x12;
const GEAR_FOREIGN_WRAPPED: u32 = 0x17;
const GEAR_WRAPPED_EFFECT_OWNER: usize = 0x10;
const GEAR_WRAPPED_EFFECT: usize = 0x50;
const GEAR_EFFECT_ID: usize = 0x28;
const COMBATANT_TARGET_BRIDGE: usize = 0x80;
/// The target bridge is a game module static; the original caches this pointer
/// in its own context. Reading it directly keeps target resolution working when
/// the local character chain is stale (observed after entering an instance).
const TARGET_BRIDGE_RVA: usize = 0x2BAC3E0;
/// Selected-target descriptor slots on the target bridge; the original reader
/// prefers the first and falls back to the second.
const TARGET_DESCRIPTOR: usize = 0x298;
const TARGET_DESCRIPTOR_FALLBACK: usize = 0x368;
/// Target descriptor layout: the kind tag followed by the target object.
const TARGET_KIND: usize = 0x08;
const TARGET_OBJECT: usize = 0x10;
/// Per-kind resource sources. Kind 0 uses the character layout below; kinds 10
/// and 11 keep health, effects and defiance on a carrier object instead.
const TARGET_HEALTH_KIND_10: usize = 0x1F0;
const TARGET_CARRIER_KIND_11: usize = 0x58;
const TARGET_HEALTH_KIND_11: usize = 0x220;
const TARGET_EFFECTS_KIND_11: usize = 0x170;
const TARGET_DEFIANCE_KIND_11: usize = 0x168;
const SELECTED_CHARACTER_BIAS: usize = 0x48;
/// Regions dumped by the target classification probe
/// (`log_target_classification_probe`), as `(start, end)` byte offsets from the
/// region base, emitted in `PROBE_CHUNK` sized hexdump lines. Only used for
/// offset recovery; see docs/superpowers/plans/2026-10-08-target-classification-recovery.md.
const PROBE_CHUNK: usize = 0x80;
const PROBE_DESCRIPTOR: (usize, usize) = (0x000, 0x080);
const PROBE_OBJECT: (usize, usize) = (0x000, 0x180);
const PROBE_CHARACTER: (usize, usize) = (0x000, 0x540);
const PROBE_CORE_STATS: (usize, usize) = (0x000, 0x180);
const PROBE_AGENT: (usize, usize) = (0x000, 0x180);
/// Number of probe dumps after which probing stops, so a client that thrashes
/// the target descriptor cannot flood the log.
const PROBE_LIMIT: u32 = 256;
/// Maximum strings logged per region by the probe.
const PROBE_STRING_LIMIT: usize = 12;
/// Ranges used by the one-shot local name calibration, which searches for a name
/// that is known to exist rather than for anything name shaped. The character is
/// wide enough to hold a name table reference, and the agent to reach past the
/// position and velocity floats.
const PROBE_LOCAL_CHARACTER: (usize, usize) = (0x000, 0x2000);
const PROBE_LOCAL_AGENT: (usize, usize) = (0x000, 0x1000);
const PROBE_LOCAL_CORE: (usize, usize) = (0x000, 0x400);
/// Bytes searched inside each pointer target by the second hop of the name search.
const PROBE_NAME_INNER: usize = 0x200;

/// Sequence of the target classification probe dump for the currently selected
/// target; 0 when no target is selected or the probe limit was reached.
static TARGET_PROBE_SEQUENCE: AtomicU32 = AtomicU32::new(0);
/// Target object the probe last dumped, so each selection dumps exactly once.
static TARGET_PROBE_OBJECT: AtomicUsize = AtomicUsize::new(0);
/// Next probe sequence to hand out.
static TARGET_PROBE_NEXT: AtomicU32 = AtomicU32::new(1);

/// Returns the probe sequence of the currently selected target.
///
/// The in-game debug window shows this so a capture can be labelled as it is
/// taken, instead of relying on the order targets were selected in. `None` when
/// no target is selected or the probe limit was reached.
pub fn target_probe_sequence() -> Option<u32> {
    match TARGET_PROBE_SEQUENCE.load(Ordering::Relaxed) {
        0 => None,
        sequence => Some(sequence),
    }
}

/// Forgets the probed target, so selecting the same target again produces a
/// fresh dump and sequence.
fn clear_target_probe() {
    TARGET_PROBE_OBJECT.store(0, Ordering::Relaxed);
    TARGET_PROBE_SEQUENCE.store(0, Ordering::Relaxed);
}
const CHARACTER_DEFIANCE: usize = 0xC8;
const CHARACTER_WEAPON_SWAP_STATE: usize = 0xD8;
const DEFIANCE_STATE: usize = 0x40;
const DEFIANCE_VALUE: usize = 0x44;
const LOCKED_SKILL_ID: u32 = 10_586;
const MAX_COOLDOWN_NODES: usize = 64;
const STATE_POLL: Duration = Duration::from_millis(100);
const BUFF_SNAPSHOT_POLL: u32 = 500;
const MAX_BUFF_CACHE_ENTRIES: usize = GroupInfo::MAX_MEMBERS + 2;
// Group offsets were recovered from the last private backend.  All walkers
// below validate every pointer, count, and local-identity match before any
// state is published, so an incompatible client falls closed with Error::Group.
const GROUP_CONTEXT_ROOT: usize = 0x290;
const GROUP_DIRECTORY_TABLE: usize = 0x110;
const GROUP_DIRECTORY_STRIDE: usize = 0x28;
const GROUP_DIRECTORY_COUNT: usize = 15;
const GROUP_DIRECTORY_IDENTITY_COUNT: usize = 0x08;
const GROUP_DIRECTORY_IDENTITIES: usize = 0x10;
const GROUP_DIRECTORY_ACTIVE: usize = 0x20;
// Each directory identity record is 20 bytes: a 16-byte identity
// (`dword@0, word@4, word@6, qword@8`, read as two u64 words) followed by a
// 4-byte validity flag at +0x10. The array is strided by the full record, not
// by the 16-byte identity payload. Confirmed against Reffect.dll 0.13.1
// (`sub_1800E2EC0`: `v31 = 20 * count`, `v162 += 20`).
const GROUP_IDENTITY_STRIDE: usize = 0x14;
const MAX_GROUP_IDENTITIES: usize = 50;
const GROUP_MEMBER_ENTRIES: usize = 0xAA8;
const GROUP_MEMBER_COUNT: usize = 0xAB4;
const GROUP_MEMBER_CHARACTER: usize = 0x78;
const MAX_GROUP_MEMBER_ENTRIES: usize = 64;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount() -> u32;
}

static STATE_WORKER: OnceLock<Mutex<Option<StateWorker>>> = OnceLock::new();

#[derive(Debug)]
struct StateWorker {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

/// Starts the runtime state worker.  Repeated calls during a load cycle are
/// harmless, which mirrors the public metadata catalogue lifecycle.
pub(crate) fn init() {
    let mut slot = state_worker()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_some() {
        return;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    match thread::Builder::new()
        .name("reffect-game-state".into())
        .spawn(move || state_worker_loop(worker_stop))
    {
        Ok(join) => *slot = Some(StateWorker { stop, join }),
        Err(error) => log::warn!("Failed to create Reffect runtime state worker: {error}"),
    }
}

/// Requests shutdown and joins the state worker before the addon unloads.
pub(crate) fn deinit() {
    let worker = state_worker()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(worker) = worker {
        worker.stop.store(true, Ordering::Release);
        if worker.join.join().is_err() {
            log::warn!("Reffect runtime state worker panicked");
        }
    }
}

fn state_worker() -> &'static Mutex<Option<StateWorker>> {
    STATE_WORKER.get_or_init(|| Mutex::new(None))
}

fn state_worker_loop(stop: Arc<AtomicBool>) {
    let reader = GameStateReader::default();
    let mut unavailable = false;
    let mut unavailable_polls = 0u32;
    while !stop.load(Ordering::Acquire) {
        match reader.snapshot() {
            Some(snapshot) => {
                unavailable = false;
                unavailable_polls = 0;
                snapshot.apply();
            }
            None => {
                unavailable_polls = unavailable_polls.saturating_add(1);
                if !unavailable {
                    log::debug!(
                        "[reffect-runtime] waiting for game-thread context (captures={})",
                        hook::capture_count()
                    );
                    unavailable = true;
                }
                if unavailable_polls == 10 {
                    let telemetry = hook::capture_telemetry();
                    log::warn!(
                        "[reffect-runtime] context unavailable after 1s: status={:?} telemetry={telemetry:?}",
                        telemetry.status()
                    );
                }
                if unavailable_polls.is_multiple_of(100) {
                    log::debug!(
                        "[reffect-runtime] snapshot still unavailable after {}s",
                        unavailable_polls / 10
                    );
                }
            }
        }
        thread::sleep(STATE_POLL);
    }
}

#[derive(Debug, Default)]
struct GameStateReader {
    game_thread: Mutex<GameThreadFinder>,
    buffs: Mutex<BuffSnapshotCache>,
    content_synced: AtomicBool,
}

impl GameStateReader {
    fn snapshot(&self) -> Option<RuntimeSnapshot> {
        let collection = self
            .game_thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .locate()
            .ok()?
            .context_address();
        let captured = hook::refresh_context_from_collection(collection)?;
        // Fold the runtime content-table skills into the metadata cache on the
        // first available capture. This is what lets `get_skill_info` label
        // internal effect ids ("Effect") instead of failing, and it happens on
        // the worker (not the startup API worker) because the scan needs a
        // valid game context. Retried until it succeeds (a content store that
        // fails the anchor check returns 0 quickly), so a transiently
        // half-loaded client does not permanently skip the classification.
        if !self.content_synced.load(Ordering::Acquire) {
            let count = crate::catalog::ingest_content_skills();
            if count > 0 {
                self.content_synced.store(true, Ordering::Release);
                log::info!("[reffect-runtime] ingested {count} runtime content skills");
            }
        }
        let mut buffs = self
            .buffs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let snapshot = decode_snapshot(
            &ProcessMemory,
            captured.collection,
            captured.local,
            &mut buffs,
        );
        // Runs on the worker rather than in `decode_snapshot` because it reads
        // the Nexus RTAPI, which is unavailable in unit tests.
        probe_local_name(&ProcessMemory, captured.local);
        // TEMP DEBUG 2026-10-04: 定位 memory 链路（1Hz 限频，诊断完删除）。
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static LAST_LOG_SECS: AtomicU64 = AtomicU64::new(0);
            let secs = std::time::Instant::now().elapsed().as_secs();
            let last = LAST_LOG_SECS.load(Ordering::Relaxed);
            if secs != last
                && LAST_LOG_SECS
                    .compare_exchange(last, secs, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                let telemetry = hook::capture_telemetry();
                match &snapshot {
                    Some(snap) => log::debug!(
                        "[reffect-runtime] snap ok: char={:X} prof={:?} res={} sb={} build={} gear={} ac={} tgt={} captures={} hook_status={:?}",
                        captured.local,
                        snap.profession,
                        snap.resources.is_some(),
                        snap.skillbar.is_some(),
                        snap.build.is_some(),
                        snap.gear.is_some(),
                        snap.active_cooldowns.as_ref().map_or(0, Vec::len),
                        snap.target.is_some(),
                        hook::capture_count(),
                        telemetry.status()
                    ),
                    None => log::debug!(
                        "[reffect-runtime] snap none: char={:X} captures={} hook_status={:?}",
                        captured.local,
                        hook::capture_count(),
                        telemetry.status()
                    ),
                }
            }
        }
        let mut snapshot = snapshot?;
        snapshot.group = read_group_info(&ProcessMemory, captured, &mut buffs, snapshot.now);
        Some(snapshot)
    }
}

/// Stateless worker-side reader. The game thread owns Context TLS; workers
/// consume only the already-published character pointer through this type.
#[derive(Debug, Clone, Copy)]
struct ProcessMemory;

trait GameMemory {
    fn read<T: Copy>(&self, address: usize) -> Option<T>;
}

impl GameMemory for ProcessMemory {
    fn read<T: Copy>(&self, address: usize) -> Option<T> {
        read_process_memory(address).ok()
    }
}

#[derive(Debug, Clone)]
struct RuntimeSnapshot {
    now: u32,
    race: Option<u8>,
    profession: Option<u8>,
    specialization: Option<u32>,
    resources: Option<PlayerResourceSnapshot>,
    buffs: Option<PlayerBuffInfo>,
    build: Option<Build>,
    gear: Option<GearSnapshot>,
    skillbar: Option<Skillbar>,
    active_cooldowns: Option<Vec<ActiveCooldown>>,
    target: Option<TargetSnapshot>,
    group: Option<GroupInfo>,
}

/// Player health and profession resources. The public context keeps these in
/// separate fields ([`PlayerInfo::health`] and [`PlayerInfo::resources`]), so
/// the reader returns them together and the publisher splits them.
#[derive(Debug, Clone)]
struct PlayerResourceSnapshot {
    health: PlayerHealth,
    resources: PlayerResources,
}

/// Skillbar plus the cooldowns the game is tracking that are not anchored to a
/// current skillbar slot (e.g. the inactive weapon set, kit/elite skills).
#[derive(Debug, Clone)]
struct SkillbarSnapshot {
    bar: Skillbar,
    active: Vec<ActiveCooldown>,
}

#[derive(Debug, Clone)]
struct GearSnapshot {
    weapons: BitFlags<Weapon>,
    sigils: [u32; 4],
    relic: u32,
}

impl GearSnapshot {
    fn resolve(self) -> Gear {
        Gear {
            weapons: self.weapons,
            sigils: self.sigils,
            relic: self.relic,
        }
    }
}

#[derive(Debug, Clone)]
struct TargetSnapshot {
    /// Disposition when readable, otherwise the reason it is not.
    affinity: Result<Affinity, Error>,
    /// Classification when readable, otherwise the reason it is not.
    category: Result<BitFlags<TargetCategory>, Error>,
    /// Resources when readable, or 'Error::TargetState' when the resolved
    /// character carries no health object (large map-break gadgets).
    resources: Result<CombatantResources, Error>,
    /// Buffs when readable; 'Error::TargetState' mirrors the original's gadget
    /// path (buffs unreadable), 'Error::Buffs' a genuinely missing tree.
    buffs: Result<BuffMap, Error>,
}

#[derive(Debug, Clone, Copy)]
struct Recharge {
    /// Remaining recharge (ms) as observed at `anchored_at`.
    remaining: u32,
    total: u32,
    rate: f32,
    /// Tick `remaining` is anchored to.
    anchored_at: u32,
}

#[derive(Debug, Clone, Copy)]
struct AmmoCooldown {
    ammo: u32,
    recharge: u32,
    /// Remaining ammo recharge (ms) as observed at `anchored_at`.
    recharge_remaining: u32,
    /// Tick `recharge_remaining` is anchored to.
    anchored_at: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Cooldown {
    recharge: Option<Recharge>,
    ammo: Option<AmmoCooldown>,
}

fn decode_snapshot(
    game: &impl GameMemory,
    collection: usize,
    character: usize,
    buff_cache: &mut BuffSnapshotCache,
) -> Option<RuntimeSnapshot> {
    // An agent pointer is a cheap, version-stable character-shape check and
    // prevents the context's transient login-shell object from being published.
    pointer_at(game, character, CHARACTER_AGENT)?;

    let now = tick_count();
    let (race, profession) = read_identity(game, character);
    let (mut build, build_slot) = match read_build(game, character) {
        Some((build, slot)) => (Some(build), Some(slot)),
        None => (None, None),
    };
    let specialization = build
        .as_ref()
        .and_then(|build| (build.specs[2] != 0).then_some(build.specs[2]));
    let resources =
        read_player_resources_for_profession_at(game, character, profession, specialization, now);
    let buffs = read_player_buff_info(game, buff_cache, character, now);
    let gear = read_gear(game, character);
    let skillbar = read_skillbar(game, character, now);
    if let (Some(build), Some(skillbar), Some(slot)) =
        (build.as_mut(), skillbar.as_ref(), build_slot)
    {
        // Equipped skills come from the build slot's per-skill tables (correct
        // even while a skill is transformed); the profession-selection ids (e.g.
        // the Evoker familiar) are appended and drive the profession selection.
        let equipped_skills = read_selected_skills(game, slot, &build.specs);
        let selection_skills = read_selection_skills(game, slot, &build.specs);
        let (skill_selections, prof_selections) = build_runtime_selections(
            profession,
            &skillbar.bar,
            &equipped_skills,
            &selection_skills,
        );
        build.skill_selections = skill_selections;
        build.prof_selections = prof_selections;
    }
    let target = read_target(game, collection, character, buff_cache, now);
    if target.is_none() {
        // No selection: forget the probed target so re-selecting it dumps again
        // and the debug window stops advertising a stale sequence.
        clear_target_probe();
    }

    Some(RuntimeSnapshot {
        now,
        race,
        profession,
        specialization,
        resources,
        buffs,
        build,
        gear,
        skillbar: skillbar.as_ref().map(|snapshot| snapshot.bar.clone()),
        active_cooldowns: skillbar.map(|snapshot| snapshot.active),
        target,
        group: None,
    })
}

impl RuntimeSnapshot {
    fn apply(self) {
        let mut context = Context::lock();
        self.apply_to(&mut context);
    }

    fn apply_to(self, context: &mut Context) {
        let gear = self.gear.map(GearSnapshot::resolve);
        context.now = self.now;

        if let Some(race) = self.race
            && let Ok(race) = Race::try_from(race)
        {
            context.player.race = Ok(race);
            context.updates.insert(Update::PlayerIdentity);
        }
        if let Some(profession) = self.profession
            && let Ok(profession) = Profession::try_from(profession)
        {
            context.player.prof = Ok(profession);
            if let Some(specialization) = self.specialization {
                context.player.spec = Specialization::try_from(Some(profession), specialization)
                    .ok_or(specialization);
            }
            context.updates.insert(Update::PlayerIdentity);
        }

        match self.resources {
            Some(resources) => {
                context.player.health = Ok(resources.health);
                context.player.resources = Ok(resources.resources);
            }
            None => {
                context.player.health = Err(Error::Health);
                context.player.resources = Err(Error::Profession);
            }
        }
        // Player buffs are filtered at read time by the effect's definition
        // pointer (see `read_buffs_at`), matching the original decoder, so the
        // buff map is published as-is.
        context.player.buff_info = self.buffs.ok_or(Error::Buffs);
        context.player.build = self.build.ok_or(Error::Build);
        context.player.gear = gear.ok_or(Error::Inventory);
        context.player.skillbar = self.skillbar.ok_or(Error::Skillbar);
        context.player.active_cooldowns = self.active_cooldowns.ok_or(Error::Skillbar);
        context.updates.insert(
            Update::PlayerResources.flags()
                | Update::PetResources.flags()
                | Update::PlayerBuffs.flags()
                | Update::PlayerBuild.flags()
                | Update::PlayerGear.flags()
                | Update::PlayerSkillbar.flags()
                | Update::PlayerActiveCooldowns.flags(),
        );

        match self.target {
            Some(target) => {
                context.target.affinity = target.affinity;
                context.target.category = target.category;
                context.target.resources = target.resources;
                context.target.buffs = target.buffs;
            }
            None => context.target.set_error(Error::Target),
        }
        context
            .updates
            .insert(Update::TargetResources.flags() | Update::TargetBuffs.flags());

        context.group = self.group.ok_or(Error::Group);
        context.updates.insert(
            Update::GroupIdentity.flags()
                | Update::GroupResources.flags()
                | Update::GroupBuffs.flags(),
        );
    }
}

/// Reads the group type from Nexus and member combat state from the validated
/// client-side group tree. Nexus intentionally exposes no current member list;
/// the game tree is therefore required before publishing a group snapshot.
fn read_group_info(
    game: &impl GameMemory,
    captured: CapturedContext,
    cache: &mut BuffSnapshotCache,
    now: u32,
) -> Option<GroupInfo> {
    let group_type = RealTimeApi::get()
        .and_then(|rtapi| rtapi.read_group())
        .map(|group| group_type_from_rtapi(group.group_type))
        .unwrap_or(GroupType::Solo);

    Some(GroupInfo {
        group_type,
        members: read_group_members(game, captured.collection, captured.local, cache, now)?,
    })
}

fn read_group_members(
    game: &impl GameMemory,
    collection: usize,
    local: usize,
    cache: &mut BuffSnapshotCache,
    now: u32,
) -> Option<[GroupMember; GroupInfo::MAX_MEMBERS]> {
    let root = pointer_at(game, collection, GROUP_CONTEXT_ROOT)?;
    let directories = pointer_at(game, root, GROUP_DIRECTORY_TABLE)?;
    let local_identity = read_group_identity(game, local)?;
    let characters = read_group_characters(game, local)?;

    for directory_index in 0..GROUP_DIRECTORY_COUNT {
        let directory = directories.checked_add(directory_index * GROUP_DIRECTORY_STRIDE)?;
        if value_at::<u32>(game, directory, GROUP_DIRECTORY_ACTIVE) != Some(1) {
            continue;
        }
        let count = usize::try_from(value_at::<u32>(
            game,
            directory,
            GROUP_DIRECTORY_IDENTITY_COUNT,
        )?)
        .ok()?;
        if count == 0 || count > MAX_GROUP_IDENTITIES {
            continue;
        }
        let identities = pointer_at(game, directory, GROUP_DIRECTORY_IDENTITIES)?;
        let local_is_member = (0..count)
            .any(|index| group_identity_at(game, identities, index) == Some(local_identity));
        if !local_is_member {
            continue;
        }

        let mut members = std::array::from_fn(|_| GroupMember::empty());
        let mut slot = 0;
        for identity_index in 0..count {
            let identity = group_identity_at(game, identities, identity_index)?;
            if identity == local_identity {
                continue;
            }
            let Some(character) = characters
                .iter()
                .find_map(|(candidate_identity, character)| {
                    (*candidate_identity == identity).then_some(*character)
                })
            else {
                continue;
            };
            members[slot] = GroupMember {
                account: None,
                resources: read_normalized_combatant_resources(game, character, now)
                    .ok_or(Error::Health),
                buffs: cache
                    .read(game, character, now)
                    .map(|mut buffs| {
                        filter_group_buffs(&mut buffs);
                        buffs
                    })
                    .ok_or(Error::Buffs),
            };
            slot += 1;
            if slot == GroupInfo::MAX_MEMBERS {
                break;
            }
        }
        return Some(members);
    }
    None
}

fn read_group_characters(game: &impl GameMemory, local: usize) -> Option<Vec<((u64, u64), usize)>> {
    let member_table = pointer_at(game, local, CHARACTER_GROUP_MEMBERS)
        .or_else(|| pointer_at(game, local, CHARACTER_GROUP_MEMBERS_FALLBACK))?;
    let entries = pointer_at(game, member_table, GROUP_MEMBER_ENTRIES)?;
    let count = usize::try_from(value_at::<u32>(game, member_table, GROUP_MEMBER_COUNT)?).ok()?;
    if count > MAX_GROUP_MEMBER_ENTRIES {
        return None;
    }

    let mut characters = Vec::with_capacity(count);
    for index in 0..count {
        let Some(entry_offset) = index.checked_mul(size_of::<usize>()) else {
            continue;
        };
        let Some(entry) = pointer_at(game, entries, entry_offset) else {
            continue;
        };
        let Some(character) = pointer_at(game, entry, GROUP_MEMBER_CHARACTER) else {
            continue;
        };
        let Some(identity) = read_group_identity(game, character) else {
            continue;
        };
        if !characters
            .iter()
            .any(|(existing_identity, _)| *existing_identity == identity)
        {
            characters.push((identity, character));
        }
    }
    Some(characters)
}

fn read_group_identity(game: &impl GameMemory, character: usize) -> Option<(u64, u64)> {
    read_identity_words(game, character.checked_add(CHARACTER_GROUP_IDENTITY)?)
}

fn group_identity_at(
    game: &impl GameMemory,
    identities: usize,
    index: usize,
) -> Option<(u64, u64)> {
    let offset = index.checked_mul(GROUP_IDENTITY_STRIDE)?;
    read_identity_words(game, identities.checked_add(offset)?)
}

fn read_identity_words(game: &impl GameMemory, base: usize) -> Option<(u64, u64)> {
    Some((
        value_at::<u64>(game, base, 0)?,
        value_at::<u64>(game, base, size_of::<u64>())?,
    ))
}

fn group_type_from_rtapi(group_type: std::result::Result<RtapiGroupType, u32>) -> GroupType {
    match group_type {
        Ok(RtapiGroupType::Party) => GroupType::Party,
        Ok(RtapiGroupType::RaidSquad | RtapiGroupType::Squad) => GroupType::Squad,
        Ok(RtapiGroupType::None) | Err(_) => GroupType::Solo,
    }
}

fn read_identity(game: &impl GameMemory, character: usize) -> (Option<u8>, Option<u8>) {
    let Some(core) = pointer_at(game, character, CHARACTER_CORE_STATS) else {
        return (None, None);
    };
    let race = value_at::<u8>(game, core, CORE_RACE).filter(|race| *race <= 4);
    let profession = value_at::<u32>(game, core, CORE_PROFESSION)
        .and_then(|profession| u8::try_from(profession).ok())
        .filter(|profession| (1..=9).contains(profession));
    (race, profession)
}

#[cfg(test)]
fn read_player_resources(
    game: &impl GameMemory,
    character: usize,
) -> Option<PlayerResourceSnapshot> {
    read_player_resources_at(game, character, tick_count())
}

#[cfg(test)]
fn read_player_resources_at(
    game: &impl GameMemory,
    character: usize,
    now: u32,
) -> Option<PlayerResourceSnapshot> {
    read_player_resources_for_profession_at(
        game,
        character,
        read_identity(game, character).1,
        None,
        now,
    )
}

fn read_player_resources_for_profession_at(
    game: &impl GameMemory,
    character: usize,
    profession: Option<u8>,
    specialization: Option<u32>,
    now: u32,
) -> Option<PlayerResourceSnapshot> {
    let combatant = read_combatant_resources(game, character, now)?;
    let pet = owned_pet_character(game, character)
        .and_then(|pet| read_combatant_resources(game, pet, now));
    let endurance = pointer_at(game, character, CHARACTER_ENDURANCE)
        .and_then(|endurance| {
            let current = value_at::<u32>(game, endurance, ENDURANCE_CURRENT)? as f32;
            let max = value_at::<u32>(game, endurance, ENDURANCE_MAX)? as f32;
            sane_resource(current, max).then_some(Resource::new(current, max))
        })
        .unwrap_or_else(Resource::empty);
    let primary = if profession == Some(5) {
        pointer_at(game, character, CHARACTER_PROFESSION)
            .and_then(|thief| {
                let base_current = value_at::<f32>(game, thief, THIEF_PRIMARY_CURRENT)?;
                let max = value_at::<f32>(game, thief, THIEF_PRIMARY_MAX)?;
                let sample_tick =
                    value_at::<u32>(game, thief, THIEF_PRIMARY_SAMPLE_TICK).unwrap_or(now);
                let rate = value_at::<f32>(game, thief, THIEF_PRIMARY_RATE)
                    .filter(|rate| rate.is_finite())
                    .unwrap_or(0.0);
                let elapsed = now.saturating_sub(sample_tick) as f32 / 1_000.0;
                let current = (base_current + elapsed * rate).clamp(0.0, max);
                sane_resource(current, max).then_some(Resource::new(current, max))
            })
            .unwrap_or_else(Resource::empty)
    } else {
        pointer_at(game, character, CHARACTER_PROFESSION_ENERGY)
            .and_then(|energy| {
                let base_current = value_at::<f32>(game, energy, PROFESSION_ENERGY_CURRENT)?;
                let max = value_at::<f32>(game, energy, PROFESSION_ENERGY_MAX)?;
                if !base_current.is_finite() || !max.is_finite() || max <= 0.0 {
                    return None;
                }
                let sample_tick =
                    value_at::<u32>(game, energy, PROFESSION_ENERGY_SAMPLE_TICK).unwrap_or(now);
                let rate = value_at::<f32>(game, energy, PROFESSION_ENERGY_RATE)
                    .filter(|rate| rate.is_finite())
                    .unwrap_or(0.0);
                let elapsed = now.saturating_sub(sample_tick) as f32 / 1_000.0;
                let current = (base_current + elapsed * rate).clamp(0.0, max);
                sane_resource(current, max).then_some(Resource::new(current, max))
            })
            .unwrap_or_else(Resource::empty)
    };
    let (primary, rate) = match profession {
        // Revenant's displayed energy is read from the generic
        // `character + 0x3d8` sampled-energy object, not from the profession
        // object. The original master reader (`sub_1800E26B0` case 9) reads the
        // primary from `profObj + 0x50/0x54` (with the regen rate at `+0x58` and
        // sample tick at `+0x5C`), but on the current client that object is a
        // distinct profession implementation and produced a permanently full
        // bar, so the generic `character + 0x3d8` snapshot is used instead. This
        // is a deliberate, live-verified divergence from the 0.13.1 decompile;
        // re-confirm against the running client before changing it.
        Some(9) => (
            primary,
            read_revenant_rate(game, character).unwrap_or_else(Resource::empty),
        ),
        // Guardian reads a profession primary only for the Willbender elite
        // spec (virtue charges); every other guardian shows no primary.
        Some(1) => (
            if specialization == Some(SPEC_GUARDIAN_WILLBENDER) {
                read_willbender_primary(game, character).unwrap_or_else(Resource::empty)
            } else {
                primary
            },
            Resource::empty(),
        ),
        Some(2 | 7 | 8) => (
            read_fixed_profession_primary(game, character, profession)
                .unwrap_or_else(Resource::empty),
            Resource::empty(),
        ),
        _ => (primary, Resource::empty()),
    };
    let (primary, secondary) = if profession == Some(6) {
        let (elementalist_primary, elementalist_secondary) =
            read_elementalist_charge_resources(game, character, specialization);
        (
            elementalist_primary.unwrap_or(primary),
            elementalist_secondary.unwrap_or_else(Resource::empty),
        )
    } else {
        (
            primary,
            read_specialized_secondary(game, character, profession, specialization)
                .unwrap_or_else(Resource::empty),
        )
    };
    let health_reduction = Resource::new(
        (combatant.health.max - combatant.health.current).max(0.0),
        combatant.health.max,
    );

    Some(PlayerResourceSnapshot {
        health: PlayerHealth {
            combatant,
            health_reduction,
        },
        resources: PlayerResources {
            pet,
            endurance,
            primary,
            secondary,
            rate,
        },
    })
}

/// The generic sampled-energy object stores the signed Revenant regeneration
/// rate.  The public resource surface represents that `-5..=5` range as
/// `0..=10`, with neutral regeneration at five.
fn read_revenant_rate(game: &impl GameMemory, character: usize) -> Option<Resource> {
    let energy = pointer_at(game, character, CHARACTER_PROFESSION_ENERGY)?;
    let current = value_at::<f32>(game, energy, PROFESSION_ENERGY_RATE)? + REVENANT_RATE_BASE;
    sane_resource(current, REVENANT_RATE_MAX).then_some(Resource::new(current, REVENANT_RATE_MAX))
}

/// Reads profession resources whose original decoder uses a fixed pair rather
/// than the generic character-energy object.
fn read_fixed_profession_primary(
    game: &impl GameMemory,
    character: usize,
    profession: Option<u8>,
) -> Option<Resource> {
    let profession_object = pointer_at(game, character, CHARACTER_PROFESSION)?;
    match profession? {
        // Warrior adrenaline and Mesmer clone count are stored as integers.
        2 | 7 => {
            let current =
                value_at::<u32>(game, profession_object, PROFESSION_PRIMARY_CURRENT)? as f32;
            let max = value_at::<u32>(game, profession_object, PROFESSION_PRIMARY_MAX)? as f32;
            sane_resource(current, max).then_some(Resource::new(current, max))
        }
        // Necromancer life force is stored as raw points and displayed as a
        // whole-number pair (e.g. 1463/19228). The max routinely exceeds the
        // generic `sane_resource` cap of 1000 (verified live at 19228 against
        // 0.13.1), so it uses a life-force-specific bound; warrior adrenaline
        // and mesmer clones stay well under 1000 and keep `sane_resource`.
        8 => {
            let current =
                value_at::<f32>(game, profession_object, PROFESSION_PRIMARY_CURRENT)?.ceil();
            let max = value_at::<f32>(game, profession_object, PROFESSION_PRIMARY_MAX)?.ceil();
            sane_life_force(current, max).then_some(Resource::new(current, max))
        }
        _ => None,
    }
}

/// Reads the Elementalist profession charges from the profession object
/// (`character + 0x510`), matching the original per-profession reader
/// `sub_1800E26B0` (case 6). The interpretation is selected by the active
/// elite specialization:
///
/// * Evoker (id 80): basic charges are two u32 counts at `profObj+0x80/0x84`
///   divided by 5.0 (live-verified: 20/30 -> 4/6). The enhanced charges
///   (secondary, max 3) are counted from the character effect tree by
///   `read_evoker_enhanced_current`.
/// * Catalyst (id 67): the same `profObj+0x80/0x84` pair as raw energy.
/// * Any other spec: no elementalist-specific resource.
fn read_elementalist_charge_resources(
    game: &impl GameMemory,
    character: usize,
    specialization: Option<u32>,
) -> (Option<Resource>, Option<Resource>) {
    let Some(profession_object) = pointer_at(game, character, CHARACTER_PROFESSION) else {
        return (None, None);
    };
    match specialization {
        Some(SPEC_ELEMENTALIST_EVOKER) => {
            let primary =
                read_scaled_charge(game, profession_object, ELEMENTALIST_EVOKER_CHARGE_DIVISOR);
            // The enhanced charges are a runtime effect-stack count in the
            // original, derived from the character effect tree rather than a
            // profession-object field.
            let current = read_evoker_enhanced_current(game, character).min(3) as f32;
            let secondary = Some(Resource::new(current, ELEMENTALIST_SECONDARY_MAX));
            (primary, secondary)
        }
        Some(SPEC_ELEMENTALIST_CATALYST) => {
            (read_scaled_charge(game, profession_object, 1.0), None)
        }
        _ => (None, None),
    }
}

/// Reads the `profObj+0x80/0x84` u32 charge pair and divides both by `divisor`
/// (5.0 for Evoker basic charges, 1.0 for Catalyst's raw energy).
fn read_scaled_charge(
    game: &impl GameMemory,
    profession_object: usize,
    divisor: f32,
) -> Option<Resource> {
    let current = value_at::<u32>(game, profession_object, ELEMENTALIST_CHARGE_CURRENT)? as f32;
    let max = value_at::<u32>(game, profession_object, ELEMENTALIST_CHARGE_MAX)? as f32;
    let (current, max) = (current / divisor, max / divisor);
    (current.is_finite() && max.is_finite() && max > 0.0 && current <= max)
        .then_some(Resource::new(current, max))
}

/// Counts the Evoker enhanced-charge state from the character effect tree,
/// mirroring `sub_1800E26B0`'s spec-80 branch: a present "full" marker effect
/// reads as the full three charges, otherwise the current equals the number of
/// [`EVOKER_ENHANCED_STACK_ID`] effects present. Walks the same tree as
/// [`read_buffs_at`].
fn read_evoker_enhanced_current(game: &impl GameMemory, character: usize) -> u32 {
    let Some(container) = pointer_at(game, character, EFFECT_TREE_CONTAINER) else {
        return 0;
    };
    let Some(node_offset) = value_at::<u32>(game, container, EFFECT_TREE_NODE_OFFSET)
        .and_then(|offset| usize::try_from(offset).ok())
        .filter(|offset| *offset <= 0x100)
    else {
        return 0;
    };
    let next_offset = node_offset + size_of::<usize>();

    let mut count = 0;
    let mut seen = Vec::with_capacity(MAX_EFFECT_NODES);
    let mut node = raw_pointer_at(game, container, EFFECT_TREE_ROOT).unwrap_or(0);
    for _ in 0..MAX_EFFECT_NODES {
        if node & 1 != 0 || !valid_pointer(node) || seen.contains(&node) {
            break;
        }
        seen.push(node);
        let next = raw_pointer_at(game, node, next_offset).unwrap_or(0);

        if let Some(wrapper) = pointer_at(game, node, EFFECT_NODE_WRAPPER)
            && value_at::<u32>(game, wrapper, EFFECT_WRAPPER_STATE) == Some(1)
            && pointer_at(game, wrapper, EFFECT_WRAPPER_VALUE).is_some()
            && let Some(id) = value_at::<u32>(game, wrapper, EFFECT_WRAPPER_ID)
        {
            if EVOKER_ENHANCED_MARKER_IDS.contains(&id) {
                return 3;
            }
            if id == EVOKER_ENHANCED_STACK_ID {
                count += 1;
            }
        }

        node = next;
    }
    count
}

/// Walks the character effect tree and returns how many nodes carry the given
/// effect id. The `state == 1 && value present` gates match the other effect
/// readers so unapplied/empty effects are not counted. This is the same
/// confirmed walk used by the Evoker enhanced charge and by the original's
/// spec-68/74/79 secondaries.
fn count_effect_stacks(game: &impl GameMemory, character: usize, id: u32) -> u32 {
    let Some(container) = pointer_at(game, character, EFFECT_TREE_CONTAINER) else {
        return 0;
    };
    let Some(node_offset) = value_at::<u32>(game, container, EFFECT_TREE_NODE_OFFSET)
        .and_then(|offset| usize::try_from(offset).ok())
        .filter(|offset| *offset <= 0x100)
    else {
        return 0;
    };
    let next_offset = node_offset + size_of::<usize>();

    let mut count = 0;
    let mut seen = Vec::with_capacity(MAX_EFFECT_NODES);
    let mut node = raw_pointer_at(game, container, EFFECT_TREE_ROOT).unwrap_or(0);
    for _ in 0..MAX_EFFECT_NODES {
        if node & 1 != 0 || !valid_pointer(node) || seen.contains(&node) {
            break;
        }
        seen.push(node);
        let next = raw_pointer_at(game, node, next_offset).unwrap_or(0);

        if let Some(wrapper) = pointer_at(game, node, EFFECT_NODE_WRAPPER)
            && value_at::<u32>(game, wrapper, EFFECT_WRAPPER_STATE) == Some(1)
            && pointer_at(game, wrapper, EFFECT_WRAPPER_VALUE).is_some()
            && value_at::<u32>(game, wrapper, EFFECT_WRAPPER_ID) == Some(id)
        {
            count += 1;
        }

        node = next;
    }
    count
}

/// Guardian Willbender virtue charges: two signed integers on the profession
/// object (`profObj + 0x90/0x94`) from `sub_1800E26B0` case 1 / spec 62.
fn read_willbender_primary(game: &impl GameMemory, character: usize) -> Option<Resource> {
    let profession_object = pointer_at(game, character, CHARACTER_PROFESSION)?;
    let current = value_at::<u32>(game, profession_object, WILLBENDER_PRIMARY_CURRENT)? as f32;
    let max = value_at::<u32>(game, profession_object, WILLBENDER_PRIMARY_MAX)? as f32;
    sane_resource(current, max).then_some(Resource::new(current, max))
}

/// Necromancer Harbinger blight: a signed integer pair at `profObj + 0x58/0x5C`
/// from `sub_1800E26B0` case 8 / spec 60.
fn read_harbinger_secondary(game: &impl GameMemory, character: usize) -> Option<Resource> {
    let profession_object = pointer_at(game, character, CHARACTER_PROFESSION)?;
    let current = value_at::<u32>(game, profession_object, HARBINGER_CURRENT)? as f32;
    let max = value_at::<u32>(game, profession_object, HARBINGER_MAX)? as f32;
    sane_resource(current, max).then_some(Resource::new(current, max))
}

/// Reads the per-elite-spec secondary resources from `sub_1800E26B0` that are
/// not otherwise covered: the Warrior Berserker/Spellbreaker charge and the
/// Revenant Vindicator alliance charge count effect stacks; the Necromancer
/// Harbinger blight reads an integer pair from the profession object.
fn read_specialized_secondary(
    game: &impl GameMemory,
    character: usize,
    profession: Option<u8>,
    specialization: Option<u32>,
) -> Option<Resource> {
    match (profession?, specialization?) {
        (2, SPEC_WARRIOR_BERSERKER) => Some(Resource::new(
            count_effect_stacks(game, character, WARRIOR_BERSERKER_BURST_ID).min(10) as f32,
            WARRIOR_BERSERKER_MAX,
        )),
        (2, SPEC_WARRIOR_SPELLBREAKER) => Some(Resource::new(
            count_effect_stacks(game, character, WARRIOR_SPELLBREAKER_STATE_ID).min(10) as f32,
            10.0,
        )),
        (8, SPEC_NECROMANCER_HARBINGER) => read_harbinger_secondary(game, character),
        (9, SPEC_REVENANT_VINDICATOR) => Some(Resource::new(
            count_effect_stacks(game, character, VINDICATOR_ALLIANCE_ID).min(5) as f32,
            5.0,
        )),
        _ => None,
    }
}

fn read_combatant_resources(
    game: &impl GameMemory,
    character: usize,
    now: u32,
) -> Option<CombatantResources> {
    read_combatant_resources_with_normalization(game, character, false, now)
}

fn read_normalized_combatant_resources(
    game: &impl GameMemory,
    character: usize,
    now: u32,
) -> Option<CombatantResources> {
    read_combatant_resources_with_normalization(game, character, true, now)
}

fn read_combatant_resources_with_normalization(
    game: &impl GameMemory,
    character: usize,
    normalize: bool,
    now: u32,
) -> Option<CombatantResources> {
    let health = pointer_at(game, character, CHARACTER_HEALTH)?;
    read_resources_from_health(
        game,
        health,
        BARRIER_LAYOUT_CHARACTER,
        normalize,
        now,
        read_defiance(game, character),
    )
}

/// Reads combatant resources from an already resolved health object. Target
/// kinds that keep their health outside the character layout (see read_target)
/// pass that object directly.
fn read_resources_from_health(
    game: &impl GameMemory,
    health: usize,
    barrier_layout: BarrierLayout,
    normalize: bool,
    now: u32,
    defiance: Option<Defiance>,
) -> Option<CombatantResources> {
    let base_current = value_at::<f32>(game, health, HEALTH_CURRENT)?;
    let max = value_at::<f32>(game, health, HEALTH_MAX)?;
    if !sane_health(base_current, max) {
        return None;
    }
    // Mirror the original reader (`collector_E2EC0`): the health object stores
    // the last-tick sample, so the displayed value is `base + (now - tick)/1000
    // * rate` clamped to max. Without this, the bar lags the actual health
    // during regeneration.
    let current = interpolate_sampled(
        game,
        health,
        HEALTH_SAMPLE_TICK,
        HEALTH_RATE,
        base_current,
        max,
        now,
    );
    let barrier = value_at::<f32>(game, health, barrier_layout.current)
        .filter(|barrier| barrier.is_finite() && *barrier >= 0.0)
        .map(|base_barrier| {
            let barrier_max = value_at::<f32>(game, health, barrier_layout.max)
                .filter(|barrier_max| barrier_max.is_finite() && *barrier_max >= 0.0)
                .unwrap_or(max);
            interpolate_sampled(
                game,
                health,
                barrier_layout.tick,
                barrier_layout.rate,
                base_barrier,
                barrier_max,
                now,
            )
        })
        .filter(|barrier| *barrier <= max * 1.25)
        .unwrap_or(0.0);
    Some(CombatantResources::new(
        normalize,
        // TODO(health-state): the client field that distinguishes alive/downed/
        // dead is not part of the recovered 0.13.1 read graph, so every combatant
        // is published as alive for now. Tracked with the target classification
        // work; see docs/superpowers/plans.
        ResourceState::HealthAlive,
        current,
        barrier,
        max,
        defiance,
    ))
}

/// `current = (base + (now - tick) / 1000 * rate).clamp(0, max)` — the original
/// sampled-resource read. Missing tick/rate fields collapse to `base`, so a
/// fixture without them still returns the stored current unchanged.
fn interpolate_sampled(
    game: &impl GameMemory,
    object: usize,
    tick_offset: usize,
    rate_offset: usize,
    base_current: f32,
    max: f32,
    now: u32,
) -> f32 {
    let tick = value_at::<u32>(game, object, tick_offset).unwrap_or(now);
    let rate = value_at::<f32>(game, object, rate_offset)
        .filter(|rate| rate.is_finite())
        .unwrap_or(0.0);
    let elapsed = now.saturating_sub(tick) as f32 / 1_000.0;
    (base_current + elapsed * rate).clamp(0.0, max)
}

fn owned_pet_character(game: &impl GameMemory, character: usize) -> Option<usize> {
    pointer_at(game, character, CHARACTER_KENNEL)
        .and_then(|kennel| pointer_at(game, kennel, KENNEL_PRIMARY_PET))
        .filter(|pet| is_owned_companion_address(character, *pet))
}

/// Reads the defiance bar. `None` means the combatant has no readable defiance
/// bar, which the public context represents as `Option<Defiance>`.
fn read_defiance(game: &impl GameMemory, character: usize) -> Option<Defiance> {
    read_defiance_at(game, pointer_at(game, character, CHARACTER_DEFIANCE))
}

/// Reads the defiance bar from an already resolved defiance object; target kinds
/// that keep it off the character pass that object directly.
fn read_defiance_at(game: &impl GameMemory, defiance: Option<usize>) -> Option<Defiance> {
    let defiance = defiance?;
    let state = value_at::<u32>(game, defiance, DEFIANCE_STATE)?;
    let mut value = value_at::<f32>(game, defiance, DEFIANCE_VALUE)?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    if value <= 1.5 {
        value *= 100.0;
    }
    if value > 100.0 {
        return None;
    }
    match state {
        0 => Some(Defiance::Active(value)),
        1 => Some(Defiance::Recover(value)),
        2 => Some(Defiance::Immune),
        _ => None,
    }
}

fn read_skillbar(game: &impl GameMemory, character: usize, now: u32) -> Option<SkillbarSnapshot> {
    let combatant = pointer_at(game, character, CHARACTER_COMBATANT)?;
    let mut cooldowns = BTreeMap::new();
    collect_cooldowns(game, combatant, &mut cooldowns, now);

    let pet_combatant = owned_pet_character(game, character)
        .and_then(|pet| pointer_at(game, pet, CHARACTER_COMBATANT));
    if let Some(pet_combatant) = pet_combatant {
        collect_cooldowns(game, pet_combatant, &mut cooldowns, now);
    }

    // Anchor the merged snapshots once per skill, so every consumer publishes the
    // same stable `last_update` for the remaining it observed.
    anchor_cooldowns(&mut cooldowns, now);

    let mut skillbar = Skillbar::empty();
    let mut any = false;
    if let Some(weapon_swap) = read_weapon_swap(game, character, combatant) {
        skillbar.set_slot(Slot::WeaponSwap, Some(weapon_swap));
        any = true;
    }
    for (raw, slot) in slot_map() {
        if let Some((id, definition)) = read_skill_definition(game, combatant, raw) {
            skillbar.set_slot(
                slot,
                Some(build_ability(
                    game, combatant, raw, id, definition, now, &cooldowns,
                )),
            );
            any = true;
        }
    }

    // Mechanist and pet-command skills live on the owned pet's combatant, which
    // indexes its skills differently from a player combatant. They only fill a
    // genuinely empty profession slot and never replace a local skilldef,
    // matching the original reader's `sub_1800E2560`.
    if let Some(pet_combatant) = pet_combatant {
        for (raw, slot) in PET_SKILL_SLOTS {
            if skillbar.slot(slot).is_none()
                && let Some((id, definition)) = read_skill_definition(game, pet_combatant, raw)
            {
                skillbar.set_slot(
                    slot,
                    Some(build_ability(
                        game,
                        pet_combatant,
                        raw,
                        id,
                        definition,
                        now,
                        &cooldowns,
                    )),
                );
                any = true;
            }
        }
    }

    log_profession_slots(game, &skillbar, pet_combatant);

    let active = build_active_cooldowns(&skillbar, &cooldowns, now);
    any.then_some(SkillbarSnapshot {
        bar: skillbar,
        active,
    })
}

/// Persistent per-skill anchor for the game's coarse cooldown snapshots.
///
/// The reader only polls the cooldown container every [`STATE_POLL`] (100 ms),
/// while the game rewrites its cooldown nodes on its own schedule. Publishing
/// the read time as `last_update` therefore re-anchors the countdown on every
/// poll: the displayed number jumps back up to the snapshot value each poll (a
/// ~100 ms sawtooth) and the projected end time never advances, which freezes
/// the duration bar and the percentage. Anchoring to the tick a cooldown state
/// was observed and projecting the remaining forward from there (the original
/// reader's `remaining = total - elapsed`) keeps the countdown, the duration bar
/// and the percentage ticking smoothly between polls.
#[derive(Debug, Clone, Copy, Default)]
struct CooldownAnchor {
    /// Snapshot remaining (ms) observed at `anchored_at`.
    remaining: u32,
    /// Tick `remaining` is anchored to.
    anchored_at: u32,
    /// Raw node snapshot of the previous pass.
    last_snap: u32,
    /// Whether the anchor holds an observed cooldown.
    valid: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct CooldownTrack {
    recharge: CooldownAnchor,
    ammo: CooldownAnchor,
    /// Tick the skill was last seen cooling down.
    seen_at: u32,
    /// Whether the skill was seen cooling down before.
    seen: bool,
}

/// Margin (ms) by which the game's snapshot has to exceed the previous snapshot
/// before it is treated as a fresh cast rather than node rewrite noise.
const COOLDOWN_RESTART_MARGIN: u32 = 1_000;

/// Margin (ms) by which the projected remaining has to exceed the game's
/// snapshot before the snapshot is adopted as a cooldown reduction.
const COOLDOWN_CORRECTION_MARGIN: u32 = 250;

/// Time (ms) a skill has to be missing from the cooldown list before a
/// returning snapshot is treated as a new cooldown. Several times the 100 ms
/// poll interval, so a single delayed or missed walk is not a cooldown end.
const COOLDOWN_GAP_MS: u32 = 500;

/// How long an anchor is kept after the skill was last seen cooling down.
const COOLDOWN_TRACK_TTL: u32 = 10_000;

fn cooldown_tracks() -> &'static Mutex<HashMap<u32, CooldownTrack>> {
    static TRACKS: OnceLock<Mutex<HashMap<u32, CooldownTrack>>> = OnceLock::new();
    TRACKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Anchors every cooldown snapshot of the pass, so the skillbar abilities and
/// the off-bar cooldown list both publish the remaining together with the tick
/// they observed it at.
fn anchor_cooldowns(cooldowns: &mut BTreeMap<u32, Cooldown>, now: u32) {
    let mut tracks = cooldown_tracks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (id, cooldown) in cooldowns.iter_mut() {
        let track = tracks.entry(*id).or_default();
        // A node that was gone for a while cannot be the continuation of the
        // anchored cooldown: it is a new one, even when the game writes the same
        // static value again.
        let restarted = track.seen && now.wrapping_sub(track.seen_at) > COOLDOWN_GAP_MS;
        track.seen = true;
        track.seen_at = now;
        if let Some(recharge) = cooldown.recharge.as_mut() {
            anchor_cooldown(
                &mut track.recharge,
                recharge.remaining,
                recharge.total,
                recharge.rate,
                now,
                restarted,
            );
            recharge.remaining = track.recharge.remaining;
            recharge.anchored_at = track.recharge.anchored_at;
        }
        if let Some(ammo) = cooldown.ammo.as_mut() {
            anchor_cooldown(
                &mut track.ammo,
                ammo.recharge_remaining,
                ammo.recharge,
                1.0,
                now,
                restarted,
            );
            ammo.recharge_remaining = track.ammo.remaining;
            ammo.anchored_at = track.ammo.anchored_at;
        }
    }
    // Forget skills that stopped cooling down. Age based, so a concurrent pass
    // over a different cooldown set cannot drop the anchors of this one.
    tracks.retain(|_, track| now.wrapping_sub(track.seen_at) < COOLDOWN_TRACK_TTL);
}

/// Anchors a single cooldown snapshot, ignoring node rewrite noise so the
/// projected countdown never ticks backwards.
fn anchor_cooldown(
    anchor: &mut CooldownAnchor,
    snap: u32,
    total: u32,
    rate: f32,
    now: u32,
    restarted: bool,
) {
    let snap = if total > 0 { snap.min(total) } else { snap };
    let projected = if anchor.valid {
        (anchor.remaining as f32 - now.saturating_sub(anchor.anchored_at) as f32 * rate).max(0.0)
    } else {
        0.0
    };
    // Re-anchor on a first observation, a fresh cast (the node grew, or the node
    // was gone and came back), a cooldown reduction (the node dropped below the
    // projection) or a live node outliving our projection. Everything else is
    // rewrite noise: moving the anchor there would make the displayed countdown
    // tick backwards on every poll.
    let fresh = !anchor.valid
        || restarted
        || snap > anchor.last_snap.saturating_add(COOLDOWN_RESTART_MARGIN)
        || (projected <= 0.0 && snap != anchor.last_snap);
    let corrected = anchor.valid && (snap as f32) + (COOLDOWN_CORRECTION_MARGIN as f32) < projected;
    if fresh || corrected {
        anchor.remaining = snap;
        anchor.anchored_at = now;
        anchor.valid = true;
    }
    anchor.last_snap = snap;
}

/// Produces the active-cooldown list from the cooldown-container walk, tagging
/// each with its skillbar slot when the skill is on the current bar and `None`
/// when it is cooling off-bar (e.g. the inactive weapon set, or a kit/elite-spec
/// skill). Only genuinely cooling entries are surfaced.
fn build_active_cooldowns(
    bar: &Skillbar,
    cooldowns: &BTreeMap<u32, Cooldown>,
    now: u32,
) -> Vec<ActiveCooldown> {
    let mut active = Vec::new();
    for (id, cooldown) in cooldowns {
        let mut entry = ActiveCooldown {
            skill: (*id).into(),
            slot: slot_for_skill(bar, *id),
            last_update: now,
            recharge_rate: 1.0,
            recharge: 0,
            recharge_remaining: 0,
            ammo: 0,
            ammo_recharge: 0,
            ammo_recharge_remaining: 0,
        };
        if let Some(recharge) = &cooldown.recharge {
            entry.last_update = recharge.anchored_at;
            entry.recharge = recharge.total;
            entry.recharge_rate = recharge.rate;
            entry.recharge_remaining = recharge.remaining;
        }
        if let Some(ammo) = &cooldown.ammo {
            entry.ammo = ammo.ammo;
            entry.ammo_recharge = ammo.recharge;
            entry.ammo_recharge_remaining = ammo.recharge_remaining;
            if cooldown.recharge.is_none() {
                entry.last_update = ammo.anchored_at;
            }
        }
        if entry.recharge_remaining(now) > 0 || entry.ammo_recharge_remaining(now) > 0 {
            active.push(entry);
        }
    }
    active
}

/// Returns the skillbar slot bound to the given skill id, if any.
fn slot_for_skill(bar: &Skillbar, id: u32) -> Option<Slot> {
    Slot::iter().find(|slot| {
        matches!(
            bar.slot(*slot).map(|ability| ability.id),
            Some(SkillId::Id(skill)) if skill == id
        )
    })
}

fn read_weapon_swap(game: &impl GameMemory, character: usize, combatant: usize) -> Option<Ability> {
    if value_at::<u32>(game, character, CHARACTER_WEAPON_SWAP_STATE)? == 2 {
        return None;
    }
    let last_update = value_at::<u32>(game, combatant, COMBATANT_WEAPON_SWAP_LAST_UPDATE)?;
    let cooldown = value_at::<u32>(game, combatant, COMBATANT_WEAPON_SWAP_COOLDOWN)?;
    let mut ability = Ability::new(
        SkillId::WeaponSwap,
        u32::from(cooldown == 0),
        last_update,
        1.0,
    );
    ability.recharge = cooldown;
    ability.recharge_remaining = cooldown;
    Some(ability)
}

fn build_runtime_selections(
    profession: Option<u8>,
    skillbar: &Skillbar,
    equipped_skills: &[u32],
    selection_skills: &[u32],
) -> (Vec<u32>, BitFlags<ProfSelection>) {
    // Prefer the build slot's equipped skill ids (which report the slotted skill
    // even while it is transformed); fall back to the runtime skillbar when the
    // build-slot resolution is unavailable.
    let mut skill_selections = if equipped_skills.is_empty() {
        [
            Slot::Heal,
            Slot::Utility1,
            Slot::Utility2,
            Slot::Utility3,
            Slot::Elite,
        ]
        .into_iter()
        .filter_map(|slot| match skillbar.slot(slot)?.id {
            SkillId::Id(id) if sane_skill_id(id) => Some(id),
            _ => None,
        })
        .collect::<Vec<_>>()
    } else {
        equipped_skills
            .iter()
            .copied()
            .filter(|id| sane_skill_id(*id))
            .collect::<Vec<_>>()
    };

    // The original appends the profession-selection skill ids (e.g. the Evoker
    // familiar id) after the equipped heal/utility/elite ids.
    skill_selections.extend(
        selection_skills
            .iter()
            .copied()
            .filter(|id| sane_skill_id(*id)),
    );

    let contains_any = |ids: &[u32]| ids.iter().any(|id| skill_selections.contains(id));
    let mut prof_selections = BitFlags::EMPTY;
    match profession {
        Some(9) => {
            if contains_any(&[28_134, 26_937, 29_209, 27_107, 28_231, 28_406]) {
                prof_selections.insert(ProfSelection::LegendAssassin);
            }
            if contains_any(&[28_195, 29_148, 27_025, 27_715, 26_821, 27_356]) {
                prof_selections.insert(ProfSelection::LegendCentaur);
            }
            if contains_any(&[28_494, 28_219, 27_505, 27_322, 27_917, 28_287]) {
                prof_selections.insert(ProfSelection::LegendDemon);
            }
            if contains_any(&[28_419, 26_557, 26_679, 28_516, 27_975]) {
                prof_selections.insert(ProfSelection::LegendDwarf);
            }
            if contains_any(&[28_085, 27_220, 26_644, 27_014, 28_379, 27_760]) {
                prof_selections.insert(ProfSelection::LegendDragon);
            }
            if contains_any(&[41_858, 46_409, 45_686, 42_949, 40_485, 41_220, 45_773]) {
                prof_selections.insert(ProfSelection::LegendRenegade);
            }
            if contains_any(&[62_749, 62_891, 62_719, 62_832, 62_962, 62_878, 62_942]) {
                prof_selections.insert(ProfSelection::LegendAlliance);
            }
            if contains_any(&[76_610, 77_043, 77_243, 77_291, 77_141, 76_968]) {
                prof_selections.insert(ProfSelection::LegendEntity);
            }
        }
        Some(6) if !selection_skills.is_empty() => {
            // Elementalist Evoker familiar: a single selection derived from the
            // profession-selection skill ids (not the equipped skillbar). The
            // original scans them in order and takes the first familiar id it
            // finds, defaulting to Fire when the list carries none.
            let familiar = selection_skills
                .iter()
                .find_map(|id| match *id {
                    EVOKER_FAMILIAR_WATER => Some(ProfSelection::FamiliarWater),
                    EVOKER_FAMILIAR_EARTH => Some(ProfSelection::FamiliarEarth),
                    EVOKER_FAMILIAR_AIR => Some(ProfSelection::FamiliarAir),
                    _ => None,
                })
                .unwrap_or(ProfSelection::FamiliarFire);
            prof_selections.insert(familiar);
        }
        _ => {}
    }

    (skill_selections, prof_selections)
}

/// Pet / mech skills that fill the player's profession slots.
///
/// A pet combatant indexes its skills differently from a player combatant, so
/// the raw slots are not the player's `12..16` profession slots. The original
/// reader's `sub_1800E2560` reads pet slot `6` into `Profession 1`, slot `11`
/// into `Profession 2` and slot `7` into `Profession 3` (the 40-byte ability
/// records at `a1[110]`, `a1[120]` and `a1[130]` of its output array), and never
/// publishes a fourth pet skill. The same function is used for the Mechanist
/// mech (`kennel + 0xF8`) and the Ranger's active pet.
///
/// This is what makes the Mechanist's F1-F3 work: the player's `Profession 1-3`
/// hold the "Locked" placeholder (`10586`, rejected by [`sane_skill_id`]) while
/// the mech is out, so the mech commands are taken from the mech's combatant.
/// The slots are only filled when the local combatant left them empty, exactly
/// like the original. `scripts/recovery/dump_skillbar_table.py` dumps the
/// original's `byte_1801B0AD8` player slot table for comparison.
const PET_SKILL_SLOTS: [(usize, Slot); 3] = [
    (6, Slot::Profession1),
    (11, Slot::Profession2),
    (7, Slot::Profession3),
];

fn slot_map() -> [(usize, Slot); 17] {
    [
        (5, Slot::Weapon1),
        (6, Slot::Weapon2),
        (7, Slot::Weapon3),
        (8, Slot::Weapon4),
        (9, Slot::Weapon5),
        (0, Slot::Heal),
        (1, Slot::Utility1),
        (2, Slot::Utility2),
        (3, Slot::Utility3),
        (4, Slot::Elite),
        (12, Slot::Profession1),
        (13, Slot::Profession2),
        (14, Slot::Profession3),
        (15, Slot::Profession4),
        (16, Slot::Profession5),
        (20, Slot::SpecialAction),
        (21, Slot::Mount),
    ]
}

fn read_skill_definition(
    game: &impl GameMemory,
    combatant: usize,
    raw: usize,
) -> Option<(u32, usize)> {
    let definition = pointer_at(
        game,
        combatant,
        COMBATANT_SKILLDEF_BASE.checked_add(raw.checked_mul(8)?)?,
    )?;
    let id = value_at::<u32>(game, definition, SKILLDEF_ID)?;
    sane_skill_id(id).then_some((id, definition))
}

fn build_ability(
    game: &impl GameMemory,
    combatant: usize,
    raw: usize,
    id: u32,
    definition: usize,
    now: u32,
    cooldowns: &BTreeMap<u32, Cooldown>,
) -> Ability {
    let mut ability = Ability::new(id, static_max_ammo(game, definition), now, 1.0);
    if value_at::<u32>(game, definition, SKILLDEF_FLAGS)
        .is_some_and(|flags| flags & SKILLDEF_GROUND_TARGETED != 0)
    {
        ability.info.insert(AbilityInfo::GroundTarget);
    }
    ability.info |= dynamic_ability_info(game, combatant, raw);
    let static_total = static_cooldown_total(game, definition);
    if let Some(cooldown) = cooldowns.get(&id) {
        if let Some(recharge) = cooldown.recharge {
            // The node carries the cooldown the game actually started, including
            // any cooldown reduction, so it must not be widened to the skill
            // definition's unmodified base: the Mechanist mech commands have a
            // 20 s / 25 s base but start at 16 s / 20 s with the reduction.
            ability.recharge = recharge.total;
            ability.recharge_remaining = recharge.remaining.min(ability.recharge);
            ability.recharge_rate = recharge.rate;
            // The remaining is the snapshot observed at the anchor, not at the
            // read time: the UI projects it forward with `recharge_rate`.
            ability.last_update = recharge.anchored_at;
        }
        if let Some(ammo) = cooldown.ammo {
            ability.ammo = ammo.ammo;
            ability.ammo_recharge = ammo.recharge;
            ability.ammo_recharge_remaining = ammo.recharge_remaining.min(ammo.recharge);
            // An ammo-only cooldown carries its anchor on the ammo snapshot.
            if cooldown.recharge.is_none() {
                ability.last_update = ammo.anchored_at;
            }
        }
    }
    // Only a skill without a live cooldown node falls back to the definition, so
    // a ready skill still reports its cooldown length.
    if ability.recharge == 0 {
        ability.recharge = static_total;
    }
    ability
}

/// Restores the slot-local state bits emitted by the original ability
/// decoder.  These values live on the combatant rather than the skill
/// definition, so pet-command abilities must use their pet combatant too.
fn dynamic_ability_info(
    game: &impl GameMemory,
    combatant: usize,
    raw: usize,
) -> BitFlags<AbilityInfo> {
    let mut info = BitFlags::EMPTY;
    let raw = u32::try_from(raw).ok();

    if raw.is_some_and(|raw| {
        value_at::<u32>(game, combatant, COMBATANT_AUTO_ATTACK_SLOT) == Some(raw)
    }) {
        info.insert(AbilityInfo::AutoAttack);
    }
    if raw.is_some_and(|raw| value_at::<u32>(game, combatant, COMBATANT_PRESSED_SLOT) == Some(raw))
    {
        info.insert(AbilityInfo::Pressed);
    }
    if raw
        .filter(|raw| *raw < u32::BITS)
        .zip(value_at::<u32>(game, combatant, COMBATANT_PENDING_MASK))
        .is_some_and(|(raw, pending)| pending & (1 << raw) != 0)
    {
        info.insert(AbilityInfo::Pending);
    }

    info
}

/// Upper bound for a skill's ammo/charge count. Real skills top out at a
/// handful of charges; anything larger (or zero) is treated as a single-charge
/// skill. Guards against a stray/garbage read at [`SKILLDEF_MAX_AMMO`].
const SANE_MAX_AMMO: u32 = 25;

fn sane_ammo(value: u32) -> Option<u32> {
    (1..=SANE_MAX_AMMO).contains(&value).then_some(value)
}

fn static_max_ammo(game: &impl GameMemory, definition: usize) -> u32 {
    pointer_at(game, definition, SKILLDEF_COOLDOWN)
        .and_then(|cooldown| value_at::<u32>(game, cooldown, SKILLDEF_MAX_AMMO))
        .and_then(sane_ammo)
        .unwrap_or(1)
}

fn static_cooldown_total(game: &impl GameMemory, definition: usize) -> u32 {
    pointer_at(game, definition, SKILLDEF_COOLDOWN)
        .and_then(|cooldown| value_at::<u32>(game, cooldown, SKILLDEF_COOLDOWN_TOTAL))
        .filter(|total| sane_cooldown_total(*total))
        .unwrap_or(0)
}

/// Diagnostic for profession slots whose source differs from the generic
/// player combatant (Mechanist mech commands, Ranger pet commands).
///
/// Logs the published profession slots together with every raw pet slot
/// whenever they change, so the pet skill layout can be confirmed from the log
/// without guessing which pet slot holds which command.
fn log_profession_slots(game: &impl GameMemory, bar: &Skillbar, pet_combatant: Option<usize>) {
    use std::sync::atomic::{AtomicU64, Ordering};

    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);

    let mut signature = 0u64;
    let mut text = String::new();
    for slot in [
        Slot::Profession1,
        Slot::Profession2,
        Slot::Profession3,
        Slot::Profession4,
        Slot::Profession5,
    ] {
        let id = bar
            .slot(slot)
            .and_then(|ability| match ability.id {
                SkillId::Id(id) => Some(id),
                _ => None,
            })
            .unwrap_or(0);
        signature = signature
            .wrapping_mul(0x1_0000_0001)
            .wrapping_add(id as u64);
        text.push_str(&format!(" {slot}={id}"));
    }
    if let Some(pet) = pet_combatant {
        for raw in 0..MAX_PET_SLOT {
            let id = read_skill_definition(game, pet, raw)
                .map(|(id, _)| id)
                .unwrap_or(0);
            signature = signature
                .wrapping_mul(0x1_0000_0001)
                .wrapping_add(id as u64);
            if id != 0 {
                text.push_str(&format!(" pet{raw}={id}"));
            }
        }
    }
    if LAST.swap(signature, Ordering::Relaxed) != signature {
        log::debug!("[reffect-runtime] profession slots:{text}");
    }
}

/// Highest pet combatant skill slot inspected by [`log_profession_slots`].
const MAX_PET_SLOT: usize = 24;

/// Walks the combatant cooldown container and merges the raw node snapshots per
/// skill. The snapshots are anchored by [`anchor_cooldowns`] once both the
/// character and the pet combatant have been collected.
fn collect_cooldowns(
    game: &impl GameMemory,
    combatant: usize,
    cooldowns: &mut BTreeMap<u32, Cooldown>,
    now: u32,
) {
    let Some(container) = pointer_at(game, combatant, COMBATANT_COOLDOWNS) else {
        return;
    };
    let rate = value_at::<f32>(game, container, COOLDOWN_RATE)
        .filter(|rate| rate.is_finite() && *rate > 0.01 && *rate < 20.0)
        .unwrap_or(1.0);
    let stride = value_at::<u32>(game, container, COOLDOWN_STRIDE)
        .and_then(|stride| usize::try_from(stride).ok())
        .filter(|stride| *stride <= 0x100)
        .unwrap_or(COOLDOWN_STRIDE);
    let mut seen = Vec::with_capacity(MAX_COOLDOWN_NODES);
    // Diagnostic: the raw node values, so a published total that disagrees with
    // the game can be traced to the node field, the skill definition base or the
    // container rate.
    let mut diagnostic = log::log_enabled!(log::Level::Debug).then(String::new);

    for root_offset in COOLDOWN_ROOTS {
        let mut node = raw_pointer_at(game, container, root_offset).unwrap_or(0) & !1;
        for _ in 0..MAX_COOLDOWN_NODES {
            if !valid_pointer(node) || node_in_range(node, container, 0x140) || seen.contains(&node)
            {
                break;
            }
            seen.push(node);

            if let Some(definition) = pointer_at(game, node, COOLDOWN_NODE_SKILLDEF)
                && let Some(id) = value_at::<u32>(game, definition, SKILLDEF_ID)
                && sane_skill_id(id)
            {
                let remaining = value_at::<u32>(game, node, COOLDOWN_NODE_REMAINING)
                    .filter(|remaining| *remaining <= 600_000)
                    .unwrap_or(0);
                let node_total = value_at::<u32>(game, node, COOLDOWN_NODE_TOTAL);
                let total = node_total
                    .filter(|total| sane_cooldown_total(*total))
                    .unwrap_or_else(|| static_cooldown_total(game, definition));
                let ammo_state = value_at::<u32>(game, node, COOLDOWN_NODE_AMMO_STATE).unwrap_or(0);
                if let Some(text) = diagnostic.as_mut() {
                    text.push_str(&format!(
                        "\n  id={id} remaining={remaining} node_total={node_total:?} \
                         static_total={} rate={rate} ammo_state={ammo_state}",
                        static_cooldown_total(game, definition)
                    ));
                }
                let entry = cooldowns.entry(id).or_default();
                if ammo_state != 0 {
                    let candidate = AmmoCooldown {
                        ammo: value_at::<u32>(game, node, COOLDOWN_NODE_AMMO)
                            .and_then(sane_ammo)
                            .unwrap_or(0),
                        recharge: total.max(remaining),
                        recharge_remaining: remaining,
                        anchored_at: now,
                    };
                    if !matches!(entry.ammo, Some(current) if current.recharge_remaining > candidate.recharge_remaining)
                    {
                        entry.ammo = Some(candidate);
                    }
                } else if remaining > 0 {
                    let candidate = Recharge {
                        remaining,
                        total: total.max(remaining),
                        rate,
                        anchored_at: now,
                    };
                    if !matches!(entry.recharge, Some(current) if current.remaining > candidate.remaining)
                    {
                        entry.recharge = Some(candidate);
                    }
                }
            }

            let primary_next =
                raw_pointer_at(game, node, stride.saturating_add(COOLDOWN_NODE_NEXT)).unwrap_or(0)
                    & !1;
            let fallback_next = raw_pointer_at(game, node, 0).unwrap_or(0) & !1;
            let secondary_next = raw_pointer_at(game, node, COOLDOWN_NODE_NEXT).unwrap_or(0) & !1;
            node = [primary_next, fallback_next, secondary_next]
                .into_iter()
                .find(|candidate| {
                    valid_pointer(*candidate)
                        && !node_in_range(*candidate, container, 0x140)
                        && !seen.contains(candidate)
                })
                .unwrap_or(0);
        }
    }

    if let Some(text) = diagnostic {
        log_cooldown_nodes(combatant, &text);
    }
}

/// Logs the raw cooldown nodes of a combatant whenever they change, keeping the
/// per-combatant dedup so the character and pet passes do not alternate.
fn log_cooldown_nodes(combatant: usize, text: &str) {
    use std::sync::{Mutex, OnceLock};

    static LAST: OnceLock<Mutex<HashMap<usize, u64>>> = OnceLock::new();

    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }

    let mut last = LAST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if last.insert(combatant, hash) != Some(hash) {
        log::debug!("[reffect-runtime] cooldown nodes {combatant:#x}:{text}");
    }
}

const EFFECT_TREE_CONTAINER: usize = 0xD0;
const EFFECT_TREE_NODE_OFFSET: usize = 0x38;
const EFFECT_TREE_ROOT: usize = 0x48;
const EFFECT_NODE_WRAPPER: usize = 0x10;
const EFFECT_NODE_DURATION: usize = 0x40;
const EFFECT_NODE_APPLIED_AT: usize = 0x44;
const EFFECT_WRAPPER_ID: usize = 0x28;
// A permanent effect carries a non-null "definition" pointer here only when it
// is a real applied buff (skill/trait-sourced). Equipment/sigil/relic and other
// internal permanent effects leave it null. The original decoder uses exactly
// this to decide which permanent effects appear as player buffs — live-verified
// 2026-09-23: of 66 permanent effects, only the three with a non-null pointer
// (ids 739/5586/77060) matched the plugin's own "Player buffs" list.
const EFFECT_WRAPPER_DEFINITION: usize = 0x50;
const EFFECT_WRAPPER_STATE: usize = 0x58;
const EFFECT_WRAPPER_VALUE: usize = 0x60;
const EFFECT_LAST_SQUAD_HIGHLIGHT: usize = 0xC0;
const EFFECT_LAST_SCREEN_BORDER: usize = 0xC8;
const EFFECT_ACTIVITY_TICK: usize = 0x28;
const MAX_EFFECT_NODES: usize = 512;

#[derive(Debug, Clone)]
struct CachedBuffSnapshot {
    captured_at: u32,
    buffs: Option<BuffMap>,
}

#[derive(Debug, Default)]
struct BuffSnapshotCache {
    entries: BTreeMap<usize, CachedBuffSnapshot>,
}

impl BuffSnapshotCache {
    fn read(&mut self, game: &impl GameMemory, character: usize, now: u32) -> Option<BuffMap> {
        if let Some(snapshot) = self.cached(&character, now) {
            return snapshot;
        }
        let buffs = read_buffs_at(game, character, now);
        self.store(character, now, buffs.clone());
        buffs
    }

    /// Reads the buffs of an already resolved effect-tree container. Targets of
    /// kinds that keep their effects outside the character layout (large
    /// map-break objectives) resolve the container themselves.
    fn read_container(
        &mut self,
        game: &impl GameMemory,
        container: usize,
        now: u32,
    ) -> Option<BuffMap> {
        if let Some(snapshot) = self.cached(&container, now) {
            return snapshot;
        }
        let buffs = read_buffs_from_container(game, container, now);
        self.store(container, now, buffs.clone());
        buffs
    }

    fn cached(&self, key: &usize, now: u32) -> Option<Option<BuffMap>> {
        let snapshot = self.entries.get(key)?;
        (now.wrapping_sub(snapshot.captured_at) < BUFF_SNAPSHOT_POLL)
            .then(|| snapshot.buffs.clone())
    }

    fn store(&mut self, key: usize, now: u32, buffs: Option<BuffMap>) {
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_BUFF_CACHE_ENTRIES {
            self.entries.clear();
        }
        self.entries.insert(
            key,
            CachedBuffSnapshot {
                captured_at: now,
                buffs,
            },
        );
    }
}

/// Reads the player-only metadata that accompanies the shared active-effect
/// tree. The original reader stores screen-border and squad-highlight ticks
/// beside the tree, so they must not be inferred from visible buff entries.
fn read_player_buff_info(
    game: &impl GameMemory,
    cache: &mut BuffSnapshotCache,
    character: usize,
    now: u32,
) -> Option<PlayerBuffInfo> {
    let buffs = cache.read(game, character, now)?;
    let container = pointer_at(game, character, EFFECT_TREE_CONTAINER)?;
    let last_screen_border = pointer_at(game, container, EFFECT_LAST_SCREEN_BORDER)
        .and_then(|entry| value_at::<u32>(game, entry, EFFECT_ACTIVITY_TICK))
        .unwrap_or(0);
    let last_squad_highlight = pointer_at(game, container, EFFECT_LAST_SQUAD_HIGHLIGHT)
        .and_then(|entry| value_at::<u32>(game, entry, EFFECT_ACTIVITY_TICK))
        .unwrap_or(0);
    Some(PlayerBuffInfo {
        buffs,
        last_screen_border,
        last_squad_highlight,
    })
}

#[cfg(test)]
fn read_buffs(game: &impl GameMemory, character: usize) -> Option<BuffMap> {
    read_buffs_at(game, character, tick_count())
}

fn read_buffs_at(game: &impl GameMemory, character: usize, now: u32) -> Option<BuffMap> {
    let container = pointer_at(game, character, EFFECT_TREE_CONTAINER)?;
    read_buffs_from_container(game, container, now)
}

/// Reads the active-effect list of an already resolved container.
fn read_buffs_from_container(
    game: &impl GameMemory,
    container: usize,
    now: u32,
) -> Option<BuffMap> {
    let node_offset =
        usize::try_from(value_at::<u32>(game, container, EFFECT_TREE_NODE_OFFSET)?).ok()?;
    let next_offset = node_offset.checked_add(size_of::<usize>())?;
    if node_offset > 0x100 || next_offset > 0x108 {
        return None;
    }

    let mut buffs = BuffMap::new();
    let mut seen = Vec::with_capacity(MAX_EFFECT_NODES);
    let mut node = raw_pointer_at(game, container, EFFECT_TREE_ROOT).unwrap_or(0);
    for _ in 0..MAX_EFFECT_NODES {
        if node & 1 != 0 || !valid_pointer(node) || seen.contains(&node) {
            break;
        }
        seen.push(node);
        let next = raw_pointer_at(game, node, next_offset).unwrap_or(0);

        let Some(wrapper) = pointer_at(game, node, EFFECT_NODE_WRAPPER) else {
            node = next;
            continue;
        };
        if value_at::<u32>(game, wrapper, EFFECT_WRAPPER_STATE) != Some(1)
            || pointer_at(game, wrapper, EFFECT_WRAPPER_VALUE).is_none()
        {
            node = next;
            continue;
        }
        let Some(id) =
            value_at::<u32>(game, wrapper, EFFECT_WRAPPER_ID).filter(|id| sane_skill_id(*id))
        else {
            node = next;
            continue;
        };
        let duration = value_at::<u32>(game, node, EFFECT_NODE_DURATION).unwrap_or(u32::MAX);
        let applied_at = value_at::<u32>(game, node, EFFECT_NODE_APPLIED_AT).unwrap_or(0);

        if duration != u32::MAX && duration <= 600_000 && applied_at != 0 {
            // A reasonable finite duration is a timed buff shown with progress.
            insert_runtime_buff(
                &mut buffs,
                id,
                1,
                applied_at,
                applied_at.wrapping_add(duration),
                now,
            );
        } else {
            // Permanent (u32::MAX) or very-long-duration effects (e.g. a
            // 6-hour map/food effect) are surfaced as a real player buff only
            // when the wrapper carries a definition pointer — the original's
            // own test that filters out equipment/internal effects. They are
            // published without a progress bar, matching the original's plain
            // effect entries.
            let has_definition = pointer_at(game, wrapper, EFFECT_WRAPPER_DEFINITION).is_some();
            if has_definition || is_standard_combat_buff(id) || crate::catalog::cached_is_buff(id) {
                insert_runtime_buff(&mut buffs, id, 1, 0, u32::MAX, now);
            }
        }

        node = next;
    }
    Some(buffs)
}

fn insert_runtime_buff(
    buffs: &mut BuffMap,
    id: u32,
    stacks: u32,
    applied_at: u32,
    runout_at: u32,
    now: u32,
) {
    if id == 0 || stacks == 0 {
        return;
    }
    let incoming = if runout_at == u32::MAX {
        Buff {
            stacks,
            ..Buff::hidden_duration()
        }
    } else {
        let remaining = runout_at.wrapping_sub(now) as i32;
        if remaining <= 0 {
            return;
        }
        Buff {
            stacks,
            apply_time: applied_at,
            runout_time: runout_at,
        }
    };

    match buffs.get_mut(&id) {
        Some(existing) if is_intensity_buff(id) => {
            existing.stacks = existing.stacks.saturating_add(incoming.stacks);
            if !incoming.is_infinite()
                && (existing.is_infinite() || incoming.runout_time > existing.runout_time)
            {
                *existing = incoming;
            }
        }
        Some(existing)
            if !incoming.is_infinite()
                && (existing.is_infinite() || incoming.runout_time > existing.runout_time) =>
        {
            *existing = incoming;
        }
        Some(_) => {}
        None => {
            buffs.insert(id, incoming);
        }
    }
}

#[cfg(test)]
fn insert_buff(buffs: &mut BuffMap, id: u32, duration: u32, applied_at: u32) {
    let timed = duration > 0 && duration <= 600_000;
    let incoming = if timed {
        Buff {
            stacks: 1,
            apply_time: applied_at,
            runout_time: applied_at.wrapping_add(duration),
        }
    } else {
        Buff::hidden_duration()
    };
    match buffs.get_mut(&id) {
        Some(existing) if is_intensity_buff(id) => {
            existing.stacks = existing.stacks.saturating_add(1);
            if timed && (existing.is_infinite() || incoming.runout_time > existing.runout_time) {
                existing.apply_time = incoming.apply_time;
                existing.runout_time = incoming.runout_time;
            }
        }
        Some(existing)
            if timed && (existing.is_infinite() || incoming.runout_time > existing.runout_time) =>
        {
            *existing = incoming;
        }
        Some(_) => {}
        None => {
            buffs.insert(id, incoming);
        }
    }
}

/// Player buffs are filtered at read time: [`read_buffs_at`] only publishes a
/// permanent effect when its wrapper carries a definition pointer
/// ([`EFFECT_WRAPPER_DEFINITION`]) — the original decoder's own test for a real
/// applied buff versus an equipment/internal effect. Timed buffs are always
/// real, so no separate catalogue-based pass is needed.
fn filter_group_buffs(buffs: &mut BuffMap) {
    buffs.retain(|id, _| crate::catalog::cached_is_group_buff(*id));
}

/// Returns the active build together with the address of the template slot it
/// was read from, so selection readers can walk the slot's per-skill tables.
fn read_build(game: &impl GameMemory, character: usize) -> Option<(Build, usize)> {
    let primary = pointer_at(game, character, CHARACTER_BUILD_HOLDER).and_then(|holder| {
        read_build_from_view(game, holder, BUILD_HOLDER_VIEW, BUILD_VIEW_SLOT_BIAS)
    });
    primary.or_else(|| {
        pointer_at(game, character, CHARACTER_CORE_STATS)
            .and_then(|core| pointer_at(game, core, CORE_BUILD_HOLDER))
            .and_then(|holder| {
                read_build_from_view(
                    game,
                    holder,
                    CORE_BUILD_HOLDER_VIEW,
                    CORE_BUILD_VIEW_SLOT_BIAS,
                )
            })
    })
}

fn read_build_from_view(
    game: &impl GameMemory,
    holder: usize,
    view_offset: usize,
    slot_bias: usize,
) -> Option<(Build, usize)> {
    let view = pointer_at(game, holder, view_offset)?;
    let seed = view.checked_sub(slot_bias)?;
    let seed_build = read_build_slot(game, seed)?;

    // A view points into one template slot.  Walk back over the compact
    // 0x150-strided bank, then use the bank's authoritative active index.
    for back in 0..=8usize {
        let candidate = seed.checked_sub(back.checked_mul(BUILD_SLOT_STRIDE)?)?;
        if read_build_slot(game, candidate).is_none() {
            break;
        }
        let Some(index) = value_at::<u32>(game, candidate, BUILD_ACTIVE_INDEX) else {
            continue;
        };
        if index > 8 {
            continue;
        }
        let active = candidate.checked_add(
            usize::try_from(index)
                .ok()?
                .checked_mul(BUILD_SLOT_STRIDE)?,
        )?;
        if let Some(build) = read_build_slot(game, active) {
            return Some((build, active));
        }
    }
    Some((seed_build, seed))
}

fn read_build_slot(game: &impl GameMemory, slot: usize) -> Option<Build> {
    let mut specs = [0; 3];
    for (index, offset) in BUILD_SPEC_POINTERS.into_iter().enumerate() {
        specs[index] = pointer_at(game, slot, offset)
            .and_then(|definition| value_at::<u32>(game, definition, SKILLDEF_ID))?;
    }
    if !specs.iter().all(|spec| (1..=81).contains(spec)) {
        return None;
    }
    let mut traits = [0; 9];
    for (index, trait_id) in traits.iter_mut().enumerate() {
        let offset = BUILD_TRAIT_POINTER_BASE.checked_add(index.checked_mul(8)?)?;
        *trait_id = pointer_at(game, slot, offset)
            .and_then(|definition| value_at::<u32>(game, definition, SKILLDEF_ID))
            .unwrap_or(0);
        if *trait_id != 0 && !(*trait_id >= 100 && *trait_id < 100_000) {
            return None;
        }
    }
    Some(Build {
        specs,
        traits,
        // Populated in `decode_snapshot` from the build slot's per-skill tables.
        skill_selections: Vec::new(),
        prof_selections: BitFlags::EMPTY,
        pets: read_ranger_pets(game, slot),
    })
}

/// Reads the flat profession-selection skill id array (`slot+0x1A8`, count at
/// `slot+0x1B4`). Only the Evoker populates this today, so it is gated on the
/// elite spec to avoid publishing unrelated ids for other builds.
fn read_selection_skills(game: &impl GameMemory, slot: usize, specs: &[u32; 3]) -> Vec<u32> {
    if !specs.contains(&ELEMENTALIST_EVOKER_SPEC) {
        return Vec::new();
    }
    let Some(array) = pointer_at(game, slot, BUILD_SELECTION_ARRAY) else {
        return Vec::new();
    };
    let count = value_at::<u32>(game, slot, BUILD_SELECTION_COUNT)
        .map(|count| count as usize)
        .filter(|count| *count <= MAX_SELECTION_SKILLS)
        .unwrap_or(0);
    (0..count)
        .filter_map(|index| {
            value_at::<u32>(game, array, index.checked_mul(4)?).filter(|id| sane_skill_id(*id))
        })
        .collect()
}

/// Reads the equipped heal/utility/elite skill ids from the build slot's
/// per-slot skill tables (`slot+0xD0..0xF0`, one container pointer each for
/// Heal/Utility1-3/Elite). Each container holds a `0x70`-strided variant table
/// starting at `container+0x60`; the equipped skill is the first variant whose
/// descriptor is not a transient/transformed entry (`+0x08 == 0 && +0x30 == 0`)
/// and whose skilldef carries the slotted-skill flag `0x0400_0000`. This
/// mirrors the original resolver `sub_1800E9FA0` and, unlike the runtime
/// skillbar, reports the equipped skill rather than an active transform (e.g.
/// Evoker utility `77190` instead of the summoned-familiar variant `76563`).
/// Gated on the Evoker elite spec, which is the only build known to transform
/// these slots; every other build keeps the runtime-skillbar list.
fn read_selected_skills(game: &impl GameMemory, slot: usize, specs: &[u32; 3]) -> Vec<u32> {
    if !specs.contains(&ELEMENTALIST_EVOKER_SPEC) {
        return Vec::new();
    }
    (0..SELECTED_SLOT_COUNT)
        .filter_map(|index| {
            let container =
                pointer_at(game, slot, BUILD_SELECTED_SLOT_BASE.checked_add(index * 8)?)?;
            resolve_selected_skill(game, container)
        })
        .collect()
}

fn resolve_selected_skill(game: &impl GameMemory, container: usize) -> Option<u32> {
    for index in 0..MAX_SKILL_VARIANTS {
        let Some(variant) =
            SKILL_VARIANT_BASE.checked_add(index.checked_mul(SKILL_VARIANT_STRIDE)?)
        else {
            break;
        };
        let Some(definition) = pointer_at(game, container, variant) else {
            continue;
        };
        // Skip transient/transformed variants; the equipped one has both link
        // words zeroed.
        if raw_pointer_at(game, container, variant + 0x08) != Some(0)
            || raw_pointer_at(game, container, variant + 0x30) != Some(0)
        {
            continue;
        }
        if value_at::<u32>(game, definition, SKILLDEF_FLAGS).unwrap_or(0) & SKILLDEF_EQUIPPED_FLAG
            == 0
        {
            continue;
        }
        if let Some(id) =
            value_at::<u32>(game, definition, SKILLDEF_ID).filter(|id| sane_skill_id(*id))
        {
            return Some(id);
        }
    }
    None
}

fn read_ranger_pets(game: &impl GameMemory, slot: usize) -> [u32; 2] {
    if value_at::<u32>(game, slot, BUILD_SLOT_PROFESSION) != Some(4) {
        return [0; 2];
    }
    let Some(selection) = pointer_at(game, slot, BUILD_SLOT_PROFESSION_SELECTION) else {
        return [0; 2];
    };
    if value_at::<u32>(game, selection, BUILD_PROFESSION_SELECTION_KIND) != Some(4) {
        return [0; 2];
    }

    [BUILD_RANGER_FIRST_PET, BUILD_RANGER_SECOND_PET].map(|offset| {
        pointer_at(game, selection, offset)
            .and_then(|pet| value_at::<u32>(game, pet, SKILLDEF_ID))
            .unwrap_or(0)
    })
}

fn read_gear(game: &impl GameMemory, character: usize) -> Option<GearSnapshot> {
    let skillbar = pointer_at(game, character, CHARACTER_SKILLBAR)?;
    let mut weapons = BitFlags::EMPTY;
    let mut found = false;
    for raw in GEAR_WEAPON_SLOTS {
        let entry_offset = SKILLBAR_SLOT_BASE.checked_add(raw.checked_mul(8)?)?;
        let Some(entry) = pointer_at(game, skillbar, entry_offset) else {
            continue;
        };
        let Some(meta) = pointer_at(game, entry, 0x40) else {
            continue;
        };
        if value_at::<u32>(game, meta, GEAR_META_KIND) != Some(GEAR_WEAPON_KIND) {
            continue;
        }
        let Some(type_object) = pointer_at(game, meta, GEAR_META_TYPE) else {
            continue;
        };
        let Some(internal_type) = value_at::<u32>(game, type_object, GEAR_TYPE_INTERNAL) else {
            continue;
        };
        let offhand = raw == 30 || raw == 32;
        if let Some(weapon) = map_weapon(internal_type, offhand) {
            weapons |= BitFlags::from_flag(weapon);
            found = true;
        }
    }
    let (sigils, relic) = read_equipped_effects(game, skillbar);
    found.then_some(GearSnapshot {
        weapons,
        sigils,
        relic,
    })
}

/// Reads the live effect ids attached to the four equipped weapon entries and
/// the dedicated relic entry. These are already the buff ids exposed by the
/// public `Gear` context; resolving them through the item catalogue loses the
/// values until an unrelated HTTP metadata request happens to populate it.
fn read_equipped_effects(game: &impl GameMemory, skillbar: usize) -> ([u32; 4], u32) {
    let mut sigils = [0; 4];
    let mut next_sigil = 0;

    for raw in GEAR_WEAPON_SLOTS {
        let entry_offset = match SKILLBAR_SLOT_BASE.checked_add(raw.saturating_mul(8)) {
            Some(offset) => offset,
            None => continue,
        };
        let Some(entry) = pointer_at(game, skillbar, entry_offset) else {
            continue;
        };
        let Some(effects) = entry.checked_add(GEAR_ENTRY_EFFECTS) else {
            continue;
        };
        for slot in GEAR_EFFECT_SLOTS {
            if next_sigil == sigils.len() {
                break;
            }
            let Some(foreign) = pointer_at(game, effects, slot) else {
                continue;
            };
            let Some(effect_id) = read_runtime_effect_id(game, foreign) else {
                continue;
            };
            sigils[next_sigil] = effect_id;
            next_sigil += 1;
        }
    }

    let relic = pointer_at(game, skillbar, GEAR_RELIC_ENTRY)
        .and_then(|entry| pointer_at(game, entry, GEAR_RELIC_EFFECT))
        .and_then(|foreign| read_runtime_effect_id(game, foreign))
        .unwrap_or(0);

    (sigils, relic)
}

fn read_runtime_effect_id(game: &impl GameMemory, foreign: usize) -> Option<u32> {
    let payload = pointer_at(game, foreign, GEAR_FOREIGN_PAYLOAD)?;
    let effect = match value_at::<u32>(game, foreign, GEAR_FOREIGN_KIND)? {
        GEAR_FOREIGN_DIRECT => pointer_at(game, payload, 0)?,
        GEAR_FOREIGN_WRAPPED => {
            let owner = pointer_at(game, payload, GEAR_WRAPPED_EFFECT_OWNER)?;
            pointer_at(game, owner, GEAR_WRAPPED_EFFECT)?
        }
        _ => return None,
    };
    value_at::<u32>(game, effect, GEAR_EFFECT_ID).filter(|id| sane_skill_id(*id))
}

fn map_weapon(internal: u32, offhand: bool) -> Option<Weapon> {
    Some(match internal {
        0 => {
            if offhand {
                Weapon::SwordOff
            } else {
                Weapon::SwordMain
            }
        }
        1 => Weapon::Hammer,
        2 => Weapon::Longbow,
        3 => Weapon::Shortbow,
        4 => {
            if offhand {
                Weapon::AxeOff
            } else {
                Weapon::AxeMain
            }
        }
        5 => {
            if offhand {
                Weapon::DaggerOff
            } else {
                Weapon::DaggerMain
            }
        }
        6 => Weapon::Greatsword,
        7 => {
            if offhand {
                Weapon::MaceOff
            } else {
                Weapon::MaceMain
            }
        }
        8 => {
            if offhand {
                Weapon::PistolOff
            } else {
                Weapon::PistolMain
            }
        }
        10 => Weapon::Rifle,
        11 => Weapon::Scepter,
        12 => Weapon::Staff,
        13 => Weapon::Focus,
        14 => Weapon::Torch,
        15 => Weapon::Warhorn,
        16 => Weapon::Shield,
        19 => Weapon::Spear,
        _ => return None,
    })
}

/// Resolves the target bridge pointer. The chained pointer (the local
/// combatant's +0x80) is preferred, but the game module's bridge static is used
/// when the chain does not expose a target descriptor -- which is effectively
/// what the original reader does by caching that static in its own context.
fn resolve_target_bridge(game: &impl GameMemory, local: usize) -> Option<usize> {
    let chained = pointer_at(game, local, CHARACTER_COMBATANT)
        .and_then(|combatant| pointer_at(game, combatant, COMBATANT_TARGET_BRIDGE));
    let cached = main_module_range()
        .map(|(base, _size)| base.wrapping_add(TARGET_BRIDGE_RVA))
        .filter(|bridge| valid_pointer(*bridge));
    for bridge in [chained, cached].into_iter().flatten() {
        if pointer_at(game, bridge, TARGET_DESCRIPTOR).is_some()
            || pointer_at(game, bridge, TARGET_DESCRIPTOR_FALLBACK).is_some()
        {
            return Some(bridge);
        }
    }
    chained.or(cached)
}

/// TEMP DEBUG (2026-10-07): 1 Hz probe of the target resolution so a client that
/// fails to recognise a selected target can be diagnosed from the log.
fn log_target_probe(game: &impl GameMemory, local: usize) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static LAST: AtomicU32 = AtomicU32::new(0);
    let now = unsafe { GetTickCount() };
    let last = LAST.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < 1_000 {
        return;
    }
    if LAST
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let combatant = pointer_at(game, local, CHARACTER_COMBATANT);
    let chained =
        combatant.and_then(|combatant| pointer_at(game, combatant, COMBATANT_TARGET_BRIDGE));
    let cached = main_module_range().map(|(base, _size)| base.wrapping_add(TARGET_BRIDGE_RVA));
    let bridge = resolve_target_bridge(game, local);
    let descriptor = bridge.and_then(|bridge| {
        pointer_at(game, bridge, TARGET_DESCRIPTOR)
            .or_else(|| pointer_at(game, bridge, TARGET_DESCRIPTOR_FALLBACK))
    });
    let kind = descriptor.and_then(|descriptor| value_at::<u32>(game, descriptor, TARGET_KIND));
    let object = descriptor.and_then(|descriptor| pointer_at(game, descriptor, TARGET_OBJECT));
    let character = object.and_then(|object| object.checked_sub(SELECTED_CHARACTER_BIAS));
    let health = character.and_then(|character| pointer_at(game, character, CHARACTER_HEALTH));
    let container =
        character.and_then(|character| pointer_at(game, character, EFFECT_TREE_CONTAINER));
    log::warn!(
        "[reffect-runtime] target probe: local={local:#x} combatant={combatant:?} chained={chained:?} cached={cached:?} bridge={bridge:?} descriptor={descriptor:?} kind={kind:?} object={object:?} character={character:?} health={health:?} container={container:?}"
    );
}

fn read_target(
    game: &impl GameMemory,
    _collection: usize,
    local: usize,
    buff_cache: &mut BuffSnapshotCache,
    now: u32,
) -> Option<TargetSnapshot> {
    log_target_probe(game, local);
    let bridge = resolve_target_bridge(game, local)?;
    // The original reader takes the selected target from the bridge's target
    // descriptor slot (with a fallback slot) rather than from the agent hard
    // lock. The descriptor carries a kind tag and the target object.
    let descriptor = pointer_at(game, bridge, TARGET_DESCRIPTOR)
        .or_else(|| pointer_at(game, bridge, TARGET_DESCRIPTOR_FALLBACK))?;
    let kind = value_at::<u32>(game, descriptor, TARGET_KIND)?;
    let object = pointer_at(game, descriptor, TARGET_OBJECT)?;
    // Kind 0 targets use the character layout (health at character+0x3E8, which
    // is object+0x3A0); kinds 10 and 11 keep health, effects and defiance on a
    // carrier object instead. This mirrors the original reader's dispatch and is
    // what makes large map-break objectives resolve at all.
    let character = object.checked_sub(SELECTED_CHARACTER_BIAS);
    if character == Some(local) {
        log_target_rejection(
            pointer_at(game, local, CHARACTER_COMBATANT).unwrap_or(0),
            bridge,
            descriptor,
            Some(object),
            None,
            None,
            local,
        );
        return None;
    }
    log_target_classification_probe(game, kind, descriptor, object, character);
    let (affinity, category) = read_target_classification(game, local, kind, character);
    let (health, barrier_layout, container, defiance) = match kind {
        0 => (
            character.and_then(|character| pointer_at(game, character, CHARACTER_HEALTH)),
            BARRIER_LAYOUT_CHARACTER,
            character.and_then(|character| pointer_at(game, character, EFFECT_TREE_CONTAINER)),
            character.and_then(|character| pointer_at(game, character, CHARACTER_DEFIANCE)),
        ),
        10 => (
            pointer_at(game, object, TARGET_HEALTH_KIND_10),
            BARRIER_LAYOUT_CARRIER,
            None,
            None,
        ),
        11 => match pointer_at(game, object, TARGET_CARRIER_KIND_11) {
            Some(carrier) => (
                pointer_at(game, carrier, TARGET_HEALTH_KIND_11),
                BARRIER_LAYOUT_CARRIER,
                pointer_at(game, carrier, TARGET_EFFECTS_KIND_11),
                pointer_at(game, carrier, TARGET_DEFIANCE_KIND_11),
            ),
            None => (None, BARRIER_LAYOUT_CARRIER, None, None),
        },
        _ => (None, BARRIER_LAYOUT_CHARACTER, None, None),
    };
    let defiance = read_defiance_at(game, defiance);
    let resources = match health.and_then(|health| {
        read_resources_from_health(game, health, barrier_layout, true, now, defiance.clone())
    }) {
        Some(resources) => Ok(resources),
        None => {
            // Health unreadable: still publish the target with a full normalized
            // bar and its real defiance, matching the original's placeholder.
            let defiance = Some(defiance.unwrap_or(Defiance::Active(100.0)));
            Ok(CombatantResources::new(
                true,
                ResourceState::HealthAlive,
                1.0,
                0.0,
                1.0,
                defiance,
            ))
        }
    };
    let buffs = container
        .and_then(|container| buff_cache.read_container(game, container, now))
        .ok_or(Error::Buffs);
    Some(TargetSnapshot {
        affinity,
        category,
        resources,
        buffs,
    })
}

/// Classifies the selected target from the fields the recovered read graph
/// exposes today.
///
/// Only structurally certain results are published: a character that is also a
/// current group member is an allied player, which needs no new client offset.
///
/// Everything else stays unclassified. The client fields carrying the agent
/// type, disposition, NPC rank and gathering discipline are not part of the
/// recovered read graph yet, and guessing them would make classification
/// triggers fire on the wrong targets. An unclassified target publishes
/// `Error::TargetState`/`Error::Affinity`, which keeps classification triggers
/// inactive and shows the gap in the debug view instead of inventing a value.
///
/// The target descriptor `kind` tag is deliberately **not** mapped to a
/// category. An earlier revision published `Gadget` for kinds 10 and 11 on the
/// assumption that they are the large map-break carrier objects, but the first
/// probe capture (2026-10-08, 13 selected targets) showed 10 of them were kind
/// 10 or 11 while only 3 were kind 0, and some kind 10 targets exposed a valid
/// character, agent and core-stats chain. Until the tag semantics are recovered,
/// mapping it would mislabel ordinary targets.
///
/// See [`log_target_classification_probe`] for how those offsets are recovered.
fn read_target_classification(
    game: &impl GameMemory,
    local: usize,
    kind: u32,
    character: Option<usize>,
) -> (
    Result<Affinity, Error>,
    Result<BitFlags<TargetCategory>, Error>,
) {
    let unclassified = (Err(Error::Affinity), Err(Error::TargetState));

    if kind != 0 {
        return unclassified;
    }
    let Some(character) = character else {
        return unclassified;
    };
    // The group member table stores the member character pointers, so a direct
    // match identifies the target as an allied player without needing any new
    // client offset.
    let is_group_member = read_group_characters(game, local)
        .unwrap_or_default()
        .iter()
        .any(|(_, member)| *member == character);
    if is_group_member {
        (
            Ok(Affinity::Friendly),
            Ok(make_bitflags!(TargetCategory::{Player | Friendly})),
        )
    } else {
        unclassified
    }
}

/// TEMP DEBUG (target classification): dumps the candidate classification fields
/// of a newly selected target so the offsets for agent type, disposition, NPC
/// rank and gathering discipline can be recovered.
///
/// Exactly one dump is emitted per selection and it carries a `seq` counter.
/// The in-game debug window shows the sequence of the current selection and
/// offers a tag button per category, which appends a `label=<name> seq=<n>`
/// line to the log. The capture therefore labels itself and no longer depends on
/// remembering the selection order. See
/// `docs/superpowers/plans/2026-10-08-target-classification-recovery.md`.
///
/// Remove this probe once the offsets are recovered and validated.
fn log_target_classification_probe(
    game: &impl GameMemory,
    kind: u32,
    descriptor: usize,
    object: usize,
    character: Option<usize>,
) {
    // Dump once per selection. `swap` also records the object for the next poll,
    // so re-selecting the same target after it was cleared dumps again.
    if TARGET_PROBE_OBJECT.swap(object, Ordering::Relaxed) == object {
        return;
    }

    let sequence = TARGET_PROBE_NEXT.fetch_add(1, Ordering::Relaxed);
    if sequence > PROBE_LIMIT {
        TARGET_PROBE_SEQUENCE.store(0, Ordering::Relaxed);
        if sequence == PROBE_LIMIT + 1 {
            log::warn!(
                "[reffect-target-classify] probe limit of {PROBE_LIMIT} dumps reached, stopping"
            );
        }
        return;
    }
    TARGET_PROBE_SEQUENCE.store(sequence, Ordering::Relaxed);

    let agent = character.and_then(|character| pointer_at(game, character, CHARACTER_AGENT));
    let core = character.and_then(|character| pointer_at(game, character, CHARACTER_CORE_STATS));
    log::warn!(
        "[reffect-target-classify] seq={sequence} kind={kind} descriptor={descriptor:#x} object={object:#x} character={} agent={} core={}",
        pointer_field(character),
        pointer_field(agent),
        pointer_field(core),
    );

    probe_region(game, "descriptor", descriptor, PROBE_DESCRIPTOR);
    probe_region(game, "object", object, PROBE_OBJECT);
    probe_strings(game, "descriptor", descriptor, PROBE_DESCRIPTOR);
    probe_strings(game, "object", object, PROBE_OBJECT);

    // `character` is `object - 0x48` regardless of kind, so it is only a real
    // character for kind 0. Dumping it for the other kinds would only add noise
    // from whatever happens to live at that offset in the heap.
    if kind == 0 {
        if let Some(character) = character {
            probe_region(game, "character", character, PROBE_CHARACTER);
            probe_strings(game, "character", character, PROBE_CHARACTER);
        }
        if let Some(core) = core {
            probe_region(game, "core", core, PROBE_CORE_STATS);
            probe_strings(game, "core", core, PROBE_CORE_STATS);
        }
        // For kind 0 the target descriptor *is* the character's own agent entry
        // (`character + 0x98`), verified on 2026-10-08, so dumping it again would
        // only duplicate the descriptor region.
        if let Some(agent) = agent
            && agent != descriptor
        {
            probe_region(game, "agent", agent, PROBE_AGENT);
            probe_strings(game, "agent", agent, PROBE_AGENT);
        }
    }
}

/// Logs every pointer in `[start, end)` at `base` that points at a printable
/// string, so the target's name can be located.
///
/// The name is what the game shows in the target frame, and the NPC rank is
/// conveyed as its "Veteran"/"Champion"/... prefix, so a readable name would
/// classify rank, gathering discipline and chests without needing the client's
/// internal rank field at all.
fn probe_strings(game: &impl GameMemory, name: &str, base: usize, range: (usize, usize)) {
    let (start, end) = range;
    let mut offset = start;
    let mut found = 0;

    while offset + size_of::<usize>() <= end {
        let pointer = value_at::<usize>(game, base, offset);
        if let Some(pointer) = pointer
            && valid_pointer(pointer)
            && let Some(text) = read_probe_string(game, pointer)
        {
            log::warn!(
                "[reffect-target-classify] string {name}+{offset:#05x} -> {pointer:#x} = {text}"
            );
            found += 1;
            if found >= PROBE_STRING_LIMIT {
                return;
            }
        }
        offset += size_of::<usize>();
    }
}

/// Reads a NUL terminated printable ASCII string at `address`.
///
/// UTF-16 is deliberately not accepted here: random heap bytes decode to valid
/// CJK code units, which buried the real matches in noise. Exact matching
/// against a known name is done by [`probe_local_name`] instead.
fn read_probe_string(game: &impl GameMemory, address: usize) -> Option<String> {
    let text = read_text(game, address, false)?;
    (text.chars().count() >= 4 && text.chars().any(|c| c.is_alphabetic())).then_some(text)
}

/// Reads a NUL terminated string at `address`, either single byte or UTF-16,
/// without any plausibility filter. For exact comparisons only.
fn read_text(game: &impl GameMemory, address: usize, wide: bool) -> Option<String> {
    const MAX: usize = 128;

    let mut text = String::new();
    if wide {
        for index in 0..MAX / 2 {
            match value_at::<u16>(game, address, index * 2) {
                Some(0) => return (!text.is_empty()).then_some(text),
                Some(unit) => text.push(char::from_u32(u32::from(unit))?),
                None => return None,
            }
        }
    } else {
        for index in 0..MAX {
            match value_at::<u8>(game, address, index) {
                Some(0) => return (!text.is_empty()).then_some(text),
                Some(byte) if (0x20..0x7f).contains(&byte) => text.push(byte as char),
                _ => return None,
            }
        }
    }

    None
}

/// Locates the character name field by searching the **local** character for the
/// name the Mumble link reports for the player.
///
/// The player's own name is already known, so a hit is unambiguous - unlike
/// scanning the target for "anything that looks like a string", which random
/// heap bytes satisfy constantly. The discovered offset can then be read for the
/// target, and the name gives the NPC rank by its "Veteran"/"Champion"/...
/// prefix, which is exactly what the game shows the player.
///
/// Runs once per session. Remove together with the rest of the probe.
fn probe_local_name(game: &impl GameMemory, local: usize) {
    use std::sync::atomic::AtomicBool;
    static DONE: AtomicBool = AtomicBool::new(false);
    static WARNED: AtomicBool = AtomicBool::new(false);

    if DONE.load(Ordering::Relaxed) {
        return;
    }

    // Read the name from the Mumble link rather than the Nexus RTAPI: the RTAPI
    // is a separate addon that is not always installed, and without it
    // `RealTimeApi::get()` returns `None` and the calibration silently never
    // ran (observed on 2026-10-08). The Mumble link is created by Nexus itself.
    let name = nexus::data_link::get_mumble_link()
        .and_then(|mumble| mumble.parse_identity().ok())
        .map(|identity| identity.name)
        .filter(|name| !name.is_empty());
    let Some(name) = name else {
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "[reffect-target-classify] local name unavailable, waiting for the Mumble identity"
            );
        }
        return;
    };
    DONE.store(true, Ordering::Relaxed);

    let agent = pointer_at(game, local, CHARACTER_AGENT);
    let core = pointer_at(game, local, CHARACTER_CORE_STATS);
    log::warn!(
        "[reffect-target-classify] local character={local:#x} agent={} core={} name={name:?}",
        pointer_field(agent),
        pointer_field(core),
    );

    probe_name_match(game, "local", local, PROBE_LOCAL_CHARACTER, &name);
    if let Some(agent) = agent {
        probe_name_match(game, "local-agent", agent, PROBE_LOCAL_AGENT, &name);
    }
    if let Some(core) = core {
        probe_name_match(game, "local-core", core, PROBE_LOCAL_CORE, &name);
    }
}

/// Searches `[start, end)` at `base` for `name` and logs every hit.
///
/// Three shapes are covered, because the client is free to keep the name in a
/// table rather than on the character:
///
/// 1. inline in the region itself
/// 2. at the target of a pointer in the region
/// 3. inside the struct a pointer in the region points to
///
/// Each hit logs where it was found so the offset can be turned into a read.
fn probe_name_match(
    game: &impl GameMemory,
    region: &str,
    base: usize,
    range: (usize, usize),
    name: &str,
) {
    let (start, end) = range;
    let needles = name_needles(name);
    let data = read_region_bytes(game, base, start, end);
    log::warn!(
        "[reffect-target-classify] region {region} readable {:#x}/{:#x}",
        data.len(),
        end - start
    );

    for (encoding, needle) in &needles {
        for index in find_all(&data, needle) {
            log::warn!(
                "[reffect-target-classify] name inline {region}+{:#05x} ({encoding})",
                start + index
            );
        }
    }

    let mut offset = start;
    while offset + size_of::<usize>() <= end {
        let Some(pointer) = value_at::<usize>(game, base, offset).filter(|p| valid_pointer(*p))
        else {
            offset += size_of::<usize>();
            continue;
        };

        for (wide, encoding) in [(false, "ascii"), (true, "utf16")] {
            if read_text(game, pointer, wide).as_deref() == Some(name) {
                log::warn!(
                    "[reffect-target-classify] name pointer {region}+{offset:#05x} -> {pointer:#x} ({encoding})"
                );
            }
        }

        let inner = read_region_bytes(game, pointer, 0, PROBE_NAME_INNER);
        for (encoding, needle) in &needles {
            for index in find_all(&inner, needle) {
                log::warn!(
                    "[reffect-target-classify] name {region}+{offset:#05x} -> {pointer:#x}+{index:#05x} ({encoding})"
                );
            }
        }

        offset += size_of::<usize>();
    }
}

/// Returns the name encoded as single byte characters and as UTF-16LE.
fn name_needles(name: &str) -> [(&'static str, Vec<u8>); 2] {
    [
        ("ascii", name.as_bytes().to_vec()),
        (
            "utf16",
            name.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>(),
        ),
    ]
}

/// Returns every offset at which `needle` occurs in `haystack`.
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut hits = Vec::new();
    if needle.is_empty() || haystack.len() < needle.len() {
        return hits;
    }

    let mut from = 0;
    while from + needle.len() <= haystack.len() {
        match haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
        {
            Some(index) => {
                hits.push(from + index);
                from += index + 1;
            }
            None => break,
        }
    }
    hits
}

/// Reads `[start, end)` at `base` as bytes, stopping at the first unreadable
/// word so a partially mapped struct is still searched instead of skipped.
fn read_region_bytes(game: &impl GameMemory, base: usize, start: usize, end: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(end - start);
    let mut offset = start;
    while offset < end {
        let Some(word) = value_at::<u32>(game, base, offset) else {
            break;
        };
        data.extend_from_slice(&word.to_le_bytes());
        offset += 4;
    }
    data
}

/// Formats an optional pointer for the probe log as `0x...`/`None`.
fn pointer_field(value: Option<usize>) -> String {
    match value {
        Some(value) => format!("{value:#x}"),
        None => String::from("None"),
    }
}

/// Logs one region as fixed size hexdump chunks, so a wide region stays
/// greppable and a partially unmapped one reports where it ends.
fn probe_region(game: &impl GameMemory, name: &str, base: usize, range: (usize, usize)) {
    let (start, end) = range;
    let mut offset = start;
    while offset < end {
        let len = PROBE_CHUNK.min(end - offset);
        log::warn!(
            "[reffect-target-classify] {name}[{offset:#05x}..{:#05x}]={}",
            offset + len,
            hexdump(game, base, offset, len)
        );
        offset += len;
    }
}

/// Formats `len` bytes at `base + offset` as space separated little-endian
/// `u32` words. Unreadable words become `????????`, so a shorter dump also
/// reports where the mapped region ends.
fn hexdump(game: &impl GameMemory, base: usize, offset: usize, len: usize) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(len / 4 * 9);
    for step in (0..len).step_by(4) {
        match value_at::<u32>(game, base, offset + step) {
            Some(word) => {
                let _ = write!(out, "{word:08x} ");
            }
            None => out.push_str("???????? "),
        }
    }
    out
}

/// Rate-limited diagnostic for a selected target the reader rejects, so a
/// target kind whose layout differs can be identified from the log.
fn log_target_rejection(
    combatant: usize,
    bridge: usize,
    selected: usize,
    character_plus_48: Option<usize>,
    character: Option<usize>,
    agent: Option<usize>,
    local: usize,
) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static LAST: AtomicU32 = AtomicU32::new(0);
    let now = unsafe { GetTickCount() };
    let last = LAST.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < 1_000 {
        return;
    }
    if LAST
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    log::warn!(
        "[reffect-runtime] target rejected: combatant={combatant:#x} bridge={bridge:#x} selected={selected:#x} c48={character_plus_48:?} character={character:?} agent={agent:?} local={local:#x}"
    );
}

fn pointer_at(game: &impl GameMemory, base: usize, offset: usize) -> Option<usize> {
    raw_pointer_at(game, base, offset).filter(|pointer| valid_pointer(*pointer))
}

fn raw_pointer_at(game: &impl GameMemory, base: usize, offset: usize) -> Option<usize> {
    game.read(base.checked_add(offset)?)
}

fn value_at<T: Copy>(game: &impl GameMemory, base: usize, offset: usize) -> Option<T> {
    game.read(base.checked_add(offset)?)
}

fn valid_pointer(pointer: usize) -> bool {
    (0x1_0000..=0x0000_7fff_ffff_ffff).contains(&pointer)
}

fn is_owned_companion_address(owner: usize, candidate: usize) -> bool {
    candidate != owner && valid_pointer(candidate)
}

fn node_in_range(node: usize, base: usize, length: usize) -> bool {
    node >= base && node < base.saturating_add(length)
}

fn sane_health(current: f32, max: f32) -> bool {
    current.is_finite()
        && max.is_finite()
        && current >= 0.0
        && max > 0.0
        && max <= 100_000_000.0
        && current <= max * 1.25
}

fn sane_resource(current: f32, max: f32) -> bool {
    current.is_finite()
        && max.is_finite()
        && current >= 0.0
        && max > 0.0
        && max <= 1_000.0
        && current <= max * 1.2
}

/// Necromancer life force is a raw-point resource whose max routinely reaches
/// five figures (verified live at 19228 vs 0.13.1), so it cannot share the
/// generic `sane_resource` cap of 1000. It uses the same shape validation with
/// a life-force-appropriate upper bound instead.
fn sane_life_force(current: f32, max: f32) -> bool {
    current.is_finite()
        && max.is_finite()
        && current >= 0.0
        && max > 0.0
        && max <= 1_000_000.0
        && current <= max * 1.2
}

fn sane_skill_id(id: u32) -> bool {
    (1..=200_000).contains(&id) && id != LOCKED_SKILL_ID
}

fn sane_cooldown_total(total: u32) -> bool {
    (250..=600_000).contains(&total)
}

fn is_standard_combat_buff(id: u32) -> bool {
    matches!(
        id,
        717 | 718
            | 719
            | 720
            | 721
            | 722
            | 723
            | 725
            | 726
            | 727
            | 736
            | 737
            | 738
            | 740
            | 742
            | 743
            | 791
            | 833
            | 861
            | 872
            | 873
            | 1122
            | 1187
            | 8730
            | 17_675
            | 19_426
            | 26_766
            | 26_980
            | 27_705
            | 30_328
    )
}

fn is_intensity_buff(id: u32) -> bool {
    matches!(id, 723 | 736 | 737 | 738 | 740 | 861 | 1122 | 19_426)
}

fn tick_count() -> u32 {
    unsafe { GetTickCount() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reffect_core::context::ProfSelection;

    #[derive(Default)]
    struct LegacyMixedTreeMemory {
        words: std::collections::BTreeMap<usize, u64>,
        bytes: std::collections::BTreeMap<usize, u8>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl LegacyMixedTreeMemory {
        fn pointer(&mut self, address: usize, value: usize) {
            self.words.insert(address, value as u64);
        }

        fn u32(&mut self, address: usize, value: u32) {
            self.words.insert(address, u64::from(value));
        }

        fn f32(&mut self, address: usize, value: f32) {
            self.u32(address, value.to_bits());
        }

        /// Stores byte granular data, for the string reader.
        fn bytes(&mut self, address: usize, data: &[u8]) {
            for (index, byte) in data.iter().enumerate() {
                self.bytes.insert(address + index, *byte);
            }
        }

        fn read_count(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl GameMemory for LegacyMixedTreeMemory {
        fn read<T: Copy>(&self, address: usize) -> Option<T> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match std::mem::size_of::<T>() {
                1 => {
                    let value = *self.bytes.get(&address)?;
                    Some(unsafe { std::mem::transmute_copy(&value) })
                }
                2 => {
                    let low = u16::from(*self.bytes.get(&address)?);
                    let high = u16::from(*self.bytes.get(&(address + 1))?);
                    let value = low | (high << 8);
                    Some(unsafe { std::mem::transmute_copy(&value) })
                }
                4 => {
                    let value = *self.words.get(&address)? as u32;
                    Some(unsafe { std::mem::transmute_copy(&value) })
                }
                8 => {
                    let value = *self.words.get(&address)?;
                    Some(unsafe { std::mem::transmute_copy(&value) })
                }
                _ => None,
            }
        }
    }

    struct GroupFixture {
        memory: LegacyMixedTreeMemory,
        collection: usize,
        local: usize,
        first: usize,
        second: usize,
    }

    fn store_identity(
        memory: &mut LegacyMixedTreeMemory,
        base: usize,
        first: usize,
        second: usize,
    ) {
        memory.pointer(base, first);
        memory.pointer(base + size_of::<usize>(), second);
    }

    fn store_member_buffs(memory: &mut LegacyMixedTreeMemory, character: usize, effect_id: u32) {
        let container = character + 0x1_000;
        let node = character + 0x2_000;
        let wrapper = character + 0x3_000;
        memory.pointer(character + EFFECT_TREE_CONTAINER, container);
        memory.u32(container + EFFECT_TREE_NODE_OFFSET, 0x58);
        memory.pointer(container + EFFECT_TREE_ROOT, node);
        memory.pointer(node + EFFECT_NODE_WRAPPER, wrapper);
        memory.u32(wrapper + EFFECT_WRAPPER_STATE, 1);
        memory.pointer(wrapper + EFFECT_WRAPPER_VALUE, wrapper + 0x80);
        memory.u32(wrapper + EFFECT_WRAPPER_ID, effect_id);
        memory.u32(node + EFFECT_NODE_DURATION, 5_000);
        memory.u32(node + EFFECT_NODE_APPLIED_AT, 100);
        memory.pointer(node + 0x60, 1);
    }

    fn store_member_resources(memory: &mut LegacyMixedTreeMemory, character: usize, current: f32) {
        let health = character + 0x4_000;
        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, current);
        memory.f32(health + HEALTH_MAX, 100.0);
        memory.f32(health + HEALTH_BARRIER, 10.0);
    }

    fn group_fixture() -> GroupFixture {
        let collection = 0x10_000;
        let group_root = 0x20_000;
        let directories = 0x30_000;
        let identities = 0x40_000;
        let local = 0x50_000;
        let member_table = 0x60_000;
        let entries = 0x70_000;
        let local_entry = 0x78_000;
        let first_entry = 0x80_000;
        let second_entry = 0x90_000;
        let outside_entry = 0xA0_000;
        let first = 0xB0_000;
        let second = 0xC0_000;
        let outside = 0xD0_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(collection + GROUP_CONTEXT_ROOT, group_root);
        memory.pointer(group_root + GROUP_DIRECTORY_TABLE, directories);
        memory.u32(directories + GROUP_DIRECTORY_ACTIVE, 1);
        memory.u32(directories + GROUP_DIRECTORY_IDENTITY_COUNT, 3);
        memory.pointer(directories + GROUP_DIRECTORY_IDENTITIES, identities);
        store_identity(&mut memory, identities, 0x11_1111, 0x22_2222);
        store_identity(
            &mut memory,
            identities + GROUP_IDENTITY_STRIDE,
            0x33_3333,
            0x44_4444,
        );
        store_identity(
            &mut memory,
            identities + GROUP_IDENTITY_STRIDE * 2,
            0x55_5555,
            0x66_6666,
        );
        store_identity(
            &mut memory,
            local + CHARACTER_GROUP_IDENTITY,
            0x11_1111,
            0x22_2222,
        );
        store_identity(
            &mut memory,
            first + CHARACTER_GROUP_IDENTITY,
            0x33_3333,
            0x44_4444,
        );
        store_identity(
            &mut memory,
            second + CHARACTER_GROUP_IDENTITY,
            0x55_5555,
            0x66_6666,
        );
        store_identity(
            &mut memory,
            outside + CHARACTER_GROUP_IDENTITY,
            0x77_7777,
            0x88_8888,
        );

        memory.pointer(local + CHARACTER_GROUP_MEMBERS, member_table);
        memory.pointer(member_table + GROUP_MEMBER_ENTRIES, entries);
        memory.u32(member_table + GROUP_MEMBER_COUNT, 4);
        memory.pointer(entries, local_entry);
        memory.pointer(entries + size_of::<usize>(), first_entry);
        memory.pointer(entries + size_of::<usize>() * 2, second_entry);
        memory.pointer(entries + size_of::<usize>() * 3, outside_entry);
        memory.pointer(local_entry + GROUP_MEMBER_CHARACTER, local);
        memory.pointer(first_entry + GROUP_MEMBER_CHARACTER, first);
        memory.pointer(second_entry + GROUP_MEMBER_CHARACTER, second);
        memory.pointer(outside_entry + GROUP_MEMBER_CHARACTER, outside);

        store_member_resources(&mut memory, first, 75.0);
        store_member_resources(&mut memory, second, 66.0);
        store_member_resources(&mut memory, outside, 50.0);
        store_member_buffs(&mut memory, first, 740);
        store_member_buffs(&mut memory, second, 717);
        store_member_buffs(&mut memory, outside, 723);

        GroupFixture {
            memory,
            collection,
            local,
            first,
            second,
        }
    }

    #[test]
    fn group_reader_publishes_only_matching_non_local_members() {
        let fixture = group_fixture();
        let mut cache = BuffSnapshotCache::default();

        let members = read_group_members(
            &fixture.memory,
            fixture.collection,
            fixture.local,
            &mut cache,
            1_000,
        )
        .expect("validated group tree");

        assert_eq!(members[0].resources.as_ref().unwrap().health.current, 75.0);
        assert!(members[0].buffs.as_ref().unwrap().contains_key(&740));
        assert_eq!(members[1].resources.as_ref().unwrap().health.current, 66.0);
        assert!(members[1].buffs.as_ref().unwrap().contains_key(&717));
        assert!(matches!(members[2].resources, Err(Error::Disabled)));
        assert!(members.iter().all(|member| member.account.is_none()));
    }

    #[test]
    fn group_reader_keeps_other_members_when_one_health_read_fails() {
        let mut fixture = group_fixture();
        let second_health = fixture.second + 0x4_000 + HEALTH_CURRENT;
        fixture.memory.words.remove(&second_health);
        let mut cache = BuffSnapshotCache::default();

        let members = read_group_members(
            &fixture.memory,
            fixture.collection,
            fixture.local,
            &mut cache,
            1_000,
        )
        .expect("validated group tree");

        assert_eq!(members[0].resources.as_ref().unwrap().health.current, 75.0);
        assert!(matches!(members[1].resources, Err(Error::Health)));
        assert!(members[1].buffs.as_ref().unwrap().contains_key(&717));
    }

    #[test]
    fn group_resources_are_normalized_to_percent() {
        let mut fixture = group_fixture();
        let first_health = fixture.first + 0x4_000;
        fixture.memory.f32(first_health + HEALTH_MAX, 200.0);
        let mut cache = BuffSnapshotCache::default();

        let members = read_group_members(
            &fixture.memory,
            fixture.collection,
            fixture.local,
            &mut cache,
            1_000,
        )
        .expect("validated group tree");
        let resources = members[0].resources.as_ref().unwrap();

        assert_eq!(resources.health.current, 37.5);
        assert_eq!(resources.health.max, 100.0);
        assert_eq!(resources.barrier.current, 5.0);
        assert_eq!(resources.barrier.max, 100.0);
    }

    #[test]
    fn group_reader_skips_an_unreadable_candidate_entry() {
        let mut fixture = group_fixture();
        fixture
            .memory
            .words
            .remove(&(0x70_000 + size_of::<usize>() * 3));
        let mut cache = BuffSnapshotCache::default();

        let members = read_group_members(
            &fixture.memory,
            fixture.collection,
            fixture.local,
            &mut cache,
            1_000,
        )
        .expect("other valid entries must still be published");

        assert_eq!(members[0].resources.as_ref().unwrap().health.current, 75.0);
        assert_eq!(members[1].resources.as_ref().unwrap().health.current, 66.0);
    }

    #[test]
    fn group_reader_rejects_a_directory_without_the_local_identity() {
        let mut fixture = group_fixture();
        let identity = 0x40_000;
        store_identity(&mut fixture.memory, identity, 0x99_9999, 0xAA_AAAA);
        let mut cache = BuffSnapshotCache::default();

        assert!(
            read_group_members(
                &fixture.memory,
                fixture.collection,
                fixture.local,
                &mut cache,
                1_000,
            )
            .is_none()
        );
    }

    #[test]
    fn group_buffs_exclude_generic_effects() {
        let mut buffs = BuffMap::new();
        buffs.insert(740, Buff::hidden_duration());
        buffs.insert(12_345, Buff::hidden_duration());

        filter_group_buffs(&mut buffs);

        assert!(buffs.contains_key(&740));
        assert!(!buffs.contains_key(&12_345));
    }

    #[test]
    fn direct_effect_tree_filters_permanent_equipment_effects() {
        const LEGACY_CHARACTER_EFFECTS: usize = 0xD0;
        const LEGACY_TREE_NODE_OFFSET: usize = 0x38;
        const LEGACY_TREE_ROOT: usize = 0x48;
        const LEGACY_NODE_WRAPPER: usize = 0x10;
        const LEGACY_WRAPPER_ID: usize = 0x28;
        const LEGACY_WRAPPER_STATE: usize = 0x58;
        const LEGACY_NODE_DURATION: usize = 0x40;
        const LEGACY_NODE_APPLIED_AT: usize = 0x44;
        let character = 0x10_000;
        let container = 0x20_000;
        let node = 0x30_000;
        let wrapper = 0x40_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + LEGACY_CHARACTER_EFFECTS, container);
        memory.pointer(container + LEGACY_TREE_ROOT, node);
        memory.u32(container + LEGACY_TREE_NODE_OFFSET, 0x58);
        memory.pointer(node + LEGACY_NODE_WRAPPER, wrapper);
        memory.u32(wrapper + LEGACY_WRAPPER_STATE, 1);
        // 739 is documented as a false-positive row in the legacy mixed tree.
        memory.u32(wrapper + LEGACY_WRAPPER_ID, 739);
        memory.u32(node + LEGACY_NODE_DURATION, 0);
        memory.u32(node + LEGACY_NODE_APPLIED_AT, 0);
        memory.pointer(node + 0x60, 0);

        assert!(
            read_buffs(&memory, character)
                .expect("validated empty effect tree")
                .is_empty()
        );
    }

    #[test]
    fn unavailable_buff_reader_stops_after_the_effect_container_read() {
        let memory = LegacyMixedTreeMemory::default();

        assert!(read_buffs(&memory, 0x10_000).is_none());
        // The direct reader starts from `character + 0xD0` and must stop at
        // the missing container without attempting unrelated pointer reads.
        assert_eq!(memory.read_count(), 1);
    }

    #[test]
    fn direct_effect_tree_publishes_timed_buffs_without_equipment_effects() {
        let character = 0x10_000;
        let container = 0x20_000;
        let first_node = 0x30_000;
        let second_node = 0x40_000;
        let equipment_node = 0x50_000;
        let first_wrapper = 0x60_000;
        let second_wrapper = 0x70_000;
        let equipment_wrapper = 0x80_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + 0xD0, container);
        memory.u32(container + 0x38, 0x58);
        memory.pointer(container + 0x48, first_node);
        memory.pointer(first_node + 0x10, first_wrapper);
        memory.u32(first_wrapper + 0x58, 1);
        memory.u32(first_wrapper + 0x28, 740);
        memory.pointer(first_wrapper + 0x60, first_wrapper + 0xD8);
        memory.u32(first_node + 0x40, 5_000);
        memory.u32(first_node + 0x44, 100);
        memory.pointer(first_node + 0x60, second_node);

        memory.pointer(second_node + 0x10, second_wrapper);
        memory.u32(second_wrapper + 0x58, 1);
        memory.u32(second_wrapper + 0x28, 740);
        memory.pointer(second_wrapper + 0x60, second_wrapper + 0xD8);
        memory.u32(second_node + 0x40, 3_000);
        memory.u32(second_node + 0x44, 200);
        memory.pointer(second_node + 0x60, equipment_node);

        memory.pointer(equipment_node + 0x10, equipment_wrapper);
        memory.u32(equipment_wrapper + 0x58, 1);
        memory.u32(equipment_wrapper + 0x28, 21_824);
        memory.pointer(equipment_wrapper + 0x60, equipment_wrapper + 0xD8);
        memory.u32(equipment_node + 0x40, u32::MAX);
        memory.u32(equipment_node + 0x44, 100);
        memory.pointer(equipment_node + 0x60, 1);

        let buffs = read_buffs_at(&memory, character, 1_000).expect("validated effect tree");

        assert_eq!(buffs.len(), 1);
        assert_eq!(buffs[&740].stacks, 2);
        assert_eq!(buffs[&740].apply_time, 100);
        assert_eq!(buffs[&740].runout_time, 5_100);
    }

    #[test]
    fn player_buff_info_reads_screen_border_and_squad_highlight_ticks() {
        const LAST_SCREEN_BORDER: usize = 0xC8;
        const LAST_SQUAD_HIGHLIGHT: usize = 0xC0;
        const ACTIVITY_TICK: usize = 0x28;
        let character = 0x10_000;
        let container = 0x20_000;
        let screen_border = 0x30_000;
        let squad_highlight = 0x40_000;
        let mut memory = LegacyMixedTreeMemory::default();
        let mut cache = BuffSnapshotCache::default();

        memory.pointer(character + EFFECT_TREE_CONTAINER, container);
        memory.u32(container + EFFECT_TREE_NODE_OFFSET, 0x58);
        memory.pointer(container + LAST_SCREEN_BORDER, screen_border);
        memory.u32(screen_border + ACTIVITY_TICK, 1_234);
        memory.pointer(container + LAST_SQUAD_HIGHLIGHT, squad_highlight);
        memory.u32(squad_highlight + ACTIVITY_TICK, 5_678);

        let info = read_player_buff_info(&memory, &mut cache, character, 1_000)
            .expect("valid player buff info");

        assert_eq!(info.last_screen_border, 1_234);
        assert_eq!(info.last_squad_highlight, 5_678);
    }

    #[test]
    fn unavailable_buffs_do_not_block_resources_or_skillbar_publication() {
        let snapshot = RuntimeSnapshot {
            now: 100,
            race: None,
            profession: None,
            specialization: None,
            resources: Some(PlayerResourceSnapshot {
                health: PlayerHealth::empty(),
                resources: PlayerResources::empty(),
            }),
            buffs: None,
            build: None,
            gear: None,
            skillbar: Some(Skillbar::empty()),
            active_cooldowns: None,
            target: None,
            group: None,
        };
        let mut context = Context::empty();

        snapshot.apply_to(&mut context);

        assert!(context.player.resources.is_ok());
        assert!(context.player.skillbar.is_ok());
        assert!(matches!(context.player.buff_info, Err(Error::Buffs)));
        assert!(matches!(context.group, Err(Error::Group)));
    }

    #[test]
    fn thief_primary_resource_uses_profession_current_and_max() {
        let character = 0x10_000;
        let agent = 0x20_000;
        let core = 0x30_000;
        let health = 0x40_000;
        let generic_energy = 0x50_000;
        let profession = 0x60_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_AGENT, agent);
        memory.pointer(character + CHARACTER_CORE_STATS, core);
        memory.u32(core + CORE_PROFESSION, 5);
        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION_ENERGY, generic_energy);
        memory.f32(generic_energy + PROFESSION_ENERGY_CURRENT, 9.0);
        memory.f32(generic_energy + PROFESSION_ENERGY_MAX, 99.0);
        memory.pointer(character + 0x510, profession);
        memory.f32(profession + 0x88, 12.0);
        memory.f32(profession + 0x8C, 15.0);

        let snapshot = decode_snapshot(
            &memory,
            0x20_000,
            character,
            &mut BuffSnapshotCache::default(),
        )
        .expect("valid thief snapshot");
        let primary = snapshot
            .resources
            .expect("player resources")
            .resources
            .primary;

        assert_eq!(snapshot.profession, Some(5));
        assert_eq!(primary.current, 12.0);
        assert_eq!(primary.max, 15.0);
    }

    #[test]
    fn thief_primary_resource_advances_from_profession_sample_tick_and_rate() {
        // Ground-truth offsets from the original reader (`sub_1800E26B0`
        // case 5): the sample timestamp sits at profObj+0x94 and the regen rate
        // at profObj+0x90. Literals are used so this test itself pins the
        // absolute layout and would fail if the production constants were ever
        // swapped back.
        const SAMPLE_TICK: usize = 0x94;
        const RATE: usize = 0x90;
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.f32(profession + THIEF_PRIMARY_CURRENT, 10.0);
        memory.f32(profession + THIEF_PRIMARY_MAX, 15.0);
        memory.u32(profession + SAMPLE_TICK, 1_000);
        memory.f32(profession + RATE, 2.0);

        let resources =
            read_player_resources_for_profession_at(&memory, character, Some(5), None, 1_500)
                .expect("valid thief resources");

        assert_eq!(resources.resources.primary.current, 11.0);
        assert_eq!(resources.resources.primary.max, 15.0);
    }

    #[test]
    fn revenant_primary_uses_the_generic_character_energy_snapshot() {
        let character = 0x10_000;
        let health = 0x20_000;
        let energy = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION_ENERGY, energy);
        memory.f32(energy + PROFESSION_ENERGY_CURRENT, 30.0);
        memory.f32(energy + PROFESSION_ENERGY_MAX, 50.0);
        memory.f32(energy + PROFESSION_ENERGY_RATE, 2.0);
        memory.u32(energy + PROFESSION_ENERGY_SAMPLE_TICK, 1_000);

        let resources =
            read_player_resources_for_profession_at(&memory, character, Some(9), None, 1_500)
                .expect("valid revenant resources");

        assert_eq!(resources.resources.primary.current, 31.0);
        assert_eq!(resources.resources.primary.max, 50.0);
        assert_eq!(resources.resources.rate.current, 7.0);
        assert_eq!(resources.resources.rate.max, 10.0);
    }

    #[test]
    fn warrior_primary_uses_the_profession_integer_pair() {
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.u32(profession + 0x50, 15);
        memory.u32(profession + 0x54, 30);

        let resources =
            read_player_resources_for_profession_at(&memory, character, Some(2), None, 1_500)
                .expect("valid warrior resources");

        assert_eq!(resources.resources.primary.current, 15.0);
        assert_eq!(resources.resources.primary.max, 30.0);
    }

    #[test]
    fn mesmer_primary_uses_the_profession_integer_pair() {
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.u32(profession + 0x50, 3);
        memory.u32(profession + 0x54, 5);

        let resources =
            read_player_resources_for_profession_at(&memory, character, Some(7), None, 1_500)
                .expect("valid mesmer resources");

        assert_eq!(resources.resources.primary.current, 3.0);
        assert_eq!(resources.resources.primary.max, 5.0);
    }

    #[test]
    fn necromancer_primary_rounds_the_profession_float_pair_upward() {
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.f32(profession + 0x50, 3.2);
        memory.f32(profession + 0x54, 5.8);

        let resources =
            read_player_resources_for_profession_at(&memory, character, Some(8), None, 1_500)
                .expect("valid necromancer resources");

        assert_eq!(resources.resources.primary.current, 4.0);
        assert_eq!(resources.resources.primary.max, 6.0);
    }

    #[test]
    fn necromancer_life_force_accepts_a_five_figure_max_from_the_live_client() {
        // Ground truth captured live vs Reffect.dll 0.13.1 (Necromancer):
        // profObj+0x50 = 12071.67 (current), profObj+0x54 = 19227.82 (max),
        // displayed as 12072/19228. The generic `sane_resource` cap of 1000
        // would reject this; `sane_life_force` must accept it.
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 23_222.0);
        memory.f32(health + HEALTH_MAX, 23_222.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.f32(profession + PROFESSION_PRIMARY_CURRENT, 12_071.67);
        memory.f32(profession + PROFESSION_PRIMARY_MAX, 19_227.82);

        let resources =
            read_player_resources_for_profession_at(&memory, character, Some(8), None, 1_500)
                .expect("necro life force with a five-figure max must be published");

        assert_eq!(resources.resources.primary.current, 12_072.0);
        assert_eq!(resources.resources.primary.max, 19_228.0);
    }

    #[test]
    fn elementalist_evoker_charges_come_from_the_scaled_profession_pair() {
        // Ground truth captured live vs Reffect.dll 0.13.1 (Elementalist /
        // Evoker, 2026-09-23): profObj+0x80 = 20, profObj+0x84 = 30, each
        // divided by 5.0 -> basic charges 4/6, matching the plugin's own
        // published output. Secondary (enhanced charges) publishes max 3 with a
        // zero current until the effect-stack count is wired in.
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.u32(profession + ELEMENTALIST_CHARGE_CURRENT, 20);
        memory.u32(profession + ELEMENTALIST_CHARGE_MAX, 30);

        let resources = read_player_resources_for_profession_at(
            &memory,
            character,
            Some(6),
            Some(SPEC_ELEMENTALIST_EVOKER),
            1_500,
        )
        .expect("valid elementalist resources");

        assert_eq!(resources.resources.primary.current, 4.0);
        assert_eq!(resources.resources.primary.max, 6.0);
        assert_eq!(resources.resources.secondary.current, 0.0);
        assert_eq!(resources.resources.secondary.max, 3.0);
    }

    #[test]
    fn elementalist_catalyst_reads_the_raw_profession_pair() {
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.u32(profession + ELEMENTALIST_CHARGE_CURRENT, 7);
        memory.u32(profession + ELEMENTALIST_CHARGE_MAX, 10);

        let resources = read_player_resources_for_profession_at(
            &memory,
            character,
            Some(6),
            Some(SPEC_ELEMENTALIST_CATALYST),
            1_500,
        )
        .expect("valid elementalist resources");

        assert_eq!(resources.resources.primary.current, 7.0);
        assert_eq!(resources.resources.primary.max, 10.0);
        assert!(
            resources.resources.secondary.max == 0.0
                || resources.resources.secondary.current == 0.0
        );
    }

    #[test]
    fn elementalist_evoker_enhanced_charge_counts_effect_tree_stacks() {
        // Live-verified (2026-09-23): the Evoker enhanced (secondary) current is
        // the number of effect id 76749 nodes in the character effect tree; a
        // single stack reads as 1/3, matching the original plugin's output.
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let container = 0x40_000;
        let node = 0x50_000;
        let wrapper = 0x60_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.u32(profession + ELEMENTALIST_CHARGE_CURRENT, 0);
        memory.u32(profession + ELEMENTALIST_CHARGE_MAX, 30);

        memory.pointer(character + EFFECT_TREE_CONTAINER, container);
        memory.u32(container + EFFECT_TREE_NODE_OFFSET, 0x58);
        memory.pointer(container + EFFECT_TREE_ROOT, node);
        memory.pointer(node + EFFECT_NODE_WRAPPER, wrapper);
        memory.u32(wrapper + EFFECT_WRAPPER_STATE, 1);
        memory.u32(wrapper + EFFECT_WRAPPER_ID, EVOKER_ENHANCED_STACK_ID);
        memory.pointer(wrapper + EFFECT_WRAPPER_VALUE, wrapper + 0xD8);
        memory.pointer(node + 0x60, 1);

        let resources = read_player_resources_for_profession_at(
            &memory,
            character,
            Some(6),
            Some(SPEC_ELEMENTALIST_EVOKER),
            1_500,
        )
        .expect("valid elementalist resources");

        assert_eq!(resources.resources.primary.current, 0.0);
        assert_eq!(resources.resources.primary.max, 6.0);
        assert_eq!(resources.resources.secondary.current, 1.0);
        assert_eq!(resources.resources.secondary.max, 3.0);
    }

    #[test]
    fn guardian_willbender_primary_uses_the_profession_pair() {
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.u32(profession + WILLBENDER_PRIMARY_CURRENT, 2);
        memory.u32(profession + WILLBENDER_PRIMARY_MAX, 3);

        let resources = read_player_resources_for_profession_at(
            &memory,
            character,
            Some(1),
            Some(SPEC_GUARDIAN_WILLBENDER),
            1_500,
        )
        .expect("valid guardian resources");

        assert_eq!(resources.resources.primary.current, 2.0);
        assert_eq!(resources.resources.primary.max, 3.0);
    }

    #[test]
    fn warrior_berserker_secondary_counts_burst_stacks() {
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let container = 0x40_000;
        let node = 0x50_000;
        let wrapper = 0x60_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.u32(profession + PROFESSION_PRIMARY_CURRENT, 20);
        memory.u32(profession + PROFESSION_PRIMARY_MAX, 30);

        memory.pointer(character + EFFECT_TREE_CONTAINER, container);
        memory.u32(container + EFFECT_TREE_NODE_OFFSET, 0x58);
        memory.pointer(container + EFFECT_TREE_ROOT, node);
        memory.pointer(node + EFFECT_NODE_WRAPPER, wrapper);
        memory.u32(wrapper + EFFECT_WRAPPER_STATE, 1);
        memory.u32(wrapper + EFFECT_WRAPPER_ID, WARRIOR_BERSERKER_BURST_ID);
        memory.pointer(wrapper + EFFECT_WRAPPER_VALUE, wrapper + 0xD8);
        memory.pointer(node + 0x60, 1);

        let resources = read_player_resources_for_profession_at(
            &memory,
            character,
            Some(2),
            Some(SPEC_WARRIOR_BERSERKER),
            1_500,
        )
        .expect("valid warrior resources");

        assert_eq!(resources.resources.primary.current, 20.0);
        assert_eq!(resources.resources.primary.max, 30.0);
        assert_eq!(resources.resources.secondary.current, 1.0);
        assert_eq!(resources.resources.secondary.max, WARRIOR_BERSERKER_MAX);
    }

    #[test]
    fn necromancer_harbinger_secondary_uses_the_profession_pair() {
        let character = 0x10_000;
        let health = 0x20_000;
        let profession = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION, profession);
        memory.f32(profession + PROFESSION_PRIMARY_CURRENT, 1463.0);
        memory.f32(profession + PROFESSION_PRIMARY_MAX, 19228.0);
        memory.u32(profession + HARBINGER_CURRENT, 3);
        memory.u32(profession + HARBINGER_MAX, 10);

        let resources = read_player_resources_for_profession_at(
            &memory,
            character,
            Some(8),
            Some(SPEC_NECROMANCER_HARBINGER),
            1_500,
        )
        .expect("valid necromancer resources");

        assert_eq!(resources.resources.primary.current, 1463.0);
        assert_eq!(resources.resources.primary.max, 19228.0);
        assert_eq!(resources.resources.secondary.current, 3.0);
        assert_eq!(resources.resources.secondary.max, 10.0);
    }

    #[test]
    fn revenant_vindicator_secondary_counts_alliance_stacks() {
        let character = 0x10_000;
        let health = 0x20_000;
        let energy = 0x30_000;
        let container = 0x40_000;
        let node = 0x50_000;
        let wrapper = 0x60_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION_ENERGY, energy);
        memory.f32(energy + PROFESSION_ENERGY_CURRENT, 30.0);
        memory.f32(energy + PROFESSION_ENERGY_MAX, 50.0);
        memory.f32(energy + PROFESSION_ENERGY_RATE, 0.0);
        memory.u32(energy + PROFESSION_ENERGY_SAMPLE_TICK, 1_000);

        memory.pointer(character + EFFECT_TREE_CONTAINER, container);
        memory.u32(container + EFFECT_TREE_NODE_OFFSET, 0x58);
        memory.pointer(container + EFFECT_TREE_ROOT, node);
        memory.pointer(node + EFFECT_NODE_WRAPPER, wrapper);
        memory.u32(wrapper + EFFECT_WRAPPER_STATE, 1);
        memory.u32(wrapper + EFFECT_WRAPPER_ID, VINDICATOR_ALLIANCE_ID);
        memory.pointer(wrapper + EFFECT_WRAPPER_VALUE, wrapper + 0xD8);
        memory.pointer(node + 0x60, 1);

        let resources = read_player_resources_for_profession_at(
            &memory,
            character,
            Some(9),
            Some(SPEC_REVENANT_VINDICATOR),
            1_500,
        )
        .expect("valid revenant resources");

        assert_eq!(resources.resources.secondary.current, 1.0);
        assert_eq!(resources.resources.secondary.max, 5.0);
    }

    #[test]
    fn health_reduction_tracks_missing_player_health() {
        let character = 0x10_000;
        let health = 0x40_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 80.0);
        memory.f32(health + HEALTH_MAX, 100.0);
        memory.f32(health + HEALTH_BARRIER, 0.0);

        let resources = read_player_resources(&memory, character).expect("valid player resources");

        assert_eq!(resources.health.combatant.health.current, 80.0);
        assert_eq!(resources.health.combatant.health.max, 100.0);
        assert_eq!(resources.health.health_reduction.current, 20.0);
        assert_eq!(resources.health.health_reduction.max, 100.0);
    }

    #[test]
    fn health_interpolates_from_the_sample_tick_and_rate() {
        // The health object stores a last-tick sample plus a regen rate; the
        // original reader (`collector_E2EC0`) interpolates the displayed value.
        let character = 0x10_000;
        let health = 0x20_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.u32(health + HEALTH_SAMPLE_TICK, 1_000);
        memory.f32(health + HEALTH_CURRENT, 80.0);
        memory.f32(health + HEALTH_MAX, 100.0);
        memory.f32(health + HEALTH_RATE, 10.0);

        let resources =
            read_player_resources_for_profession_at(&memory, character, None, None, 1_500)
                .expect("valid player resources");

        // now(1500) - tick(1000) = 0.5s * rate(10) = +5.0 -> 85.0
        assert_eq!(resources.health.combatant.health.current, 85.0);
        assert_eq!(resources.health.combatant.health.max, 100.0);
    }

    #[test]
    fn rtapi_group_types_publish_a_usable_group_state() {
        use nexus::rtapi::GroupType as RtapiGroupType;
        use reffect_core::context::GroupType;

        assert_eq!(
            group_type_from_rtapi(Ok::<RtapiGroupType, u32>(RtapiGroupType::None)),
            GroupType::Solo
        );
        assert_eq!(
            group_type_from_rtapi(Ok::<RtapiGroupType, u32>(RtapiGroupType::Party)),
            GroupType::Party
        );
        assert_eq!(
            group_type_from_rtapi(Ok::<RtapiGroupType, u32>(RtapiGroupType::Squad)),
            GroupType::Squad
        );
        assert_eq!(
            group_type_from_rtapi(Ok::<RtapiGroupType, u32>(RtapiGroupType::RaidSquad)),
            GroupType::Squad
        );
        assert_eq!(group_type_from_rtapi(Err(99)), GroupType::Solo);
    }

    #[test]
    fn thief_without_a_profession_object_does_not_substitute_generic_energy() {
        let character = 0x10_000;
        let agent = 0x20_000;
        let core = 0x30_000;
        let health = 0x40_000;
        let generic_energy = 0x50_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_AGENT, agent);
        memory.pointer(character + CHARACTER_CORE_STATS, core);
        memory.u32(core + CORE_PROFESSION, 5);
        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION_ENERGY, generic_energy);
        memory.f32(generic_energy + PROFESSION_ENERGY_CURRENT, 15.0);
        memory.f32(generic_energy + PROFESSION_ENERGY_MAX, 15.0);

        let snapshot = decode_snapshot(
            &memory,
            0x20_000,
            character,
            &mut BuffSnapshotCache::default(),
        )
        .expect("valid thief snapshot");
        let primary = snapshot
            .resources
            .expect("player resources")
            .resources
            .primary;

        assert_eq!(primary.current, 0.0);
        assert_eq!(primary.max, 0.0);
    }

    #[test]
    fn primary_resource_advances_from_the_sample_tick_at_its_reported_rate() {
        let character = 0x10_000;
        let health = 0x40_000;
        let generic_energy = 0x50_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_HEALTH, health);
        memory.f32(health + HEALTH_CURRENT, 12_345.0);
        memory.f32(health + HEALTH_MAX, 12_345.0);
        memory.pointer(character + CHARACTER_PROFESSION_ENERGY, generic_energy);
        memory.f32(generic_energy + PROFESSION_ENERGY_CURRENT, 10.0);
        memory.f32(generic_energy + PROFESSION_ENERGY_MAX, 15.0);
        memory.u32(generic_energy + 0x08, 1_000);
        memory.f32(generic_energy + 0x14, 2.0);

        let resources =
            read_player_resources_at(&memory, character, 1_500).expect("valid player resources");

        assert_eq!(resources.resources.primary.current, 11.0);
        assert_eq!(resources.resources.primary.max, 15.0);
    }

    #[test]
    fn skill_map_matches_public_slot_order() {
        assert_eq!(slot_map()[0], (5, Slot::Weapon1));
        assert_eq!(slot_map()[5], (0, Slot::Heal));
        assert_eq!(slot_map()[16], (21, Slot::Mount));
    }

    #[test]
    fn weapon_swap_uses_the_separate_combatant_cooldown_state() {
        let character = 0x10_000;
        let combatant = 0x20_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.u32(character + 0xD8, 0);
        memory.u32(combatant + 0x28C, 1_000);
        memory.u32(combatant + 0x290, 8_000);

        let skillbar = read_skillbar(&memory, character, 1_500)
            .expect("a valid standalone weapon-swap state must publish a skillbar");
        let weapon_swap = skillbar
            .bar
            .slot(Slot::WeaponSwap)
            .expect("the separate cooldown state must populate Weapon Swap");

        assert_eq!(weapon_swap.id, SkillId::WeaponSwap);
        assert_eq!(weapon_swap.ammo, 0);
        assert_eq!(weapon_swap.last_update, 1_000);
        assert_eq!(weapon_swap.recharge, 8_000);
        assert_eq!(weapon_swap.recharge_remaining, 8_000);
        assert_eq!(weapon_swap.recharge_rate, 1.0);
    }

    #[test]
    fn weapon_swap_is_unavailable_in_the_original_readers_excluded_state() {
        let character = 0x10_000;
        let combatant = 0x20_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.u32(character + 0xD8, 2);
        memory.u32(combatant + 0x28C, 1_000);
        memory.u32(combatant + 0x290, 8_000);

        assert!(read_skillbar(&memory, character, 1_500).is_none());
    }

    #[test]
    fn ability_uses_static_and_dynamic_ammunition_cooldowns() {
        let character = 0x10_000;
        let combatant = 0x20_000;
        let definition = 0x30_000;
        let cooldown_obj = 0x35_000;
        let cooldowns = 0x40_000;
        let ammo_node = 0x50_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.pointer(combatant + COMBATANT_SKILLDEF_BASE, definition);
        memory.u32(definition + SKILLDEF_ID, 54_321);
        memory.pointer(definition + SKILLDEF_COOLDOWN, cooldown_obj);
        memory.u32(cooldown_obj + SKILLDEF_MAX_AMMO, 3);

        memory.pointer(combatant + COMBATANT_COOLDOWNS, cooldowns);
        memory.f32(cooldowns + COOLDOWN_RATE, 1.0);
        memory.pointer(cooldowns + COOLDOWN_ROOTS[0], ammo_node);
        memory.pointer(ammo_node + COOLDOWN_NODE_SKILLDEF, definition);
        memory.u32(ammo_node + COOLDOWN_NODE_REMAINING, 1_500);
        memory.u32(ammo_node + COOLDOWN_NODE_TOTAL, 3_000);
        memory.u32(ammo_node + 0x24, 1);
        memory.u32(ammo_node + 0x28, 2);

        let skillbar = read_skillbar(&memory, character, 2_000).expect("skillbar");
        let ability = skillbar.bar.slot(Slot::Heal).expect("heal ability");

        assert_eq!(ability.ammo, 2);
        assert_eq!(ability.ammo_recharge, 3_000);
        assert_eq!(ability.ammo_recharge_remaining, 1_500);
    }

    #[test]
    fn active_cooldowns_include_skills_not_on_the_bar() {
        let character = 0x10_000;
        let combatant = 0x20_000;
        let heal_def = 0x30_000;
        let heal_cd_obj = 0x35_000;
        let off_def = 0x40_000;
        let off_cd_obj = 0x45_000;
        let cooldowns = 0x50_000;
        let node1 = 0x60_000;
        let node2 = 0x70_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        // Heal slot skill (raw 0) is on-bar.
        memory.pointer(combatant + COMBATANT_SKILLDEF_BASE, heal_def);
        memory.u32(heal_def + SKILLDEF_ID, 111);
        memory.pointer(heal_def + SKILLDEF_COOLDOWN, heal_cd_obj);
        // Off-bar skill is not referenced by any slot (e.g. the inactive weapon
        // set).
        memory.u32(off_def + SKILLDEF_ID, 222);
        memory.pointer(off_def + SKILLDEF_COOLDOWN, off_cd_obj);

        memory.pointer(combatant + COMBATANT_COOLDOWNS, cooldowns);
        memory.f32(cooldowns + COOLDOWN_RATE, 1.0);
        memory.pointer(cooldowns + COOLDOWN_ROOTS[0], node1);
        memory.pointer(node1 + COOLDOWN_NODE_SKILLDEF, heal_def);
        memory.u32(node1 + COOLDOWN_NODE_REMAINING, 4_000);
        memory.u32(node1 + COOLDOWN_NODE_TOTAL, 6_000);
        memory.pointer(node1 + 0x40, node2);
        memory.pointer(node2 + COOLDOWN_NODE_SKILLDEF, off_def);
        memory.u32(node2 + COOLDOWN_NODE_REMAINING, 2_000);
        memory.u32(node2 + COOLDOWN_NODE_TOTAL, 8_000);

        let snapshot = read_skillbar(&memory, character, 10_000).expect("skillbar");

        let heal_ability = snapshot.bar.slot(Slot::Heal).expect("heal ability");
        assert_eq!(heal_ability.id, SkillId::Id(111));
        assert_eq!(heal_ability.recharge_remaining(10_000), 4_000);

        // Both cooldowns are published; the off-bar one carries no slot.
        let has_heal = snapshot
            .active
            .iter()
            .any(|cd| cd.skill == SkillId::Id(111) && cd.slot == Some(Slot::Heal));
        let has_off = snapshot
            .active
            .iter()
            .any(|cd| cd.skill == SkillId::Id(222) && cd.slot == None);
        assert!(has_heal, "on-bar cooldown should be tagged with its slot");
        assert!(has_off, "off-bar cooldown should be published with no slot");
    }

    #[test]
    fn pet_skills_fill_the_profession_slots_from_the_original_raw_slots() {
        // A pet / mech combatant indexes its skills differently from a player
        // combatant: the original reader maps pet slot 6 to Profession 1, slot 11
        // to Profession 2 and slot 7 to Profession 3, and publishes no fourth
        // pet skill. Getting this wrong showed the mech's skills in the wrong
        // F1-F3 slots.
        let character = 0x10_000;
        let combatant = 0x20_000;
        let kennel = 0x30_000;
        let pet = 0x40_000;
        let pet_combatant = 0x50_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.pointer(character + CHARACTER_KENNEL, kennel);
        memory.pointer(kennel + KENNEL_PRIMARY_PET, pet);
        memory.pointer(pet + CHARACTER_COMBATANT, pet_combatant);
        // Slot 5 is a decoy: it must not reach any profession slot.
        for (raw, id) in [(5, 111_111), (6, 63_121), (7, 63_236), (11, 63_334)] {
            let definition = 0x60_000 + raw * 0x100;
            memory.pointer(
                pet_combatant + COMBATANT_SKILLDEF_BASE + raw * 8,
                definition,
            );
            memory.u32(definition + SKILLDEF_ID, id);
        }

        let snapshot = read_skillbar(&memory, character, 2_000).expect("skillbar");
        let bar = &snapshot.bar;
        let id = |slot| bar.slot(slot).map(|ability| ability.id);

        assert_eq!(id(Slot::Profession1), Some(SkillId::Id(63_121)));
        assert_eq!(id(Slot::Profession2), Some(SkillId::Id(63_334)));
        assert_eq!(id(Slot::Profession3), Some(SkillId::Id(63_236)));
        assert_eq!(id(Slot::Profession4), None);
        assert_eq!(id(Slot::Profession5), None);
    }

    #[test]
    fn pet_skills_do_not_replace_local_profession_skills() {
        // The player's own profession slot always wins over the pet fallback,
        // matching the original's "only fill an empty slot" gate.
        let character = 0x10_000;
        let combatant = 0x20_000;
        let local_definition = 0x28_000;
        let kennel = 0x30_000;
        let pet = 0x40_000;
        let pet_combatant = 0x50_000;
        let pet_definition = 0x60_000 + 6 * 0x100;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.pointer(character + CHARACTER_KENNEL, kennel);
        memory.pointer(kennel + KENNEL_PRIMARY_PET, pet);
        memory.pointer(pet + CHARACTER_COMBATANT, pet_combatant);
        // Local Profession 1 (raw 12) skill.
        memory.pointer(
            combatant + COMBATANT_SKILLDEF_BASE + 12 * 8,
            local_definition,
        );
        memory.u32(local_definition + SKILLDEF_ID, 7_777);
        // Pet Profession 1 slot.
        memory.pointer(
            pet_combatant + COMBATANT_SKILLDEF_BASE + 6 * 8,
            pet_definition,
        );
        memory.u32(pet_definition + SKILLDEF_ID, 63_121);

        let snapshot = read_skillbar(&memory, character, 2_000).expect("skillbar");
        let ability = snapshot
            .bar
            .slot(Slot::Profession1)
            .expect("local profession skill");

        assert_eq!(ability.id, SkillId::Id(7_777));
    }

    #[test]
    fn ability_prefers_the_node_cooldown_over_the_definition_base() {
        // The node holds the cooldown the game actually started, including any
        // cooldown reduction. The skill definition only carries the unmodified
        // base, so taking the wider of the two reported 20 s for Mechanist mech
        // commands that really cool down for 16 s.
        let character = 0x10_000;
        let combatant = 0x20_000;
        let definition = 0x30_000;
        let cooldown_obj = 0x35_000;
        let cooldowns = 0x40_000;
        let node = 0x50_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.pointer(combatant + COMBATANT_SKILLDEF_BASE, definition);
        memory.u32(definition + SKILLDEF_ID, 77_002);
        memory.pointer(definition + SKILLDEF_COOLDOWN, cooldown_obj);
        memory.u32(cooldown_obj + SKILLDEF_COOLDOWN_TOTAL, 20_000);
        memory.pointer(combatant + COMBATANT_COOLDOWNS, cooldowns);
        memory.f32(cooldowns + COOLDOWN_RATE, 1.0);
        memory.pointer(cooldowns + COOLDOWN_ROOTS[0], node);
        memory.pointer(node + COOLDOWN_NODE_SKILLDEF, definition);
        memory.u32(node + COOLDOWN_NODE_REMAINING, 16_000);
        memory.u32(node + COOLDOWN_NODE_TOTAL, 16_000);

        let snapshot = read_skillbar(&memory, character, 1_000).expect("skillbar");
        let ability = snapshot.bar.slot(Slot::Heal).expect("heal ability");

        assert_eq!(ability.recharge, 16_000);
        assert_eq!(ability.recharge_remaining(1_000), 16_000);
    }

    #[test]
    fn ready_ability_falls_back_to_the_definition_cooldown() {
        // Without a live node the definition still supplies the cooldown length,
        // so a ready skill keeps a usable total for the duration bar.
        let definition = 0x30_000;
        let cooldown_obj = 0x35_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(definition + SKILLDEF_COOLDOWN, cooldown_obj);
        memory.u32(cooldown_obj + SKILLDEF_COOLDOWN_TOTAL, 20_000);

        let ability = build_ability(
            &memory,
            0x20_000,
            0,
            54_321,
            definition,
            2_000,
            &BTreeMap::new(),
        );

        assert_eq!(ability.recharge, 20_000);
        assert_eq!(ability.recharge_remaining, 0);
    }

    #[test]
    fn active_cooldown_publishes_the_snapshot_anchor() {
        // Cooldown nodes are only rewritten coarsely and the reader polls every
        // 100 ms. Re-anchoring the published `last_update` on every read made the
        // countdown jump back to the snapshot value each poll (a visible
        // sawtooth) and kept the projected end time - and therefore the duration
        // bar and percentage - frozen. The anchor must stay put instead.
        // A unique skill id keeps this independent of the other cooldown tests
        // sharing the module-level track.
        let bar = Skillbar::empty();
        let mut cooldowns = BTreeMap::new();
        cooldowns.insert(
            999,
            Cooldown {
                recharge: Some(Recharge {
                    remaining: 8_000,
                    total: 10_000,
                    rate: 1.0,
                    anchored_at: 0,
                }),
                ammo: None,
            },
        );

        let first = build_active_cooldowns(&bar, &cooldowns, 0);
        assert_eq!(first[0].last_update, 0);
        assert_eq!(first[0].recharge_remaining, 8_000);
        assert_eq!(first[0].recharge_remaining(0), 8_000);

        // Same snapshot 2s later: the anchor stays, the projection ticks down.
        let second = build_active_cooldowns(&bar, &cooldowns, 2_000);
        assert_eq!(second[0].last_update, 0);
        assert_eq!(second[0].recharge_remaining, 8_000);
        assert_eq!(second[0].recharge_remaining(2_000), 6_000);
    }

    #[test]
    fn cooldown_anchor_projects_through_rewrite_noise() {
        let mut anchor = CooldownAnchor::default();

        // First observation anchors the snapshot.
        anchor_cooldown(&mut anchor, 8_000, 10_000, 1.0, 1_000, false);
        assert_eq!((anchor.remaining, anchor.anchored_at), (8_000, 1_000));

        // Node rewrite noise within the margin must not move the anchor: the
        // projection keeps ticking down instead of jumping back to the snapshot.
        anchor_cooldown(&mut anchor, 7_950, 10_000, 1.0, 1_050, false);
        assert_eq!((anchor.remaining, anchor.anchored_at), (8_000, 1_000));
        let projected = anchor.remaining - (1_050 - anchor.anchored_at);
        assert_eq!(projected, 7_950);

        // A node that keeps reporting its initial value for the whole cooldown
        // (the coarse rewrite of an off-bar cooldown) must never re-anchor
        // either, not even once the projection is close to running out.
        for now in [2_000, 5_000, 9_000] {
            anchor_cooldown(&mut anchor, 7_950, 10_000, 1.0, now, false);
            assert_eq!((anchor.remaining, anchor.anchored_at), (8_000, 1_000));
        }
    }

    #[test]
    fn cooldown_anchor_follows_a_fresh_cast() {
        let mut anchor = CooldownAnchor::default();
        anchor_cooldown(&mut anchor, 8_000, 10_000, 1.0, 1_000, false);

        // The node grew: a fresh cast, even when our projection is still running.
        anchor_cooldown(&mut anchor, 10_000, 10_000, 1.0, 1_100, false);
        assert_eq!((anchor.remaining, anchor.anchored_at), (10_000, 1_100));

        // A node that was missing for a pass and returns with the same static
        // value starts a new cooldown as well.
        anchor_cooldown(&mut anchor, 10_000, 10_000, 1.0, 2_000, false);
        assert_eq!((anchor.remaining, anchor.anchored_at), (10_000, 1_100));
        anchor_cooldown(&mut anchor, 10_000, 10_000, 1.0, 2_100, true);
        assert_eq!((anchor.remaining, anchor.anchored_at), (10_000, 2_100));
    }

    #[test]
    fn cooldown_anchor_adopts_a_cooldown_reduction() {
        let mut anchor = CooldownAnchor::default();
        anchor_cooldown(&mut anchor, 8_000, 10_000, 1.0, 1_000, false);

        // The game reports clearly less remaining than projected: a cooldown
        // reduction, adopted as the new anchor.
        anchor_cooldown(&mut anchor, 5_000, 10_000, 1.0, 1_100, false);
        assert_eq!((anchor.remaining, anchor.anchored_at), (5_000, 1_100));
    }

    #[test]
    fn cooldown_anchor_restarts_after_the_node_was_gone() {
        // A node that disappears for longer than the gap and comes back with the
        // same static value is a new cooldown, not a continuation of the old one.
        let cooldown = Cooldown {
            recharge: Some(Recharge {
                remaining: 5_000,
                total: 5_000,
                rate: 1.0,
                anchored_at: 0,
            }),
            ammo: None,
        };
        let anchored = |id: u32| {
            cooldown_tracks()
                .lock()
                .unwrap()
                .get(&id)
                .map(|track| (track.recharge.remaining, track.recharge.anchored_at))
        };

        let mut cooldowns = BTreeMap::new();
        cooldowns.insert(88_001, cooldown);
        anchor_cooldowns(&mut cooldowns, 1_000);
        assert_eq!(anchored(88_001), Some((5_000, 1_000)));

        // One poll later the same snapshot continues the anchored cooldown.
        anchor_cooldowns(&mut cooldowns, 1_100);
        assert_eq!(anchored(88_001), Some((5_000, 1_000)));

        // Longer than the gap later it is a new cooldown, anchored from now.
        anchor_cooldowns(&mut cooldowns, 1_700);
        assert_eq!(anchored(88_001), Some((5_000, 1_700)));
    }

    #[test]
    fn ability_countdown_projects_from_the_cooldown_anchor() {
        let character = 0x10_000;
        let combatant = 0x20_000;
        let definition = 0x30_000;
        let cooldown_obj = 0x35_000;
        let cooldowns = 0x40_000;
        let node = 0x50_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.pointer(combatant + COMBATANT_SKILLDEF_BASE, definition);
        memory.u32(definition + SKILLDEF_ID, 77_001);
        memory.pointer(definition + SKILLDEF_COOLDOWN, cooldown_obj);
        memory.pointer(combatant + COMBATANT_COOLDOWNS, cooldowns);
        memory.f32(cooldowns + COOLDOWN_RATE, 1.0);
        memory.pointer(cooldowns + COOLDOWN_ROOTS[0], node);
        memory.pointer(node + COOLDOWN_NODE_SKILLDEF, definition);
        memory.u32(node + COOLDOWN_NODE_REMAINING, 8_000);
        memory.u32(node + COOLDOWN_NODE_TOTAL, 10_000);

        let first = read_skillbar(&memory, character, 1_000).expect("skillbar");
        let ability = first.bar.slot(Slot::Heal).expect("heal ability");
        assert_eq!(ability.last_update, 1_000);
        assert_eq!(ability.recharge_remaining, 8_000);
        assert_eq!(ability.recharge_remaining(1_000), 8_000);

        // One poll later the node reports a slightly lower value: the published
        // anchor stays, so the countdown keeps ticking down from it instead of
        // jumping back to the node value every poll.
        memory.u32(node + COOLDOWN_NODE_REMAINING, 7_900);
        let second = read_skillbar(&memory, character, 1_100).expect("skillbar");
        let ability = second.bar.slot(Slot::Heal).expect("heal ability");
        assert_eq!(ability.last_update, 1_000);
        assert_eq!(ability.recharge_remaining(1_100), 7_900);
    }

    #[test]
    fn static_max_ammo_reads_the_charge_count_from_the_cooldown_object() {
        // The max-ammo count lives on the cooldown sub-object at
        // `definition + 0x60`, offset `+0xF0` (SKILLDEF_MAX_AMMO). Live-verified
        // 2026-09-24: weapon skill 3 (id 5694) stores 2 there; single-charge
        // skills store 0 (treated as one); a missing sub-object also means one.
        let two_charge = 0x30_000;
        let two_charge_obj = 0x31_000;
        let single = 0x40_000;
        let single_obj = 0x41_000;
        let no_object = 0x50_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(two_charge + SKILLDEF_COOLDOWN, two_charge_obj);
        memory.u32(two_charge_obj + SKILLDEF_MAX_AMMO, 2);
        memory.pointer(single + SKILLDEF_COOLDOWN, single_obj);
        memory.u32(single_obj + SKILLDEF_MAX_AMMO, 0);

        assert_eq!(static_max_ammo(&memory, two_charge), 2);
        assert_eq!(static_max_ammo(&memory, single), 1);
        assert_eq!(static_max_ammo(&memory, no_object), 1);
    }

    #[test]
    fn ability_marks_the_skilldef_ground_target_flag() {
        let definition = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.u32(definition + SKILLDEF_FLAGS, SKILLDEF_GROUND_TARGETED);

        let ability = build_ability(
            &memory,
            0x20_000,
            0,
            54_321,
            definition,
            2_000,
            &BTreeMap::new(),
        );

        assert!(ability.info.contains(AbilityInfo::GroundTarget));
    }

    #[test]
    fn ability_tracks_combatant_auto_pending_and_pressed_slots() {
        let character = 0x10_000;
        let combatant = 0x20_000;
        let definition = 0x30_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_COMBATANT, combatant);
        memory.pointer(combatant + COMBATANT_SKILLDEF_BASE, definition);
        memory.u32(definition + SKILLDEF_ID, 54_321);
        memory.u32(combatant + 0x88, 0);
        memory.u32(combatant + 0x128, 1);
        memory.u32(combatant + 0x74, 0);

        let skillbar = read_skillbar(&memory, character, 2_000).expect("skillbar");
        let ability = skillbar.bar.slot(Slot::Heal).expect("heal ability");

        assert!(ability.info.contains(AbilityInfo::AutoAttack));
        assert!(ability.info.contains(AbilityInfo::Pending));
        assert!(ability.info.contains(AbilityInfo::Pressed));
    }

    #[test]
    fn ranger_build_publishes_the_two_selected_pet_ids() {
        let character = 0x10_000;
        let holder = 0x20_000;
        let slot = 0x30_000;
        let pet_selection = 0x40_000;
        let first_pet = 0x50_000;
        let second_pet = 0x60_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_BUILD_HOLDER, holder);
        memory.pointer(holder + BUILD_HOLDER_VIEW, slot + BUILD_VIEW_SLOT_BIAS);
        memory.u32(slot + BUILD_ACTIVE_INDEX, 0);
        for (offset, id) in BUILD_SPEC_POINTERS.into_iter().zip([5, 27, 55]) {
            let definition = 0x70_000 + offset;
            memory.pointer(slot + offset, definition);
            memory.u32(definition + SKILLDEF_ID, id);
        }
        memory.u32(slot + 0xC0, 4);
        memory.pointer(slot + 0xC8, pet_selection);
        memory.u32(pet_selection + 0x08, 4);
        memory.pointer(pet_selection + 0x18, first_pet);
        memory.pointer(pet_selection + 0x20, second_pet);
        memory.u32(first_pet + SKILLDEF_ID, 5_102);
        memory.u32(second_pet + SKILLDEF_ID, 5_103);

        let (build, _slot) = read_build(&memory, character).expect("ranger build");

        assert_eq!(build.pets, [5_102, 5_103]);
    }

    #[test]
    fn build_falls_back_to_the_core_stats_view_when_the_primary_holder_is_absent() {
        let character = 0x10_000;
        let core = 0x20_000;
        let holder = 0x30_000;
        let slot = 0x40_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + CHARACTER_CORE_STATS, core);
        memory.pointer(core + 0xF0, holder);
        memory.pointer(holder + 0x78, slot + 0x30);
        memory.u32(slot + BUILD_ACTIVE_INDEX, 0);
        for (offset, id) in BUILD_SPEC_POINTERS.into_iter().zip([5, 27, 55]) {
            let definition = 0x50_000 + offset;
            memory.pointer(slot + offset, definition);
            memory.u32(definition + SKILLDEF_ID, id);
        }

        let (build, _slot) = read_build(&memory, character).expect("core build fallback");

        assert_eq!(build.specs, [5, 27, 55]);
    }

    #[test]
    fn intensity_buffs_merge_but_duration_buffs_do_not() {
        let mut buffs = BuffMap::new();
        insert_buff(&mut buffs, 740, 10_000, 100);
        insert_buff(&mut buffs, 740, 8_000, 200);
        insert_buff(&mut buffs, 717, 10_000, 100);
        insert_buff(&mut buffs, 717, 8_000, 200);
        assert_eq!(buffs[&740].stacks, 2);
        assert_eq!(buffs[&717].stacks, 1);
        assert_eq!(buffs[&717].runout_time, 10_100);
    }

    #[test]
    fn weapon_mapping_preserves_hand_specific_flags() {
        assert_eq!(map_weapon(4, false), Some(Weapon::AxeMain));
        assert_eq!(map_weapon(4, true), Some(Weapon::AxeOff));
        assert_eq!(map_weapon(1, true), Some(Weapon::Hammer));
        assert_eq!(map_weapon(9, false), None);
    }

    #[test]
    fn sanity_bounds_reject_layout_noise() {
        assert!(sane_health(100.0, 100.0));
        assert!(!sane_health(f32::NAN, 100.0));
        assert!(sane_cooldown_total(250));
        assert!(!sane_cooldown_total(249));
        assert!(!sane_skill_id(LOCKED_SKILL_ID));
    }

    #[test]
    fn build_selections_use_only_configured_skill_slots() {
        let mut skillbar = Skillbar::empty();
        skillbar.set_slot(Slot::Weapon1, Some(Ability::simple(111, 0, 0)));
        skillbar.set_slot(Slot::Heal, Some(Ability::simple(222, 0, 0)));
        skillbar.set_slot(Slot::Utility1, Some(Ability::simple(333, 0, 0)));
        skillbar.set_slot(Slot::Utility2, Some(Ability::simple(444, 0, 0)));
        skillbar.set_slot(Slot::Utility3, Some(Ability::simple(555, 0, 0)));
        skillbar.set_slot(Slot::Elite, Some(Ability::simple(666, 0, 0)));

        let (skills, _) = build_runtime_selections(Some(1), &skillbar, &[], &[]);

        assert_eq!(skills, vec![222, 333, 444, 555, 666]);
    }

    #[test]
    fn revenant_skill_sets_produce_legend_selection_flags() {
        let mut skillbar = Skillbar::empty();
        skillbar.set_slot(Slot::Utility1, Some(Ability::simple(28_134, 0, 0)));
        skillbar.set_slot(Slot::Elite, Some(Ability::simple(45_773, 0, 0)));

        let (_, selections) = build_runtime_selections(Some(9), &skillbar, &[], &[]);

        assert!(selections.contains(ProfSelection::LegendAssassin));
        assert!(selections.contains(ProfSelection::LegendRenegade));
        assert!(!selections.contains(ProfSelection::LegendCentaur));
    }

    #[test]
    fn elementalist_familiar_is_a_single_selection_from_the_selection_skill_ids() {
        // The active familiar comes from the build's profession-selection skill
        // ids, not the equipped skillbar. 77225→Water(Otter), 77226→Earth(Toad),
        // 77370→Air(Hare); anything else present defaults to Fire(Fox). Only one
        // familiar is ever selected (fixes the old multi-flag over-listing).
        let mut skillbar = Skillbar::empty();
        skillbar.set_slot(Slot::Heal, Some(Ability::simple(5_503, 0, 0)));
        skillbar.set_slot(Slot::Utility1, Some(Ability::simple(76_711, 0, 0)));

        let (skills, water) = build_runtime_selections(Some(6), &skillbar, &[], &[77_225]);
        assert_eq!(skills, vec![5_503, 76_711, 77_225]);
        assert!(water.contains(ProfSelection::FamiliarWater));
        assert!(!water.contains(ProfSelection::FamiliarFire));
        assert!(!water.contains(ProfSelection::FamiliarEarth));

        let (_, earth) = build_runtime_selections(Some(6), &skillbar, &[], &[77_226]);
        assert!(earth.contains(ProfSelection::FamiliarEarth));
        assert_eq!(earth.iter().count(), 1);

        // A selection list with no familiar id defaults to Fire.
        let (_, fire) = build_runtime_selections(Some(6), &skillbar, &[], &[12_345]);
        assert!(fire.contains(ProfSelection::FamiliarFire));
        assert_eq!(fire.iter().count(), 1);

        // No selection ids at all (non-Evoker elementalist) → no familiar.
        let (_, none) = build_runtime_selections(Some(6), &skillbar, &[], &[]);
        assert!(none.is_empty());
    }

    #[test]
    fn resolve_selected_skill_takes_the_equipped_variant_over_a_transform() {
        // A build-slot skill container holds a 0x70-strided variant table at
        // +0x60. Variant 0 is a live transform (its +0x08 link word is set) and
        // must be skipped; variant 1 is the equipped skill (link words zeroed,
        // slotted-skill flag present). Mirrors the live Evoker case where the
        // runtime skillbar shows 76563 but the equipped utility is 77190.
        let container = 0x40_000;
        let transform = 0x41_000;
        let equipped = 0x42_000;
        let mut memory = LegacyMixedTreeMemory::default();

        // variant[0] @ +0x60: transformed → skipped via the non-zero link word.
        memory.pointer(container + SKILL_VARIANT_BASE, transform);
        memory.u32(transform + SKILLDEF_ID, 76_563);
        memory.u32(transform + SKILLDEF_FLAGS, SKILLDEF_EQUIPPED_FLAG);
        memory.pointer(container + SKILL_VARIANT_BASE + 0x08, 0x99_000);
        memory.pointer(container + SKILL_VARIANT_BASE + 0x30, 0);

        // variant[1] @ +0x60 + 0x70: equipped → both link words zeroed.
        let slot1 = SKILL_VARIANT_BASE + SKILL_VARIANT_STRIDE;
        memory.pointer(container + slot1, equipped);
        memory.u32(equipped + SKILLDEF_ID, 77_190);
        memory.u32(equipped + SKILLDEF_FLAGS, SKILLDEF_EQUIPPED_FLAG);
        memory.pointer(container + slot1 + 0x08, 0);
        memory.pointer(container + slot1 + 0x30, 0);

        assert_eq!(resolve_selected_skill(&memory, container), Some(77_190));
    }

    #[test]
    fn read_selected_skills_only_reads_evoker_builds() {
        let slot = 0x10_000;
        let memory = LegacyMixedTreeMemory::default();
        // Non-Evoker specs → no build-slot skill resolution (keeps the skillbar).
        assert!(read_selected_skills(&memory, slot, &[5, 27, 55]).is_empty());
    }

    #[test]
    fn companion_address_must_be_valid_and_distinct_from_its_owner() {
        assert!(is_owned_companion_address(0x10000, 0x20000));
        assert!(!is_owned_companion_address(0x10000, 0));
        assert!(!is_owned_companion_address(0x10000, 0x10000));
    }

    #[test]
    fn gear_snapshot_publishes_runtime_effect_ids() {
        let snapshot = GearSnapshot {
            weapons: BitFlags::from_flag(Weapon::Rifle),
            sigils: [101, 102, 104, 105],
            relic: 103,
        };

        let gear = snapshot.resolve();

        assert_eq!(gear.sigils, [101, 102, 104, 105]);
        assert_eq!(gear.relic, 103);
        assert!(gear.weapons.contains(Weapon::Rifle));
    }

    #[test]
    fn reads_runtime_equipped_effect_ids_without_catalog_metadata() {
        let skillbar = 0x10_000;
        let weapon_entry = 0x20_000;
        let first_foreign = 0x30_000;
        let first_payload = 0x40_000;
        let first_wrapper = 0x50_000;
        let first_effect = 0x60_000;
        let second_foreign = 0x70_000;
        let second_payload = 0x80_000;
        let second_wrapper = 0x90_000;
        let second_effect = 0xA0_000;
        let relic_entry = 0xB0_000;
        let relic_foreign = 0xC0_000;
        let relic_payload = 0xD0_000;
        let relic_effect = 0xE0_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(skillbar + SKILLBAR_SLOT_BASE + 29 * 8, weapon_entry);
        memory.pointer(weapon_entry + 0xA0 + 0x28, first_foreign);
        memory.u32(first_foreign + 0x2C, 0x17);
        memory.pointer(first_foreign + 0x30, first_payload);
        memory.pointer(first_payload + 0x10, first_wrapper);
        memory.pointer(first_wrapper + 0x50, first_effect);
        memory.u32(first_effect + 0x28, 21_824);

        memory.pointer(weapon_entry + 0xA0 + 0x30, second_foreign);
        memory.u32(second_foreign + 0x2C, 0x17);
        memory.pointer(second_foreign + 0x30, second_payload);
        memory.pointer(second_payload + 0x10, second_wrapper);
        memory.pointer(second_wrapper + 0x50, second_effect);
        memory.u32(second_effect + 0x28, 9_452);

        memory.pointer(skillbar + 0x378, relic_entry);
        memory.pointer(relic_entry + 0x40, relic_foreign);
        memory.u32(relic_foreign + 0x2C, 0x12);
        memory.pointer(relic_foreign + 0x30, relic_payload);
        memory.pointer(relic_payload, relic_effect);
        memory.u32(relic_effect + 0x28, 74_494);

        assert_eq!(
            read_equipped_effects(&memory, skillbar),
            ([21_824, 9_452, 0, 0], 74_494)
        );
    }

    #[test]
    fn permanent_buffs_require_a_definition_pointer() {
        // A permanent effect is published only when its wrapper carries a
        // definition pointer (a real applied buff). Equipment/internal permanent
        // effects leave it null and must be dropped — live-verified against the
        // original's "Player buffs" list (ids 739/77060 kept, relic/sigil not).
        let character = 0x10_000;
        let container = 0x20_000;
        let buff_node = 0x30_000;
        let equip_node = 0x40_000;
        let buff_wrapper = 0x50_000;
        let equip_wrapper = 0x60_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + EFFECT_TREE_CONTAINER, container);
        memory.u32(container + EFFECT_TREE_NODE_OFFSET, 0x58);
        memory.pointer(container + EFFECT_TREE_ROOT, buff_node);

        // Real applied buff: permanent, with a definition pointer.
        memory.pointer(buff_node + EFFECT_NODE_WRAPPER, buff_wrapper);
        memory.u32(buff_wrapper + EFFECT_WRAPPER_STATE, 1);
        memory.u32(buff_wrapper + EFFECT_WRAPPER_ID, 739);
        memory.pointer(buff_wrapper + EFFECT_WRAPPER_VALUE, buff_wrapper + 0xD8);
        memory.pointer(
            buff_wrapper + EFFECT_WRAPPER_DEFINITION,
            buff_wrapper + 0x100,
        );
        memory.u32(buff_node + EFFECT_NODE_DURATION, u32::MAX);
        memory.pointer(buff_node + 0x60, equip_node);

        // Equipment effect: permanent, no definition pointer -> dropped.
        memory.pointer(equip_node + EFFECT_NODE_WRAPPER, equip_wrapper);
        memory.u32(equip_wrapper + EFFECT_WRAPPER_STATE, 1);
        memory.u32(equip_wrapper + EFFECT_WRAPPER_ID, 70_047);
        memory.pointer(equip_wrapper + EFFECT_WRAPPER_VALUE, equip_wrapper + 0xD8);
        memory.u32(equip_node + EFFECT_NODE_DURATION, u32::MAX);
        memory.pointer(equip_node + 0x60, 1);

        let buffs = read_buffs(&memory, character).expect("validated effect tree");

        assert!(buffs.contains_key(&739));
        assert!(!buffs.contains_key(&70_047));
    }

    #[test]
    fn very_long_duration_effects_are_published_as_permanent_buffs() {
        // Live-verified (2026-09-23): effect 9283 carries a ~6.6h duration
        // (23_760_000 ms) and a definition pointer; the original lists it as a
        // plain "Effect". The reader must publish it (as a hidden-duration buff)
        // rather than dropping it for exceeding the timed-buff window.
        let character = 0x10_000;
        let container = 0x20_000;
        let node = 0x30_000;
        let wrapper = 0x40_000;
        let mut memory = LegacyMixedTreeMemory::default();

        memory.pointer(character + EFFECT_TREE_CONTAINER, container);
        memory.u32(container + EFFECT_TREE_NODE_OFFSET, 0x58);
        memory.pointer(container + EFFECT_TREE_ROOT, node);
        memory.pointer(node + EFFECT_NODE_WRAPPER, wrapper);
        memory.u32(wrapper + EFFECT_WRAPPER_STATE, 1);
        memory.u32(wrapper + EFFECT_WRAPPER_ID, 9283);
        memory.pointer(wrapper + EFFECT_WRAPPER_VALUE, wrapper + 0xD8);
        memory.pointer(wrapper + EFFECT_WRAPPER_DEFINITION, wrapper + 0x100);
        memory.u32(node + EFFECT_NODE_DURATION, 23_760_000);
        memory.u32(node + EFFECT_NODE_APPLIED_AT, 100);
        memory.pointer(node + 0x60, 1);

        let buffs = read_buffs(&memory, character).expect("validated effect tree");

        let buff = buffs.get(&9283).expect("long-duration effect is published");
        assert!(buff.is_infinite());
    }

    #[test]
    fn find_all_reports_every_occurrence() {
        assert_eq!(find_all(b"ababab", b"ab"), vec![0, 2, 4]);
        assert_eq!(find_all(b"abc", b"z"), Vec::<usize>::new());
        assert_eq!(find_all(b"abc", b""), Vec::<usize>::new());
        assert_eq!(find_all(b"ab", b"abc"), Vec::<usize>::new());
    }

    #[test]
    fn name_needles_encodes_both_representations() {
        let [ascii, utf16] = name_needles("鹏霄");

        assert_eq!(ascii.0, "ascii");
        assert_eq!(ascii.1, "鹏霄".as_bytes());
        assert_eq!(utf16.0, "utf16");
        assert_eq!(utf16.1, vec![0x4f, 0x9e, 0x04, 0x97]);
    }

    #[test]
    fn probe_string_reads_narrow_names_and_read_text_reads_wide_ones() {
        let mut memory = LegacyMixedTreeMemory::default();
        memory.bytes(0x1000, b"Veteran Flame Legion Charr\0");
        let wide: Vec<u8> = "Champion Karka"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .chain([0, 0])
            .collect();
        memory.bytes(0x2000, &wide);

        assert_eq!(
            read_probe_string(&memory, 0x1000).as_deref(),
            Some("Veteran Flame Legion Charr")
        );
        // The scanning reader stays ASCII only, so random heap bytes that decode
        // to valid CJK code units cannot drown the real matches.
        assert_eq!(read_probe_string(&memory, 0x2000), None);
        assert_eq!(
            read_text(&memory, 0x2000, true).as_deref(),
            Some("Champion Karka")
        );
    }

    #[test]
    fn probe_string_rejects_values_that_are_not_names() {
        let mut memory = LegacyMixedTreeMemory::default();
        // Unmapped pointer target.
        assert_eq!(read_probe_string(&memory, 0x3000), None);

        // Too short to be a name.
        memory.bytes(0x4000, b"ab\0");
        assert_eq!(read_probe_string(&memory, 0x4000), None);

        // Control bytes, not text.
        memory.bytes(0x5000, &[0x01, 0x02, 0x03, 0x04, 0, 0]);
        assert_eq!(read_probe_string(&memory, 0x5000), None);

        // No terminator inside the read window.
        memory.bytes(0x6000, &[b'a'; 200]);
        assert_eq!(read_probe_string(&memory, 0x6000), None);
    }

    #[test]
    fn target_classification_is_fail_closed_until_offsets_are_recovered() {
        let local = 0x10_000;
        let member = 0x20_000;
        let stranger = 0x30_000;
        let member_table = 0x40_000;
        let entries = 0x50_000;
        let member_entry = 0x60_000;
        let mut memory = LegacyMixedTreeMemory::default();

        store_identity(&mut memory, member + CHARACTER_GROUP_IDENTITY, 1, 2);
        memory.pointer(local + CHARACTER_GROUP_MEMBERS, member_table);
        memory.pointer(member_table + GROUP_MEMBER_ENTRIES, entries);
        memory.u32(member_table + GROUP_MEMBER_COUNT, 1);
        memory.pointer(entries, member_entry);
        memory.pointer(member_entry + GROUP_MEMBER_CHARACTER, member);

        // A character that is a current group member is an allied player: this
        // needs no new client offset, so it is classified today.
        let (affinity, category) = read_target_classification(&memory, local, 0, Some(member));
        assert!(matches!(affinity, Ok(Affinity::Friendly)));
        assert!(matches!(
            category,
            Ok(flags) if flags == make_bitflags!(TargetCategory::{Player | Friendly})
        ));

        // Any other character stays unknown rather than being guessed at, so a
        // classification trigger cannot fire on the wrong target.
        let (affinity, category) = read_target_classification(&memory, local, 0, Some(stranger));
        assert!(matches!(affinity, Err(Error::Affinity)));
        assert!(matches!(category, Err(Error::TargetState)));

        // Kind 0 without a character pointer is unknown too.
        let (_, category) = read_target_classification(&memory, local, 0, None);
        assert!(matches!(category, Err(Error::TargetState)));

        // Kinds other than 0 stay unknown. An earlier revision mapped kinds 10
        // and 11 to `Gadget`, but the first probe capture showed they cover
        // ordinary targets too, so the tag is not mapped until it is understood.
        for kind in [10, 11, 7] {
            let (_, category) = read_target_classification(&memory, local, kind, None);
            assert!(matches!(category, Err(Error::TargetState)));
        }

        // A group member is only recognised through the kind 0 character path.
        let (_, category) = read_target_classification(&memory, local, 10, Some(member));
        assert!(matches!(category, Err(Error::TargetState)));
    }
}
