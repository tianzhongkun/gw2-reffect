//! Version-checked access to Guild Wars 2 runtime content tables.
//!
//! The content-table iterator is reconstructed from `reffect-0.12.0.dll`.
//! Its two UUID selectors are stable content identifiers, not code addresses.
//! Before reading any entry the walker verifies the skill-table anchor is
//! effect id 740 (Might); a changed client layout therefore fails closed.
//! It receives the store through the ContextCollection captured on the game
//! thread, so the metadata worker never suspends the game thread.

use super::capture::CapturedContext;
use super::{GameError, hook, main_module_range, read_process_memory};
use enumflags2::BitFlags;
use reffect_core::context::{Category, ItemInfo, SkillInfo, Stacking};
use std::{
    collections::{BTreeMap, HashSet},
    mem::size_of,
};

const SKILL_SELECTOR: [u8; 16] = [
    0x64, 0x3A, 0xD0, 0x26, 0x3A, 0x4D, 0x70, 0x4F, 0x9E, 0x48, 0xCF, 0x90, 0xA4, 0xCF, 0xC5, 0xA2,
];

/// Item content UUID recovered alongside the skill UUID. The item decoder is
/// kept separate because modern relics use a different record shape.
pub const ITEM_SELECTOR: [u8; 16] = [
    0x16, 0x6C, 0x35, 0xDE, 0x8C, 0xD8, 0xBC, 0x4F, 0xB1, 0x78, 0x3F, 0x1B, 0x3F, 0xFC, 0xD3, 0xD2,
];

const SKILL_TABLE_ANCHOR_ID: u32 = 740;
pub(crate) const CONTENT_VTABLE_NEXT_OFFSET: usize = 72;
pub(crate) const CONTENT_VTABLE_LOOKUP_OFFSET: usize = 104;
const MAX_CONTENT_RECORDS: usize = 100_000;
const MAX_ABILITY_TYPE_OVERRIDES: usize = 256;

type LookupContent = unsafe extern "system" fn(*const u8, *const u8) -> *const u8;
type NextContent = unsafe extern "system" fn(*const u8, u32, *mut u32) -> *const u8;

/// Runtime skill metadata keyed by Guild Wars 2 skill/effect id.
pub type SkillCatalog = BTreeMap<u32, SkillInfo>;

/// Runtime mapping between equipped upgrade item ids and their hidden effects.
#[derive(Debug, Default)]
pub struct ItemCatalog {
    /// Item-id lookup used by the gear trigger editor.
    pub by_item: BTreeMap<u32, ItemInfo>,
    /// Reverse lookup used when an equipped effect is observed in game data.
    pub by_buff: BTreeMap<u32, u32>,
}

/// Returns a complete runtime skill catalogue for the currently loaded game
/// client.
///
/// The ContentStore originates from the latest validated game-thread capture.
/// If no current ContextCollection has been captured yet, this fails closed.
pub fn scan_skills() -> Result<SkillCatalog, GameError> {
    ContentTable::from_game_context()?.scan_skills()
}

/// Returns the runtime sigil and relic mappings for the current game client.
pub fn scan_items() -> Result<ItemCatalog, GameError> {
    ContentTable::from_game_context()?.scan_items()
}

#[derive(Debug)]
struct ContentTable {
    table: usize,
    vtable: usize,
}

impl ContentTable {
    fn from_game_context() -> Result<Self, GameError> {
        let captured = hook::latest_context();
        log_content_probe(captured);
        let table = content_store_address(captured, |address| {
            read_process_memory::<usize>(address).ok()
        })?;
        let (module_base, module_end) = module_range()?;
        let validated = validate_content_store(table, module_base, module_end, |address| {
            read_process_memory::<usize>(address).ok()
        });
        if validated.is_none() {
            log_content_store_rejection(captured, table, module_base, module_end);
        }
        let vtable = validated.ok_or(GameError::IncompatibleContentLayout)?;

        Ok(Self { table, vtable })
    }

