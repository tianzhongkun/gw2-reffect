//! Native, unload-safe capture hook for the game's TLS context accessor.

use super::{
    capture::{CaptureState, CapturedContext, resolve_context_tree},
    read_process_bytes, read_process_memory,
};
use std::{
    ffi::c_void,
    mem,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};

const CONTEXT_GETTER_LEN: usize = 29;
// GW2 moved the TLS context getter in the 2026-10-02 client update
// (0x9B80E0 -> 0x9B7A40, confirmed by locating the getter signature in the new
// Gw2-64.exe). The module-image scan in `find_context_getter`/`
// discover_context_tls_index` finds it regardless, so this RVA is only the
// fast path, but it must stay current so the fast path is correct.
const CONTEXT_GETTER_RVA: usize = 0x9B7A40;
const CONTEXT_GETTER_PATCH_LEN: usize = 6;
const DOS_HEADER_E_LFANEW_OFFSET: usize = 0x3C;
const PE_OPTIONAL_HEADER_SIZE_OF_IMAGE_OFFSET: usize = 0x50;
const MODULE_SCAN_CHUNK: usize = 0x1_0000;
const ABSOLUTE_JUMP_LEN: usize = 14;
const RELATIVE_JUMP_LEN: usize = 5;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_RELEASE: u32 = 0x8000;
const MAX_CAPTURE_AGE_MS: u32 = 1_000;
const CAPTURE_INTERVAL_MS: u32 = 100;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn FlushInstructionCache(process: isize, base_address: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> isize;
    fn GetModuleHandleW(module_name: *const u16) -> isize;
    fn GetTickCount() -> u32;
    fn Sleep(milliseconds: u32);
    fn VirtualAlloc(
        address: *const c_void,
        size: usize,
        allocation_type: u32,
        protect: u32,
    ) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    fn VirtualProtect(
        address: *mut c_void,
        size: usize,
        new_protect: u32,
        old_protect: *mut u32,
    ) -> i32;
}

type ContextGetter = unsafe extern "system" fn() -> usize;

static CAPTURE_STATE: CaptureState = CaptureState::new();
static ORIGINAL_GETTER: AtomicUsize = AtomicUsize::new(0);
static HOOK_ENABLED: AtomicBool = AtomicBool::new(false);
static ACTIVE_CALLBACKS: AtomicU64 = AtomicU64::new(0);
static HOOK_HITS: AtomicU64 = AtomicU64::new(0);
static GETTER_CALLS: AtomicU64 = AtomicU64::new(0);
static CONTEXT_GETTER_REJECTIONS: AtomicU64 = AtomicU64::new(0);
static CONTEXT_TREE_REJECTIONS: AtomicU64 = AtomicU64::new(0);
static CAPTURE_COUNT: AtomicU64 = AtomicU64::new(0);
static LAST_CAPTURE_TICK: AtomicU32 = AtomicU32::new(u32::MAX - (CAPTURE_INTERVAL_MS - 1));
static CAPTURE: OnceLock<Mutex<Option<ContextGetterCapture>>> = OnceLock::new();

/// A low-frequency diagnostic snapshot of the game-thread accessor pipeline.
///
/// These counters distinguish an inactive detour from a valid callback whose
/// TLS getter or ContextCollection tree could not be decoded. They are only
/// surfaced when the worker has remained unavailable for a full second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureTelemetry {
    hook_hits: u64,
    getter_calls: u64,
    getter_rejections: u64,
    tree_rejections: u64,
    captures: u64,
}

impl CaptureTelemetry {
    #[must_use]
    pub(crate) const fn new(
        hook_hits: u64,
        getter_calls: u64,
        getter_rejections: u64,
        tree_rejections: u64,
        captures: u64,
    ) -> Self {
        Self {
            hook_hits,
            getter_calls,
            getter_rejections,
            tree_rejections,
            captures,
        }
    }