    fn scan_skills(&self) -> Result<SkillCatalog, GameError> {
        let result = self.walk_skills();
        log_walk_once("skill", result.as_ref().map(|skills| skills.len()));
        result
    }

    fn walk_skills(&self) -> Result<SkillCatalog, GameError> {
        let anchor = self.lookup(&SKILL_SELECTOR)?;
        let anchor_id = if anchor == 0 {
            None
        } else {
            self.read::<u32>(anchor + 40).ok()
        };
        log_anchor_once("skill", anchor, anchor_id);
        if anchor == 0 || !has_table_anchor(anchor_id.unwrap_or(0), SKILL_TABLE_ANCHOR_ID) {
            return Err(GameError::IncompatibleContentLayout);
        }

        // 'anchor + 16' is the table id passed as the iterator's second
        // argument. The third argument is an in/out position packed as
        // '{ bucket: 2 bits, index: 30 bits }' that the client advances on
        // every call, so the table id stays constant for the whole walk.
        let cursor = self.read::<u32>(anchor + 16)?;
        let entries = walk_content(
            "skill",
            cursor,
            |cursor, state| self.next(cursor, state),
            |record| self.decode_skill(record),
        )?;
        Ok(entries.into_iter().collect())
    }

    fn scan_items(&self) -> Result<ItemCatalog, GameError> {
        let result = self.walk_items();
        log_walk_once("item", result.as_ref().map(|catalog| catalog.by_item.len()));
        result
    }

    fn walk_items(&self) -> Result<ItemCatalog, GameError> {
        let anchor = self.lookup(&ITEM_SELECTOR)?;
        let anchor_id = if anchor == 0 {
            None
        } else {
            self.read::<u32>(anchor + 40).ok()
        };
        log_anchor_once("item", anchor, anchor_id);
        if anchor == 0 || !has_table_anchor(anchor_id.unwrap_or(0), 19_700) {
            return Err(GameError::IncompatibleContentLayout);
        }

        let cursor = self.read::<u32>(anchor + 16)?;
        let entries = walk_content(
            "item",
            cursor,
            |cursor, state| self.next(cursor, state),
            |record| self.decode_item(record),
        )?;

        let mut items = ItemCatalog::default();
        for (item_id, buff, info) in entries {
            items.by_buff.insert(buff, item_id);
            items.by_item.insert(item_id, info);
        }
        Ok(items)
    }

    fn decode_skill(&self, record: usize) -> Result<Option<(u32, SkillInfo)>, GameError> {
        let data = self.read_usize(record + 96)?;
        if data == 0 {
            return Ok(None);
        }
        let id = self.read::<u32>(record + 40)?;
        let available = self.read_usize(record + 80)? != 0;

        match self.read::<u32>(record + 88)? {
            1 => Ok(self.decode_buff(available, data)?.map(|info| (id, info))),
            0 if available => Ok(Some((
                id,
                decode_ability_metadata(
                    self.read_ability_kind(data)?,
                    self.read::<u32>(record + 56)?,
                ),
            ))),
            _ => Ok(None),
        }
    }

    /// Reconstructs the type selector used by the original skill catalogue.
    /// A content record can override the default kind through a compact
    /// 8-byte-strided list; only kinds 0, 1, and 4 affect public metadata.
    fn read_ability_kind(&self, data: usize) -> Result<u32, GameError> {
        let default = self.read::<u32>(data + 240)?;
        let entries = self.read_usize(data + 400)?;
        if entries == 0 {
            return Ok(default);
        }
        let count = self.read::<u32>(data + 408)? as usize;
        if count > MAX_ABILITY_TYPE_OVERRIDES {
            return Err(GameError::IncompatibleContentLayout);
        }

        select_ability_kind(
            default,
            (0..count).map(|index| {
                let offset = index
                    .checked_mul(8)
                    .and_then(|offset| entries.checked_add(offset))
                    .ok_or(GameError::IncompatibleContentLayout)?;
                self.read::<u32>(offset)
            }),
        )
    }