    #[must_use]
    pub(crate) const fn status(self) -> CaptureStatus {
        if self.captures != 0 {
            CaptureStatus::Captured
        } else if self.hook_hits == 0 {
            CaptureStatus::NoHookCallbacks
        } else if self.getter_calls == 0 {
            CaptureStatus::NoContextGetterCalls
        } else if self.getter_rejections != 0 {
            CaptureStatus::ContextGetterRejected
        } else if self.tree_rejections != 0 {
            CaptureStatus::ContextTreeRejected
        } else {
            CaptureStatus::AwaitingCapture
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureStatus {
    NoHookCallbacks,
    NoContextGetterCalls,
    ContextGetterRejected,
    ContextTreeRejected,
    AwaitingCapture,
    Captured,
}

/// Result of validating and publishing one ContextCollection returned by the
/// game's TLS accessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureOutcome {
    Captured,
    InvalidCollection,
    InvalidTree,
}

/// Validates the ContextCollection returned by the game-thread TLS accessor and
/// publishes its stable pointer tree.
pub(crate) fn capture_collection(
    state: &CaptureState,
    captured_at: u32,
    collection: usize,
    read: impl FnMut(usize) -> Option<usize>,
) -> CaptureOutcome {
    if !is_user_pointer(collection) {
        return CaptureOutcome::InvalidCollection;
    }
    state.publish_collection(collection, captured_at);
    let Some((cli, local)) = resolve_context_tree(collection, read) else {
        return CaptureOutcome::InvalidTree;
    };
    state.publish(CapturedContext::new(collection, cli, local, captured_at));
    CaptureOutcome::Captured
}

#[derive(Debug)]
struct ContextGetterCapture {
    target: usize,
    original: [u8; CONTEXT_GETTER_PATCH_LEN],
    trampoline: Option<usize>,
    relay: usize,
}

impl ContextGetterCapture {
    fn install() -> Result<Self, &'static str> {
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        if base == 0 {
            return Err("main module unavailable");
        }
        let target = find_context_getter(base).ok_or("ContextCollection signature missing")?;
        let bytes = read_process_bytes(target, CONTEXT_GETTER_LEN)
            .map_err(|_| "ContextCollection getter unreadable")?;
        let mut original = [0; CONTEXT_GETTER_PATCH_LEN];
        original.copy_from_slice(&bytes[..CONTEXT_GETTER_PATCH_LEN]);

        let (original_getter, trampoline) = match context_getter_target(target, &bytes) {
            Some(ContextGetterTarget::Clean) => {
                let trampoline = build_context_getter_trampoline(target, &original)?;
                (trampoline, Some(trampoline))
            }
            Some(ContextGetterTarget::Chained(destination)) => (destination, None),
            None => return Err("ContextCollection getter entry is not supported"),
        };
        let relay = match build_near_relay(target, context_getter_hook as *const () as usize) {
            Some(relay) => relay,
            None => {
                if let Some(trampoline) = trampoline {
                    unsafe { VirtualFree(trampoline as *mut c_void, 0, MEM_RELEASE) };
                }
                return Err("unable to allocate ContextCollection hook relay");
            }
        };
        let Some(patch) = context_getter_patch(target, relay) else {
            unsafe { VirtualFree(relay as *mut c_void, 0, MEM_RELEASE) };
            if let Some(trampoline) = trampoline {
                unsafe { VirtualFree(trampoline as *mut c_void, 0, MEM_RELEASE) };
            }
            return Err("ContextCollection hook relay is unreachable");
        };

        ORIGINAL_GETTER.store(original_getter, Ordering::Release);
        HOOK_ENABLED.store(true, Ordering::Release);
        if !patch_bytes(target, &patch) {
            HOOK_ENABLED.store(false, Ordering::Release);
            ORIGINAL_GETTER.store(0, Ordering::Release);
            unsafe { VirtualFree(relay as *mut c_void, 0, MEM_RELEASE) };
            if let Some(trampoline) = trampoline {
                unsafe { VirtualFree(trampoline as *mut c_void, 0, MEM_RELEASE) };
            }
            return Err("ContextCollection getter patch failed");
        }

        Ok(Self {
            target,
            original,
            trampoline,
            relay,
        })
    }
}

impl Drop for ContextGetterCapture {
    fn drop(&mut self) {
        HOOK_ENABLED.store(false, Ordering::Release);
        if !patch_bytes(self.target, &self.original) {
            log::error!("[reffect-runtime] failed to restore ContextCollection hook");
        }
        while ACTIVE_CALLBACKS.load(Ordering::Acquire) != 0 {
            unsafe { Sleep(1) };
        }
        ORIGINAL_GETTER.store(0, Ordering::Release);
        if let Some(trampoline) = self.trampoline {
            unsafe { VirtualFree(trampoline as *mut c_void, 0, MEM_RELEASE) };
        }
        unsafe { VirtualFree(self.relay as *mut c_void, 0, MEM_RELEASE) };
    }
}

pub(crate) fn init() {
    let mut slot = capture_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_some() {
        return;
    }
    match ContextGetterCapture::install() {
        Ok(capture) => {
            log::info!("[reffect-runtime] ContextCollection getter hook installed");
            *slot = Some(capture);
        }
        Err(error) => {
            log::warn!("[reffect-runtime] ContextCollection getter hook unavailable: {error}")
        }
    }
}

pub(crate) fn deinit() {
    let capture = capture_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    drop(capture);
}

#[must_use]
pub(crate) fn latest_context() -> Option<CapturedContext> {
    let now = unsafe { GetTickCount() };
    if let Some(context) = CAPTURE_STATE.latest_at(now, MAX_CAPTURE_AGE_MS) {
        return Some(context);
    }
    let collection = CAPTURE_STATE.latest_collection_at(now, MAX_CAPTURE_AGE_MS)?;
    let (cli, local) = resolve_context_tree(collection, |address| {
        read_process_memory::<usize>(address)
            .ok()
            .filter(|address| is_user_pointer(*address))
    })?;
    let context = CapturedContext::new(collection, cli, local, now);
    CAPTURE_STATE.publish(context);
    Some(context)
}

/// Resolves and publishes a ContextCollection discovered by the worker from
/// the game thread's TLS block. Unlike the legacy detour path, this never
/// executes on the game thread.
pub(crate) fn refresh_context_from_collection(collection: usize) -> Option<CapturedContext> {
    let now = unsafe { GetTickCount() };
    resolve_and_publish_context(&CAPTURE_STATE, now, collection, |address| {
        read_process_memory::<usize>(address)
            .ok()
            .filter(|address| is_user_pointer(*address))
    })
}

#[must_use]
pub(crate) fn capture_count() -> u64 {
    CAPTURE_COUNT.load(Ordering::Acquire)
}

#[must_use]
pub(crate) fn capture_telemetry() -> CaptureTelemetry {
    CaptureTelemetry::new(
        HOOK_HITS.load(Ordering::Acquire),
        GETTER_CALLS.load(Ordering::Acquire),
        CONTEXT_GETTER_REJECTIONS.load(Ordering::Acquire),
        CONTEXT_TREE_REJECTIONS.load(Ordering::Acquire),
        CAPTURE_COUNT.load(Ordering::Acquire),
    )
}

fn capture_slot() -> &'static Mutex<Option<ContextGetterCapture>> {
    CAPTURE.get_or_init(|| Mutex::new(None))
}