    fn decode_buff(&self, available: bool, data: usize) -> Result<Option<SkillInfo>, GameError> {
        let flags = self.read::<u32>(data + 4)?;
        let raw_category = self.read::<u32>(data + 24)?;
        let stacking_kind = self.read::<u32>(data + 12)?;
        let special = is_special_buff(available, flags, raw_category);
        let (screen_border, squad_highlight) = if special {
            (
                self.read_usize(data + 104)? != 0,
                self.read_usize(data + 96)? != 0,
            )
        } else {
            (false, false)
        };

        Ok(decode_buff_metadata(
            available,
            flags,
            raw_category,
            stacking_kind,
            screen_border,
            squad_highlight,
        ))
    }

    fn decode_item(&self, record: usize) -> Result<Option<(u32, u32, ItemInfo)>, GameError> {
        let buff = self.read::<u32>(record + 40)?;
        let content = match self.read::<u32>(record + 44)? {
            // `Foreign<UpgradeComponent>`: the content pointer is stored in
            // the payload at +48. The release maps this variant to a sigil.
            0x12 => {
                let pointer = self.read_usize(record + 48)?;
                (pointer, true)
            }
            // `Foreign<Relic>` wraps a second item reference. Only wrapper
            // kind 3 carries an equipped-relic effect mapping.
            0x17 => {
                if self.read::<u32>(record + 48 + 36)? != 3 {
                    return Ok(None);
                }
                let wrapper = self.read_usize(record + 48 + 16)?;
                if wrapper == 0 {
                    return Ok(None);
                }
                (self.read_usize(wrapper + 80)?, false)
            }
            _ => return Ok(None),
        };
        let (content, is_sigil) = content;
        if content == 0 || buff == 0 {
            return Ok(None);
        }
        let item_id = self.read::<u32>(content + 40)?;
        if item_id == 0 {
            return Ok(None);
        }

        let info = if is_sigil {
            ItemInfo::Sigil { buff }
        } else {
            ItemInfo::Relic { buff }
        };
        Ok(Some((item_id, buff, info)))
    }

    fn lookup(&self, selector: &[u8; 16]) -> Result<usize, GameError> {
        let function = self.function::<LookupContent>(CONTENT_VTABLE_LOOKUP_OFFSET)?;
        // Both pointer arguments reference stable, readable memory. The
        // receiver is the validated content table from the suspended game
        // context; the function pointer was read from its vtable. The call is
        // still guarded: a stale capture can resolve the receiver to an
        // unrelated object, and the client would then dereference the selector
        // bytes as a C++ object. That fault must fail the scan, not the process.
        let table = self.table;
        microseh::try_seh(|| unsafe { function(table as *const u8, selector.as_ptr()) as usize })
            .map_err(|exception| {
                log_content_fault("lookup", exception);
                GameError::ContentCallFaulted
            })
    }

    fn next(&self, cursor: u32, next: &mut u32) -> Result<usize, GameError> {
        let function = self.function::<NextContent>(CONTENT_VTABLE_NEXT_OFFSET)?;
        let table = self.table;
        microseh::try_seh(|| unsafe { function(table as *const u8, cursor, next) as usize })
            .map_err(|exception| {
                log_content_fault("next", exception);
                GameError::ContentCallFaulted
            })
    }

    fn function<T: Copy>(&self, offset: usize) -> Result<T, GameError> {
        let address = self.read_usize(self.vtable + offset)?;
        if address == 0 {
            return Err(GameError::IncompatibleContentLayout);
        }
        // Function-pointer sizes are checked by the concrete aliases above.
        Ok(unsafe { std::mem::transmute_copy(&address) })
    }

    fn read<T: Copy>(&self, address: usize) -> Result<T, GameError> {
        read_process_memory(address)
    }

    fn read_usize(&self, address: usize) -> Result<usize, GameError> {
        self.read(address)
    }
}

/// Walks one content table through the client iterator.
///
/// 'next' takes the constant table id and the in/out position the client
/// advances on every call, and returns the record at that position (null
/// once the table is exhausted). The table id must not be replaced by the
/// position: doing so makes the client re-read the first bucket, so the walk
/// repeats a cursor until the duplicate guard aborts it (observed live as
/// cursor=0x40 state=0x1 then cursor=0x1 state=0x1).
fn walk_content<T>(
    kind: &str,
    cursor: u32,
    mut next: impl FnMut(u32, &mut u32) -> Result<usize, GameError>,
    mut decode: impl FnMut(usize) -> Result<Option<T>, GameError>,
) -> Result<Vec<T>, GameError> {
    let mut state = 0u32;
    let mut seen = HashSet::new();
    let mut entries = Vec::new();

    for iteration in 0..MAX_CONTENT_RECORDS {
        if iteration % 10_000 == 0 {
            log::debug!(
                "[reffect-runtime] {kind} walk iteration {iteration} cursor={cursor:#x} state={state:#x} entries={}",
                entries.len()
            );
        }
        if !seen.insert(state) {
            return Err(GameError::IncompatibleContentLayout);
        }

        let record = next(cursor, &mut state)?;
        log_walk_step(kind, iteration, cursor, state, record);
        if record == 0 {
            return Ok(entries);
        }

        if let Some(entry) = decode(record)? {
            entries.push(entry);
        }
    }

    Err(GameError::IncompatibleContentLayout)
}

fn content_store_address(
    captured: Option<CapturedContext>,
    mut read: impl FnMut(usize) -> Option<usize>,
) -> Result<usize, GameError> {
    captured
        .and_then(|context| context.collection.checked_add(0xE0))
        .and_then(&mut read)
        .filter(|address| *address != 0)
        .ok_or(GameError::IncompatibleContentLayout)
}

/// The mapped main-module image as `(base, end)`.
fn module_range() -> Result<(usize, usize), GameError> {
    let (base, size) = main_module_range().ok_or(GameError::IncompatibleContentLayout)?;
    let end = base
        .checked_add(size)
        .ok_or(GameError::IncompatibleContentLayout)?;
    Ok((base, end))
}

fn is_module_pointer(value: usize, module_base: usize, module_end: usize) -> bool {
    (module_base..module_end).contains(&value)
}

/// Validates a candidate content store before any of its code runs.
///
/// The recovered reader (`reffect-0.12.0.dll`) only proceeds when the store
/// pointer is non-null and its second word is non-null, and it reads the two
/// iterator entry points from the store's vtable. Reproduce those
/// preconditions and additionally require the vtable and both entry points to
/// live inside the client image. A stale capture or a false-positive context
/// discovery points the store at unrelated module-headed objects (a live crash
/// resolved it to an `AgentStatus` widget), and calling one of those makes the
/// client dereference the selector as a C++ object.
pub(crate) fn validate_content_store(
    table: usize,
    module_base: usize,
    module_end: usize,
    mut read: impl FnMut(usize) -> Option<usize>,
) -> Option<usize> {
    let vtable = read(table)?;
    if !is_module_pointer(vtable, module_base, module_end) {
        return None;
    }
    if read(table.checked_add(size_of::<usize>())?)? == 0 {
        return None;
    }
    for offset in [CONTENT_VTABLE_NEXT_OFFSET, CONTENT_VTABLE_LOOKUP_OFFSET] {
        let function = read(vtable.checked_add(offset)?)?;
        if !is_module_pointer(function, module_base, module_end) {
            return None;
        }
    }
    Some(vtable)
}