unsafe extern "system" fn context_getter_hook() -> usize {
    ACTIVE_CALLBACKS.fetch_add(1, Ordering::AcqRel);
    HOOK_HITS.fetch_add(1, Ordering::Relaxed);
    let original = ORIGINAL_GETTER.load(Ordering::Acquire);
    let collection = if original == 0 {
        0
    } else {
        unsafe { mem::transmute::<usize, ContextGetter>(original)() }
    };

    if HOOK_ENABLED.load(Ordering::Acquire) {
        capture_context_collection(collection);
    }
    ACTIVE_CALLBACKS.fetch_sub(1, Ordering::Release);
    collection
}

fn capture_context_collection(collection: usize) {
    let now = unsafe { GetTickCount() };
    if !claim_capture_slot(now) {
        return;
    }
    GETTER_CALLS.fetch_add(1, Ordering::Relaxed);
    match capture_collection(&CAPTURE_STATE, now, collection, |address| {
        read_process_memory::<usize>(address)
            .ok()
            .filter(|address| is_user_pointer(*address))
    }) {
        CaptureOutcome::Captured => {
            CAPTURE_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        CaptureOutcome::InvalidCollection => {
            CONTEXT_GETTER_REJECTIONS.fetch_add(1, Ordering::Relaxed);
        }
        CaptureOutcome::InvalidTree => {
            CONTEXT_TREE_REJECTIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) fn resolve_and_publish_context(
    state: &CaptureState,
    captured_at: u32,
    collection: usize,
    read: impl FnMut(usize) -> Option<usize>,
) -> Option<CapturedContext> {
    (capture_collection(state, captured_at, collection, read) == CaptureOutcome::Captured)
        .then(|| state.latest_at(captured_at, 0))
        .flatten()
}

fn claim_capture_slot(now: u32) -> bool {
    let previous = LAST_CAPTURE_TICK.load(Ordering::Relaxed);
    capture_is_due(previous, now)
        && LAST_CAPTURE_TICK
            .compare_exchange(previous, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
}

pub(super) fn capture_is_due(previous: u32, now: u32) -> bool {
    now.wrapping_sub(previous) >= CAPTURE_INTERVAL_MS
}

fn find_context_getter(base: usize) -> Option<usize> {
    let by_rva = base.checked_add(CONTEXT_GETTER_RVA)?;
    if read_process_bytes(by_rva, CONTEXT_GETTER_LEN)
        .ok()
        .is_some_and(|bytes| context_getter_target(by_rva, &bytes).is_some())
    {
        return Some(by_rva);
    }

    let image_size = module_image_size(base)?;
    let mut offset = 0usize;
    while offset < image_size {
        let remaining = image_size.checked_sub(offset)?;
        let length = remaining.min(MODULE_SCAN_CHUNK + CONTEXT_GETTER_LEN - 1);
        let address = base.checked_add(offset)?;
        let bytes = read_process_bytes(address, length).ok()?;
        if let Some(index) = bytes
            .windows(CONTEXT_GETTER_LEN)
            .position(|candidate| context_tls_index_address(address, candidate).is_some())
        {
            return address.checked_add(index);
        }
        offset = offset.checked_add(MODULE_SCAN_CHUNK)?;
    }
    None
}

fn module_image_size(base: usize) -> Option<usize> {
    let nt_header_offset =
        read_process_memory::<u32>(base.checked_add(DOS_HEADER_E_LFANEW_OFFSET)?).ok()? as usize;
    let image_size = read_process_memory::<u32>(
        base.checked_add(nt_header_offset)?
            .checked_add(PE_OPTIONAL_HEADER_SIZE_OF_IMAGE_OFFSET)?,
    )
    .ok()? as usize;
    (CONTEXT_GETTER_LEN..=0x4000_0000)
        .contains(&image_size)
        .then_some(image_size)
}

fn is_user_pointer(address: usize) -> bool {
    (0x1_0000..0x0000_8000_0000_0000).contains(&address)
}

/// Determines whether the current accessor is clean or already detoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextGetterTarget {
    Clean,
    Chained(usize),
}

pub(crate) fn context_getter_target(target: usize, bytes: &[u8]) -> Option<ContextGetterTarget> {
    if bytes.first() == Some(&0xE9) {
        return relative_destination(target, bytes).map(ContextGetterTarget::Chained);
    }
    if is_absolute_jump(bytes) {
        return absolute_destination(bytes).map(ContextGetterTarget::Chained);
    }
    context_tls_index_address(target, bytes).map(|_| ContextGetterTarget::Clean)
}

/// Builds the six-byte entry patch required to replace the complete
/// RIP-relative TLS-index load without touching the next instruction.
pub(crate) fn context_getter_patch(
    target: usize,
    relay: usize,
) -> Option<[u8; CONTEXT_GETTER_PATCH_LEN]> {
    let relative = relative_jump(target, relay)?;
    let mut patch = [0x90; CONTEXT_GETTER_PATCH_LEN];
    patch[..RELATIVE_JUMP_LEN].copy_from_slice(&relative);
    Some(patch)
}

fn is_absolute_jump(bytes: &[u8]) -> bool {
    bytes.len() >= ABSOLUTE_JUMP_LEN && bytes[..6] == [0xFF, 0x25, 0, 0, 0, 0]
}

fn absolute_destination(bytes: &[u8]) -> Option<usize> {
    let bytes: [u8; 8] = bytes.get(6..ABSOLUTE_JUMP_LEN)?.try_into().ok()?;
    usize::try_from(u64::from_le_bytes(bytes)).ok()
}

fn relative_destination(site: usize, bytes: &[u8]) -> Option<usize> {
    let displacement = i32::from_le_bytes(bytes.get(1..RELATIVE_JUMP_LEN)?.try_into().ok()?);
    site.checked_add(RELATIVE_JUMP_LEN)?
        .checked_add_signed(displacement as isize)
}

fn absolute_jump(destination: usize) -> [u8; ABSOLUTE_JUMP_LEN] {
    let mut bytes = [0; ABSOLUTE_JUMP_LEN];
    bytes[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    bytes[6..].copy_from_slice(&(destination as u64).to_le_bytes());
    bytes
}

fn build_context_getter_trampoline(target: usize, original: &[u8]) -> Result<usize, &'static str> {
    let trampoline = allocate_near(target).ok_or("unable to allocate trampoline")?;
    let mut tls_index_load = [0; CONTEXT_GETTER_PATCH_LEN];
    tls_index_load.copy_from_slice(&original[..CONTEXT_GETTER_PATCH_LEN]);
    if !relocate_tls_index_load(&mut tls_index_load, target, trampoline) {
        unsafe { VirtualFree(trampoline as *mut c_void, 0, MEM_RELEASE) };
        return Err("unable to relocate ContextCollection TLS index load");
    }
    let mut payload = [0; CONTEXT_GETTER_PATCH_LEN + ABSOLUTE_JUMP_LEN];
    payload[..CONTEXT_GETTER_PATCH_LEN].copy_from_slice(&tls_index_load);
    payload[CONTEXT_GETTER_PATCH_LEN..].copy_from_slice(&absolute_jump(
        target
            .checked_add(CONTEXT_GETTER_PATCH_LEN)
            .ok_or("trampoline return overflow")?,
    ));
    if !write_executable(trampoline, &payload) {
        unsafe { VirtualFree(trampoline as *mut c_void, 0, MEM_RELEASE) };
        return Err("unable to write trampoline");
    }
    Ok(trampoline)
}

fn build_near_relay(target: usize, destination: usize) -> Option<usize> {
    let relay = allocate_near(target)?;
    if !write_executable(relay, &absolute_jump(destination)) {
        unsafe { VirtualFree(relay as *mut c_void, 0, MEM_RELEASE) };
        return None;
    }
    if relative_jump(target, relay).is_none() {
        unsafe { VirtualFree(relay as *mut c_void, 0, MEM_RELEASE) };
        return None;
    }
    Some(relay)
}

fn allocate_near(target: usize) -> Option<usize> {
    const GRANULARITY: usize = 0x1_0000;
    const MAX_DISTANCE: usize = 0x7000_0000;
    for offset in (GRANULARITY..MAX_DISTANCE).step_by(GRANULARITY) {
        for candidate in [target.checked_add(offset), target.checked_sub(offset)] {
            let Some(candidate) = candidate else {
                continue;
            };
            let candidate = candidate & !(GRANULARITY - 1);
            let allocation = unsafe {
                VirtualAlloc(
                    candidate as *const c_void,
                    0x1000,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                )
            };
            if !allocation.is_null() {
                return Some(allocation as usize);
            }
        }
    }
    None
}

fn write_executable(address: usize, bytes: &[u8]) -> bool {
    let destination = address as *mut c_void;
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast(), bytes.len()) };
    (unsafe { FlushInstructionCache(GetCurrentProcess(), destination, bytes.len()) }) != 0
}

fn patch_bytes(address: usize, bytes: &[u8]) -> bool {
    let mut old_protect = 0u32;
    let destination = address as *mut c_void;
    if unsafe {
        VirtualProtect(
            destination,
            bytes.len(),
            PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    } == 0
    {
        return false;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast(), bytes.len()) };
    let flushed =
        (unsafe { FlushInstructionCache(GetCurrentProcess(), destination, bytes.len()) }) != 0;
    let mut ignored = 0u32;
    (unsafe { VirtualProtect(destination, bytes.len(), old_protect, &mut ignored) }) != 0 && flushed
}

/// Decodes the `mov ecx, [rip + disp32]` operand in ContextCollection's
/// current TLS getter.
pub(crate) fn context_tls_index_address(getter_address: usize, bytes: &[u8]) -> Option<usize> {
    let [0x8B, 0x0D, displacement @ ..] = bytes.get(..6)? else {
        return None;
    };
    if !matches!(
        bytes.get(6..CONTEXT_GETTER_LEN)?,
        [
            0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0x00, 0x00, 0x00, 0xBA, 0x10, 0x00, 0x00, 0x00,
            0x48, 0x8B, 0x04, 0xC8, 0x48, 0x8B, 0x04, 0x02, 0xC3,
        ]
    ) {
        return None;
    }
    let displacement = i32::from_le_bytes(displacement.try_into().ok()?);
    getter_address
        .checked_add(6)?
        .checked_add_signed(displacement as isize)
}

/// Rewrites the six-byte `mov ecx, [rip + disp32]` getter prefix after it has
/// been copied to a nearby trampoline.
pub(crate) fn relocate_tls_index_load(
    code: &mut [u8],
    original_address: usize,
    trampoline_address: usize,
) -> bool {
    let Some([0x8B, 0x0D, displacement_bytes @ ..]) = code.get_mut(..6) else {
        return false;
    };
    let Ok(old_displacement) = <[u8; 4]>::try_from(&*displacement_bytes) else {
        return false;
    };
    let old_displacement = i32::from_le_bytes(old_displacement) as isize;
    let Some(old_after) = original_address.checked_add(6) else {
        return false;
    };
    let old_destination = old_after.wrapping_add_signed(old_displacement);
    let Some(new_after) = trampoline_address.checked_add(6) else {
        return false;
    };
    let displacement = (old_destination as i128) - (new_after as i128);
    let Ok(displacement) = i32::try_from(displacement) else {
        return false;
    };
    displacement_bytes.copy_from_slice(&displacement.to_le_bytes());
    true
}

/// Produces an x86-64 `E9 rel32` branch when the target is reachable.
pub(crate) fn relative_jump(from: usize, to: usize) -> Option<[u8; 5]> {
    let displacement = (to as i128).checked_sub((from as i128).checked_add(5)?)?;
    let displacement = i32::try_from(displacement).ok()?;
    let mut bytes = [0xE9, 0, 0, 0, 0];
    bytes[1..].copy_from_slice(&displacement.to_le_bytes());
    Some(bytes)
}