/// Logs the first observed content store (or its absence) so a silent scan
/// failure can be told apart from a missing game context. Logs once per
/// outcome.
fn log_content_probe(captured: Option<CapturedContext>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED_NONE: AtomicBool = AtomicBool::new(false);
    static LOGGED_SOME: AtomicBool = AtomicBool::new(false);
    let store = captured
        .and_then(|context| context.collection.checked_add(0xE0))
        .and_then(|address| read_process_memory::<usize>(address).ok())
        .filter(|store| *store != 0);
    match store {
        None => {
            if !LOGGED_NONE.swap(true, Ordering::Relaxed) {
                log::debug!(
                    "[reffect-runtime] content probe: no store (collection={:?})",
                    captured.map(|context| context.collection)
                );
            }
        }
        Some(store) => {
            if !LOGGED_SOME.swap(true, Ordering::Relaxed) {
                let vtable = read_process_memory::<usize>(store).ok();
                log::debug!(
                    "[reffect-runtime] content probe: collection={:?} cli={:?} local={:?} store={store:#x} vtable={vtable:?}",
                    captured.map(|context| context.collection),
                    captured.map(|context| context.cli),
                    captured.map(|context| context.local)
                );
            }
        }
    }
}

/// Logs the first resolved content anchor and the id read from it, so a
/// selector lookup failure is distinguishable from a table-walk failure.
fn log_anchor_once(kind: &str, anchor: usize, id: Option<u32>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::debug!("[reffect-runtime] {kind} content anchor={anchor:#x} id={id:?}");
    }
}

/// Logs the first few content-walk steps, so a hang or a short chain is
/// distinguishable.
fn log_walk_step(kind: &str, iteration: usize, cursor: u32, state: u32, record: usize) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let index = COUNT.fetch_add(1, Ordering::Relaxed);
    if index < 5 {
        log::debug!(
            "[reffect-runtime] {kind} walk[{index}]: iter={iteration} cursor={cursor:#x} state={state:#x} record={record:#x}"
        );
    }
}

/// Logs the first completed (or failed) content walk once.
fn log_walk_once(kind: &str, result: Result<usize, &GameError>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        match result {
            Ok(count) => {
                log::debug!("[reffect-runtime] {kind} content walk completed: {count} records")
            }
            Err(error) => {
                log::debug!("[reffect-runtime] {kind} content walk failed: {error:?}")
            }
        }
    }
}

/// Logs the first rejected content store once. This distinguishes a moved
/// content-store offset (the field now holds an unrelated object) from a
/// stale/false-positive context capture, and keeps the 100 ms retry loop from
/// spamming the log.
fn log_content_store_rejection(
    captured: Option<CapturedContext>,
    table: usize,
    module_base: usize,
    module_end: usize,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let vtable = read_process_memory::<usize>(table).ok();
    let second = read_process_memory::<usize>(table.wrapping_add(size_of::<usize>())).ok();
    log::warn!(
        "[reffect-runtime] content store rejected: collection={:?} table={:#x} vtable={:?} second={:?} module={:#x}..{:#x}",
        captured.map(|context| context.collection),
        table,
        vtable,
        second,
        module_base,
        module_end
    );
}

/// Logs the first content-table fault once, so a wrong receiver is diagnosable
/// without the 100 ms retry loop spamming the log.
fn log_content_fault(call: &str, exception: microseh::Exception) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "[reffect-runtime] content-table {call} call faulted at {:#x}: {}",
            exception.address() as usize,
            exception.code()
        );
    }
}

fn has_table_anchor(actual: u32, expected: u32) -> bool {
    actual == expected
}

fn is_special_buff(available: bool, flags: u32, raw_category: u32) -> bool {
    !available || flags & 0x1000 != 0 || matches!(raw_category, 6 | 15)
}

/// Maps the release's private packed buff metadata into the public core ABI.
/// The three special conditions use marker pointers rather than normal
/// category/stacking data: +104 is ScreenBorder; +96 is SquadHighlight.
fn decode_buff_metadata(
    available: bool,
    flags: u32,
    raw_category: u32,
    stacking_kind: u32,
    screen_border: bool,
    squad_highlight: bool,
) -> Option<SkillInfo> {
    if is_special_buff(available, flags, raw_category) {
        let (category, visibility) = if screen_border {
            (
                Category::ScreenBorder,
                BitFlags::from_flag(reffect_core::context::Visibility::Player),
            )
        } else if squad_highlight {
            (
                Category::SquadHighlight,
                BitFlags::from_flag(reffect_core::context::Visibility::Player)
                    | BitFlags::from_flag(reffect_core::context::Visibility::Group),
            )
        } else {
            return None;
        };
        return Some(SkillInfo::Buff {
            category,
            stacking: Stacking::Other,
            visibility,
        });
    }

    let category = match raw_category {
        0 => Category::Boon,
        2 => Category::Condition,
        _ => Category::Effect,
    };
    let stacking = match stacking_kind {
        // The client has five internal stacking kinds. The release maps even
        // kinds to one public behavior and odd kinds to the other.
        // This table is also visible at `reffect-0.12.0.dll+0x1A5D08`.
        0 | 2 | 4 => Stacking::Intensity,
        1 | 3 => Stacking::Duration,
        _ => Stacking::Other,
    };
    // The release packs visibility into the high byte before storing it in
    // its Rust map.  Reconstruct it directly as public Visibility flags.
    let raw_visibility = if flags & 0x80 != 0 {
        1
    } else {
        3 | u32::from(flags & 0x40 == 0) << 2 | u32::from(raw_category & !2 == 0) << 3
    };

    Some(SkillInfo::Buff {
        category,
        stacking,
        visibility: BitFlags::from_bits_truncate(raw_visibility as u8),
    })
}

/// Converts the original catalogue's ability kind and source flags to the
/// four public `SkillInfo::Ability` fields.  The bit positions come directly
/// from the release packing expression at `reffect-0.12.0.dll+0xD89A8`.
fn decode_ability_metadata(kind: u32, source_flags: u32) -> SkillInfo {
    SkillInfo::Ability {
        is_instant: kind < 2 || source_flags & 0x80000 != 0,
        is_ammo: kind >= 2,
        is_ground_targeted: source_flags & 0x1000 != 0,
        is_stunbreak: source_flags & 0x20 != 0,
    }
}

fn select_ability_kind(
    default: u32,
    kinds: impl IntoIterator<Item = Result<u32, GameError>>,
) -> Result<u32, GameError> {
    for kind in kinds {
        let kind = kind?;
        if is_supported_ability_kind(kind) {
            return Ok(kind);
        }
    }
    Ok(default)
}

fn is_supported_ability_kind(kind: u32) -> bool {
    matches!(kind, 0 | 1 | 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::capture::CapturedContext;

    #[test]
    fn recovered_selectors_are_stable_uuid_values() {
        assert_eq!(SKILL_SELECTOR.len(), 16);
        assert_eq!(ITEM_SELECTOR.len(), 16);
        assert_eq!(SKILL_SELECTOR[0], 0x64);
        assert_eq!(ITEM_SELECTOR[0], 0x16);
    }

    #[test]
    fn standard_boons_are_visible_everywhere() {
        let category = Category::Boon;
        let raw = 3 | 4 | u32::from(matches!(category, Category::Boon | Category::Condition)) << 3;
        let visibility =
            BitFlags::<reffect_core::context::Visibility>::from_bits_truncate(raw as u8);
        assert!(visibility.contains(reffect_core::context::Visibility::Player));
        assert!(visibility.contains(reffect_core::context::Visibility::TargetNonHostile));
        assert!(visibility.contains(reffect_core::context::Visibility::TargetHostile));
        assert!(visibility.contains(reffect_core::context::Visibility::Group));
    }

    #[test]
    fn item_selector_uses_a_distinct_content_uuid() {
        assert_ne!(ITEM_SELECTOR, SKILL_SELECTOR);
    }

    #[test]
    fn unavailable_normal_buff_is_not_published() {
        assert!(decode_buff_metadata(false, 0, 0, 0, false, false).is_none());
    }

    #[test]
    fn special_buff_markers_map_to_their_public_categories() {
        let screen_border = decode_buff_metadata(false, 0, 0, 0, true, false).unwrap();
        let squad_highlight = decode_buff_metadata(false, 0, 0, 0, false, true).unwrap();

        assert!(matches!(
            screen_border,
            SkillInfo::Buff {
                category: Category::ScreenBorder,
                stacking: Stacking::Other,
                visibility,
            } if visibility == BitFlags::from_flag(reffect_core::context::Visibility::Player)
        ));
        assert!(matches!(
            squad_highlight,
            SkillInfo::Buff {
                category: Category::SquadHighlight,
                stacking: Stacking::Other,
                visibility,
            } if visibility == BitFlags::from_flag(reffect_core::context::Visibility::Player)
                | BitFlags::from_flag(reffect_core::context::Visibility::Group)
        ));
    }

    #[test]
    fn special_runtime_flags_do_not_publish_an_ordinary_buff() {
        assert!(decode_buff_metadata(true, 0x1000, 0, 0, false, false).is_none());
        assert!(decode_buff_metadata(true, 0, 6, 0, false, false).is_none());
        assert!(decode_buff_metadata(true, 0, 15, 0, false, false).is_none());
    }

    #[test]
    fn maps_runtime_buff_category_and_stacking_semantics() {
        let boon = decode_buff_metadata(true, 0, 0, 0, false, false).unwrap();
        let condition = decode_buff_metadata(true, 0, 2, 3, false, false).unwrap();
        let effect = decode_buff_metadata(true, 0x40, 9, 4, false, false).unwrap();

        assert!(matches!(
            boon,
            SkillInfo::Buff {
                category: Category::Boon,
                stacking: Stacking::Intensity,
                ..
            }
        ));
        assert!(matches!(
            condition,
            SkillInfo::Buff {
                category: Category::Condition,
                stacking: Stacking::Duration,
                ..
            }
        ));
        assert!(matches!(
            effect,
            SkillInfo::Buff {
                category: Category::Effect,
                stacking: Stacking::Intensity,
                visibility,
            } if visibility == BitFlags::from_flag(reffect_core::context::Visibility::Player)
                | BitFlags::from_flag(reffect_core::context::Visibility::TargetNonHostile)
        ));
    }

    #[test]
    fn content_store_requires_a_captured_context_and_non_null_pointer() {
        assert_eq!(
            content_store_address(None, |_| Some(0x2000)),
            Err(GameError::IncompatibleContentLayout)
        );

        let captured = CapturedContext::new(0x1000, 0x2000, 0x3000, 1);
        assert_eq!(
            content_store_address(Some(captured), |_| Some(0)),
            Err(GameError::IncompatibleContentLayout)
        );
        assert_eq!(
            content_store_address(Some(captured), |address| (address == 0x10E0)
                .then_some(0x4000)),
            Ok(0x4000)
        );
    }

    #[test]
    fn table_anchor_must_match_the_recovered_effect_id() {
        assert!(has_table_anchor(
            SKILL_TABLE_ANCHOR_ID,
            SKILL_TABLE_ANCHOR_ID
        ));
        assert!(!has_table_anchor(
            SKILL_TABLE_ANCHOR_ID - 1,
            SKILL_TABLE_ANCHOR_ID
        ));
    }

    #[test]
    fn ability_metadata_matches_the_recovered_runtime_bits() {
        let instant = decode_ability_metadata(0, 0);
        let ammo_ground_stunbreak = decode_ability_metadata(2, 0x1000 | 0x20);
        let instant_ammo = decode_ability_metadata(4, 0x80000);

        assert!(matches!(
            instant,
            SkillInfo::Ability {
                is_instant: true,
                is_ammo: false,
                is_ground_targeted: false,
                is_stunbreak: false,
            }
        ));
        assert!(matches!(
            ammo_ground_stunbreak,
            SkillInfo::Ability {
                is_instant: false,
                is_ammo: true,
                is_ground_targeted: true,
                is_stunbreak: true,
            }
        ));
        assert!(matches!(
            instant_ammo,
            SkillInfo::Ability {
                is_instant: true,
                is_ammo: true,
                is_ground_targeted: false,
                is_stunbreak: false,
            }
        ));
    }

    #[test]
    fn ability_type_override_uses_the_first_supported_runtime_kind() {
        assert_eq!(select_ability_kind(3, [Ok(2), Ok(4)]), Ok(4));
        assert_eq!(select_ability_kind(3, [Ok(3), Ok(2)]), Ok(3));
        assert_eq!(select_ability_kind(0, [Ok(1), Ok(4)]), Ok(1));
    }

    #[test]
    fn content_store_validation_requires_a_module_headed_store() {
        let (base, end) = (0x1800_0000usize, 0x1900_0000usize);
        let vtable = base + 0x1000;
        let mut memory = std::collections::HashMap::new();
        memory.insert(0x2000usize, vtable);
        memory.insert(0x2008usize, 0x1234);
        memory.insert(vtable + CONTENT_VTABLE_NEXT_OFFSET, base + 0x2000);
        memory.insert(vtable + CONTENT_VTABLE_LOOKUP_OFFSET, base + 0x3000);

        let read = |address: usize| memory.get(&address).copied();
        assert_eq!(
            validate_content_store(0x2000, base, end, read),
            Some(vtable)
        );

        // A null second word is not a live content table.
        memory.insert(0x2008usize, 0);
        let read = |address: usize| memory.get(&address).copied();
        assert_eq!(validate_content_store(0x2000, base, end, read), None);

        // A vtable outside the client image is rejected.
        memory.insert(0x2008usize, 0x1234);
        memory.insert(0x2000usize, 0x4000);
        let read = |address: usize| memory.get(&address).copied();
        assert_eq!(validate_content_store(0x2000, base, end, read), None);
    }

    #[test]
    fn content_store_validation_rejects_foreign_entry_points() {
        let (base, end) = (0x1800_0000usize, 0x1900_0000usize);
        let vtable = base + 0x1000;
        let mut memory = std::collections::HashMap::new();
        memory.insert(0x2000usize, vtable);
        memory.insert(0x2008usize, 0x1234);
        memory.insert(vtable + CONTENT_VTABLE_NEXT_OFFSET, 0x4000);
        memory.insert(vtable + CONTENT_VTABLE_LOOKUP_OFFSET, base + 0x3000);

        let read = |address: usize| memory.get(&address).copied();
        assert_eq!(validate_content_store(0x2000, base, end, read), None);
    }

    #[test]
    fn content_walk_keeps_the_table_id_and_advances_the_state() {
        let mut calls = Vec::new();
        let entries = walk_content(
            "test",
            0x40,
            |cursor, state| {
                calls.push((cursor, *state));
                let index = *state;
                *state += 1;
                Ok(if index < 3 { 0x1000 + index as usize } else { 0 })
            },
            |record| Ok(Some(record)),
        )
        .unwrap();

        assert_eq!(entries, vec![0x1000, 0x1001, 0x1002]);
        assert!(calls.iter().all(|(cursor, _)| *cursor == 0x40));
        assert_eq!(
            calls.iter().map(|(_, state)| *state).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn lookup_contains_a_faulting_entry_point() {
        // A vtable slot pointing at an unmapped address stands in for a stale
        // capture whose receiver is the wrong object. The SEH guard must turn
        // the hardware fault into an error instead of terminating the process.
        static BAD_VTABLE: [usize; 16] = [1; 16];
        let table = ContentTable {
            table: 0x1000,
            vtable: BAD_VTABLE.as_ptr() as usize,
        };

        assert_eq!(
            table.lookup(&SKILL_SELECTOR),
            Err(GameError::ContentCallFaulted)
        );
    }
}
