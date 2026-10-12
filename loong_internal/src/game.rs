//! Low-level Guild Wars 2 runtime discovery.
//!
//! The original private backend located the game context through the game
//! thread's TLS block.  This module reconstructs that protocol from the
//! release binary while keeping the dangerous part deliberately small: all
//! game memory is read through `ReadProcessMemory`, and the game thread is
//! always resumed when [`SuspendedGameThread`] is dropped.
//!
//! This is intentionally independent from the higher-level decoders.  The
//! latter change with client layouts, while the TLS discovery and thread
//! ownership checks have remained stable across the recovered builds.

#![cfg(windows)]

use std::{
    error::Error as StdError,
    ffi::c_void,
    fmt,
    mem::{MaybeUninit, size_of},
    sync::OnceLock,
};

mod capture;
pub mod content;
#[allow(dead_code)] // Retained only for its tested patch helpers; state reads use TLS polling.
mod hook;
pub mod state;

type Handle = isize;

const INVALID_HANDLE_VALUE: Handle = -1;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const THREAD_GET_CONTEXT: u32 = 0x0008;
const THREAD_QUERY_INFORMATION: u32 = 0x0040;
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
const GAME_THREAD_ACCESS: u32 =
    THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION | THREAD_SUSPEND_RESUME;
const TEB_TLS_POINTER_OFFSET: usize = 0x58;
const CONTEXT_TLS_VALUE_OFFSET: usize = 0x10;
const CONTEXT_COLLECTION_CH_CLI_CONTEXT: usize = 0x98;
const CH_CLI_CONTEXT_LOCAL_CONTEXT: usize = 0xA0;
const LOCAL_CONTEXT_PLAYER: usize = 0x20;
const CHARACTER_AGENT: usize = 0x98;
const GAME_CONTEXT_CONTENT_OFFSET: usize = 0xE0;
const DOS_HEADER_E_LFANEW_OFFSET: usize = 0x3C;
const PE_OPTIONAL_HEADER_SIZE_OF_IMAGE_OFFSET: usize = 0x50;
const MODULE_SCAN_CHUNK: usize = 0x1_0000;
const CONTEXT_GETTER_LEN: usize = 29;
// When the accessor signature cannot be located (it moves or is chained by
// another addon across client updates) the TLS slot index is unknown. Walk
// this many slots directly on the candidate thread — GW2's ContextCollection
// sits in a low-indexed TLS slot on the game thread.
const MAX_CONTEXT_TLS_SCAN: usize = 64;
// 2026-10-04: after a re-login the client stopped republishing the
// ContextCollection into the game thread's implicit TLS slot — every thread's
// indexed slot read zero on the live client while the tree itself stayed alive
// in the Windows heap. The tree objects are vtable-headed with consecutive
// main-module pointers, so heap discovery anchors on that shape and strongly
// validates the chain before publishing (live-verified on the running client:
// exactly one ChCliContext candidate exists and exactly one backlink passes
// the content-store check). The TLS paths above stay as the cheap fast path.
const HEAP_SCAN_CHUNK: usize = 0x1_0000;
const HEAP_WINDOW_CARRY: usize = 0x20;
const HEAP_CANDIDATE_LIMIT: usize = 16;
const HEAP_DISCOVERY_BACKOFF_MS: u32 = 2_000;
const MIN_SCANNED_ADDRESS: usize = 0x1_0000;
const MAX_USER_ADDRESS: usize = 0x7FFF_FFFF_FFFF;
const MEM_COMMIT: u32 = 0x1000;
const MEM_PRIVATE: u32 = 0x2_0000;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
// Character tree offsets used by the strong validation below. `CORE_STATS_
// PROFESSION` and `HEALTH_STRUCT_MAX` are read through their containing
// aligned words (the client is little-endian x86-64).
const CHARACTER_CORE_STATS: usize = 0x388;
const CORE_STATS_PROFESSION: usize = 0x12C;
const CHARACTER_HEALTH: usize = 0x3E8;
const HEALTH_STRUCT_MAX: usize = 0x10;

static CONTEXT_TLS_INDEX: OnceLock<Option<usize>> = OnceLock::new();

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CloseHandle(handle: Handle) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentProcessId() -> u32;
    fn GetModuleHandleW(module_name: *const u16) -> Handle;
    fn GetTickCount() -> u32;
    fn OpenThread(desired_access: u32, inherit_handle: i32, thread_id: u32) -> Handle;
    fn ReadProcessMemory(
        process: Handle,
        base_address: *const u8,
        buffer: *mut u8,
        size: usize,
        bytes_read: *mut usize,
    ) -> i32;
    fn ResumeThread(thread: Handle) -> u32;
    fn SuspendThread(thread: Handle) -> u32;
    fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    fn VirtualQuery(
        address: *const c_void,
        information: *mut MemoryBasicInformation,
        length: usize,
    ) -> usize;
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationThread(
        thread: Handle,
        information_class: u32,
        information: *mut ThreadBasicInformation,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ClientId {
    process: usize,
    thread: usize,
}

/// Native `THREAD_BASIC_INFORMATION` on 64-bit Windows.
///
/// Guild Wars 2 is a 64-bit client.  Keeping this type local avoids coupling
/// the recovered reader to unstable, undocumented SDK module paths.
#[repr(C)]
#[derive(Clone, Copy)]
struct ThreadBasicInformation {
    exit_status: i32,
    teb_base_address: usize,
    client_id: ClientId,
    affinity_mask: usize,
    priority: i32,
    base_priority: i32,
}

const _: () = assert!(size_of::<ThreadBasicInformation>() == 0x30);

/// Native `MEMORY_BASIC_INFORMATION` on 64-bit Windows, walked by the heap
/// discovery scan.
#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryBasicInformation {
    base_address: usize,
    allocation_base: usize,
    allocation_protect: u32,
    _alignment: u32,
    region_size: usize,
    state: u32,
    protect: u32,
    memory_type: u32,
    _alignment2: u32,
}

const _: () = assert!(size_of::<MemoryBasicInformation>() == 0x30);

/// Errors produced while locating or safely reading the game context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameError {
    /// The current build is not the x86-64 game process expected by the
    /// recovered TLS layout.
    UnsupportedArchitecture,
    /// Windows rejected an operation.  The payload is `GetLastError()`.
    Windows(u32),
    /// `NtQueryInformationThread(ThreadBasicInformation)` failed.
    NtStatus(i32),
    /// No process thread matched the recovered TLS context topology.
    ThreadNotFound,
    /// The handle was valid, but `SuspendThread` failed.
    SuspendFailed,
    /// A memory read was incomplete despite a successful API call.
    PartialRead { expected: usize, actual: usize },
    /// A content-table anchor did not match the versioned layout recovered
    /// from the 0.12.0 release.
    IncompatibleContentLayout,
    /// A call through the content table's vtable raised a hardware exception.
    /// The SEH guard contained it and the scan fails closed.
    ContentCallFaulted,
}

impl fmt::Display for GameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedArchitecture => {
                formatter.write_str("GW2 runtime reader requires x86-64")
            }
            Self::Windows(code) => write!(formatter, "Windows error {code}"),
            Self::NtStatus(status) => {
                write!(formatter, "NtQueryInformationThread failed: {status:#x}")
            }
            Self::ThreadNotFound => formatter.write_str("Game thread not found"),
            Self::SuspendFailed => formatter.write_str("Failed to suspend game thread"),
            Self::PartialRead { expected, actual } => {
                write!(
                    formatter,
                    "Incomplete process-memory read: {actual}/{expected} bytes"
                )
            }
            Self::IncompatibleContentLayout => formatter
                .write_str("Guild Wars 2 content layout is not compatible with this reader"),
            Self::ContentCallFaulted => {
                formatter.write_str("Guild Wars 2 content-table call raised a hardware exception")
            }
        }
    }
}

impl StdError for GameError {}

/// A validated observation of the per-client game context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextProbe {
    /// Address of the context object obtained from the game thread's TLS.
    pub context_address: usize,
    /// The content-store pointer at the recovered 0.12.0 offset, when the
    /// client has initialized it.  A missing value is normal at the login
    /// screen and must not be treated as an invalid context.
    pub content_store_address: Option<usize>,
}

/// Owns the native handle used to suspend the identified game thread.
#[derive(Debug)]
pub struct GameThread {
    context_address: usize,
    handle: Handle,
    thread_id: u32,
}

impl GameThread {
    /// Thread identifier returned by Toolhelp.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.thread_id
    }

    /// Game context address after the TLS link validation.
    #[must_use]
    pub const fn context_address(&self) -> usize {
        self.context_address
    }

    fn has_valid_context(&self) -> bool {
        context_layout_is_valid(self.context_address).unwrap_or(false)
    }
}

impl Drop for GameThread {
    fn drop(&mut self) {
        if self.handle != 0 && self.handle != INVALID_HANDLE_VALUE {
            // The handle is owned exclusively by this value and can no longer
            // be observed after Drop.
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Caches a game-thread handle and rediscovers it when the TLS link changes.
///
/// Keep a finder on the worker thread which consumes the decoded data.  It is
/// deliberately not a global singleton: this makes unloading deterministic and
/// ensures that a stale OS handle is closed promptly.
#[derive(Debug, Default)]
pub struct GameThreadFinder {
    cached: Option<GameThread>,
    /// Tick after which a new discovery attempt may run, when the previous one
    /// failed.  A failed discovery costs a full heap scan, so a state without
    /// a live tree (login, character select) must throttle the worker instead
    /// of rescanning on every poll.
    throttled_until: Option<u32>,
}

impl GameThreadFinder {
    /// Locates the game thread and returns its validated context address.
    pub fn locate(&mut self) -> Result<&GameThread, GameError> {
        if !self
            .cached
            .as_ref()
            .is_some_and(GameThread::has_valid_context)
        {
            let now = unsafe { GetTickCount() };
            let throttled = self
                .throttled_until
                .is_some_and(|until| now.wrapping_sub(until) >= 0x8000_0000);
            if throttled {
                return Err(GameError::ThreadNotFound);
            }
            match discover_game_thread() {
                Ok(game_thread) => self.cached = Some(game_thread),
                Err(error) => {
                    self.throttled_until = Some(now.wrapping_add(HEAP_DISCOVERY_BACKOFF_MS));
                    return Err(error);
                }
            }
        }

        self.cached.as_ref().ok_or(GameError::ThreadNotFound)
    }

    /// Suspends the game thread for a bounded memory-reading operation.
    ///
    /// Do not take application locks, perform I/O, or call into game code
    /// while the returned guard is alive.  Those actions can deadlock the
    /// suspended game thread.  Memory reads through the guard are safe from
    /// torn pointer traversal and it resumes the thread automatically.
    pub fn suspend(&mut self) -> Result<SuspendedGameThread<'_>, GameError> {
        let game_thread = self.locate()?;
        let previous_count = unsafe { SuspendThread(game_thread.handle) };
        if previous_count == u32::MAX {
            return Err(GameError::SuspendFailed);
        }

        Ok(SuspendedGameThread {
            game_thread,
            suspended: true,
        })
    }

    /// Closes the cached thread handle immediately.
    pub fn clear(&mut self) {
        self.cached = None;
    }
}

/// RAII guard for a suspended game thread.
#[derive(Debug)]
pub struct SuspendedGameThread<'thread> {
    game_thread: &'thread GameThread,
    suspended: bool,
}

impl SuspendedGameThread<'_> {
    /// Context address associated with the suspended game thread.
    #[must_use]
    pub const fn context_address(&self) -> usize {
        self.game_thread.context_address
    }

    /// Reads a plain-old-data value from the current process without creating
    /// a Rust reference to foreign game memory.
    pub fn read<T: Copy>(&self, address: usize) -> Result<T, GameError> {
        read_process_memory(address)
    }

    /// Reads a pointer-sized word from the game address space.
    pub fn read_usize(&self, address: usize) -> Result<usize, GameError> {
        self.read(address)
    }

    /// Records the stable context and optional content-store anchor used by
    /// the recovered skill/item table walkers.
    pub fn probe(&self) -> Result<ContextProbe, GameError> {
        let content_store_address = self
            .read_usize(self.context_address() + GAME_CONTEXT_CONTENT_OFFSET)
            .ok()
            .filter(|address| *address != 0);

        Ok(ContextProbe {
            context_address: self.context_address(),
            content_store_address,
        })
    }
}

impl Drop for SuspendedGameThread<'_> {
    fn drop(&mut self) {
        if self.suspended {
            // There is no recoverable path during Drop.  ResumeThread is still
            // attempted exactly once, preserving the release binary's RAII
            // semantics even if the decoder returned an error.
            unsafe { ResumeThread(self.game_thread.handle) };
            self.suspended = false;
        }
    }
}

fn discover_game_thread() -> Result<GameThread, GameError> {
    if size_of::<usize>() != 8 {
        return Err(GameError::UnsupportedArchitecture);
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(GameError::Windows(last_error()));
    }
    let snapshot = HandleGuard(snapshot);
    let current_process = unsafe { GetCurrentProcessId() };

    let mut entry = ThreadEntry32 {
        size: size_of::<ThreadEntry32>() as u32,
        usage: 0,
        thread_id: 0,
        owner_process_id: 0,
        base_priority: 0,
        delta_priority: 0,
        flags: 0,
    };
    let mut has_entry = unsafe { Thread32First(snapshot.0, &mut entry) } != 0;
    while has_entry {
        if entry.owner_process_id == current_process
            && let Some(game_thread) = inspect_thread(entry.thread_id)?
        {
            log_discovery_once("TLS", game_thread.context_address);
            return Ok(game_thread);
        }

        entry.size = size_of::<ThreadEntry32>() as u32;
        has_entry = unsafe { Thread32Next(snapshot.0, &mut entry) } != 0;
    }

    // 2026-10-04: the client stopped republishing the ContextCollection into
    // the game thread's implicit TLS slot after a re-login (verified live: all
    // TLS slots read zero while the tree stays alive in the heap and other
    // readers still resolve it). The tree objects are vtable-headed with
    // consecutive main-module pointers, so the heap scan anchors on that shape
    // and validates the full chain before publishing. The result is wrapped in
    // a handle-less `GameThread` because no thread owns this context.
    if let Some(collection) = discover_collection_by_heap_scan() {
        log_discovery_once("heap scan", collection);
        return Ok(GameThread {
            context_address: collection,
            handle: 0,
            thread_id: 0,
        });
    }

    log_discovery_failure_once();
    Err(GameError::ThreadNotFound)
}

/// Logs the first successful context discovery and the path taken. The
/// heap-scan fallback is diagnostic: it is only reached when the TLS walk
/// cannot resolve a valid tree.
fn log_discovery_once(path: &str, address: usize) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::info!("[loong-runtime] context discovered via {path}: {address:#x}");
    }
}

/// Logs the first complete discovery failure once. The finder retries on a
/// backoff, so an unguarded log would spam.
fn log_discovery_failure_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "[loong-runtime] no game context discovered (TLS walk and heap scan both failed)"
        );
    }
}

fn inspect_thread(thread_id: u32) -> Result<Option<GameThread>, GameError> {
    let handle = unsafe { OpenThread(GAME_THREAD_ACCESS, 0, thread_id) };
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        // Toolhelp can expose short-lived threads which disappear before
        // OpenThread.  This is not a discovery failure.
        return Ok(None);
    }
    let handle = HandleGuard(handle);

    let mut information = MaybeUninit::<ThreadBasicInformation>::zeroed();
    let status = unsafe {
        NtQueryInformationThread(
            handle.0,
            0, // ThreadBasicInformation
            information.as_mut_ptr(),
            size_of::<ThreadBasicInformation>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        // An inaccessible thread is simply not usable as the game thread.
        return Ok(None);
    }
    let information = unsafe { information.assume_init() };
    if information.teb_base_address == 0 {
        return Ok(None);
    }

    let tls_array =
        match read_process_memory::<usize>(information.teb_base_address + TEB_TLS_POINTER_OFFSET) {
            Ok(tls) if tls != 0 => tls,
            _ => return Ok(None),
        };

    // Fast path: use the getter-derived TLS index when the accessor signature
    // is present. The getter is located once and cached via `context_tls_index`.
    let candidate = context_tls_index().and_then(|tls_index| {
        context_from_tls_array(tls_array, tls_index, |address| {
            read_process_memory::<usize>(address).ok()
        })
    });
    // Fallback: the client updated and the accessor signature moved or was
    // chained by another addon, so no TLS index is known (or the indexed slot
    // no longer holds the context). Walk the thread's TLS slots directly for a
    // valid context tree — this keeps game-thread discovery independent of the
    // accessor's code shape, which is what changes across client updates.
    let candidate = candidate.or_else(|| {
        (0..MAX_CONTEXT_TLS_SCAN).find_map(|tls_index| {
            context_from_tls_array(tls_array, tls_index, |address| {
                read_process_memory::<usize>(address).ok()
            })
            .filter(|context| context_layout_is_valid(*context).unwrap_or(false))
        })
    });
    // TLS slots owned by unrelated libraries are expected to contain pointers
    // which are not readable as a GW2 context. A failed shape check is a
    // non-match, not a failure of the complete thread scan.
    if let Some(candidate) = candidate
        && context_layout_is_valid(candidate).unwrap_or(false)
    {
        let handle = handle.into_raw();
        return Ok(Some(GameThread {
            context_address: candidate,
            handle,
            thread_id,
        }));
    }

    Ok(None)
}

/// Validates a ContextCollection against the recovered layout with the strong
/// shape checks shared by TLS discovery and heap discovery: every tree object
/// is headed by a main-module vtable pointer, the character carries a nonzero
/// agent, and its core/health objects expose a sane profession and health
/// maximum. The bare non-null chain check this replaces accepted false
/// positives whose "character" pointers landed in unrelated DLL code (observed
/// live 2026-10-04: TLS slots owned by other libraries carried objects which
/// only the module-pointer and profession/health checks reject).
fn context_layout_is_valid(context: usize) -> Result<bool, GameError> {
    let Some((module_base, image_size)) = main_module_range() else {
        return Ok(false);
    };
    let Some(module_end) = module_base.checked_add(image_size) else {
        return Ok(false);
    };
    let chcli_context = read_process_memory::<usize>(
        context
            .checked_add(CONTEXT_COLLECTION_CH_CLI_CONTEXT)
            .ok_or(GameError::ThreadNotFound)?,
    )?;
    if chcli_context == 0 {
        return Ok(false);
    }
    let mut read = |address: usize| read_process_memory::<usize>(address).ok();
    Ok(chcli_tree_validity(chcli_context, module_base, module_end, &mut read).is_some())
}

/// The main module's mapped image as `(base, image_size)`.
pub(crate) fn main_module_range() -> Option<(usize, usize)> {
    let module_base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
    if module_base == 0 {
        return None;
    }
    let nt_header_offset =
        read_process_memory::<u32>(module_base.checked_add(DOS_HEADER_E_LFANEW_OFFSET)?).ok()?
            as usize;
    let image_size = read_process_memory::<u32>(
        module_base
            .checked_add(nt_header_offset)?
            .checked_add(PE_OPTIONAL_HEADER_SIZE_OF_IMAGE_OFFSET)?,
    )
    .ok()? as usize;
    (CONTEXT_GETTER_LEN..=0x4000_0000)
        .contains(&image_size)
        .then_some((module_base, image_size))
}

fn is_module_pointer(value: usize, module_base: usize, module_end: usize) -> bool {
    (module_base..module_end).contains(&value)
}

fn is_sane_profession(profession: u32) -> bool {
    (1..=9).contains(&profession)
}

fn is_sane_health_max(max_health: f32) -> bool {
    max_health.is_finite() && (100.0..1_000_000.0).contains(&max_health)
}

/// Strong validation of the ChCliContext -> local context -> character tree.
///
/// Returns `Some(())` when the tree matches the recovered layout with all of:
/// module-headed ChCliContext/local/character objects, a nonzero agent, a sane
/// profession id on the core object, and a finite sane health maximum. Every
/// dereference goes through the provided reader so tests can drive it and the
/// production path never dereferences an unvalidated pointer.
fn chcli_tree_validity(
    chcli_context: usize,
    module_base: usize,
    module_end: usize,
    read: &mut impl FnMut(usize) -> Option<usize>,
) -> Option<()> {
    if !is_module_pointer(read(chcli_context)?, module_base, module_end) {
        return None;
    }
    let local_context = read(chcli_context.checked_add(CH_CLI_CONTEXT_LOCAL_CONTEXT)?)?;
    if !is_module_pointer(read(local_context)?, module_base, module_end) {
        return None;
    }
    let character = read(local_context.checked_add(LOCAL_CONTEXT_PLAYER)?)?;
    if !is_module_pointer(read(character)?, module_base, module_end) {
        return None;
    }
    if read(character.checked_add(CHARACTER_AGENT)?)? == 0 {
        return None;
    }
    let core = read(character.checked_add(CHARACTER_CORE_STATS)?)?;
    if core == 0 {
        return None;
    }
    // CORE_STATS_PROFESSION (0x12C) is the high half of the aligned word at
    // `core + 0x128` on the little-endian client.
    let profession = (read(core.checked_add(CORE_STATS_PROFESSION - 4)?)? >> 32) as u32;
    if !is_sane_profession(profession) {
        return None;
    }
    let health = read(character.checked_add(CHARACTER_HEALTH)?)?;
    if health == 0 {
        return None;
    }
    // HEALTH_STRUCT_MAX (0x10) is the low half of the aligned word at
    // `health + 0x10`.
    let max_health = f32::from_bits(read(health.checked_add(HEALTH_STRUCT_MAX)?)? as u32);
    if !is_sane_health_max(max_health) {
        return None;
    }
    Some(())
}

/// Discovers the live ContextCollection from the heap alone.
///
/// The ChCliContext instances are located by their vtable-shaped heads and
/// validated through [`chcli_tree_validity`]; each surviving context is then
/// matched to the collection that anchors it at `+0x98` and exposes a content
/// store with a main-module vtable. Live-verified 2026-10-04: exactly one
/// candidate survives each stage on the running client.
fn discover_collection_by_heap_scan() -> Option<usize> {
    for chcli_context in discover_chcli_contexts_by_heap_scan() {
        if let Some(collection) = collection_anchoring_chcli(chcli_context) {
            return Some(collection);
        }
    }
    None
}

/// Heap-scans for ChCliContext candidates whose tree passes strong validation.
///
/// A candidate head is a qword followed by two more consecutive main-module
/// pointers (the live object carries six), which is cheap to prefilter before
/// the full tree validation. The cursor threads the resume address across the
/// overlapping windows so every heap qword is tested exactly once.
fn discover_chcli_contexts_by_heap_scan() -> Vec<usize> {
    let Some((module_base, image_size)) = main_module_range() else {
        return Vec::new();
    };
    let Some(module_end) = module_base.checked_add(image_size) else {
        return Vec::new();
    };
    let mut chcli_contexts: Vec<usize> = Vec::new();
    let mut read = |address: usize| read_process_memory::<usize>(address).ok();
    let mut cursor = 0usize;
    for_each_heap_window(HEAP_WINDOW_CARRY, |window_base, window| {
        cursor = scan_window_for_chcli_contexts(
            cursor,
            window_base,
            window,
            module_base,
            module_end,
            &mut read,
            &mut chcli_contexts,
        );
        chcli_contexts.len() < HEAP_CANDIDATE_LIMIT
    });
    chcli_contexts
}

/// Scans one carry-overlapped window for ChCliContext candidates and returns
/// the resume address for the following window.
///
/// A candidate is a qword followed by two more consecutive main-module
/// pointers (the live object carries six); its `0x18`-byte head must fit the
/// window. Addresses below `cursor` were consumed by previous overlapping
/// windows and are skipped, so every heap qword is tested exactly once.
/// Found candidates are appended to `chcli_contexts` after their tree passes
/// [`chcli_tree_validity`].
fn scan_window_for_chcli_contexts(
    cursor: usize,
    window_base: usize,
    window: &[u8],
    module_base: usize,
    module_end: usize,
    read: &mut impl FnMut(usize) -> Option<usize>,
    chcli_contexts: &mut Vec<usize>,
) -> usize {
    let usable = window.len().saturating_sub(0x18);
    let mut offset = cursor.saturating_sub(window_base).min(usable / 8 * 8);
    while offset + 0x18 <= window.len() {
        let candidate = window_base + offset;
        let heads = (
            window_qword(window, offset),
            window_qword(window, offset + 8),
            window_qword(window, offset + 0x10),
        );
        if let (Some(head), Some(second), Some(third)) = heads
            && is_module_pointer(head, module_base, module_end)
            && is_module_pointer(second, module_base, module_end)
            && is_module_pointer(third, module_base, module_end)
            && !chcli_contexts.contains(&candidate)
            && chcli_tree_validity(candidate, module_base, module_end, read).is_some()
        {
            chcli_contexts.push(candidate);
        }
        offset += 8;
    }
    window_base + offset
}

/// Heap-scans for the ContextCollection anchoring `chcli_context` at `+0x98`.
///
/// Each hit is validated through the content store at `+0xE0`: it must be
/// nonzero and headed by a main-module vtable, which rejects the addon caches
/// and unrelated heap nodes that also hold the context pointer (verified live
/// 2026-10-04: OmniTool's static cache, a heap hash node, and a module-static
/// holder all fail this check while the true collection passes).
fn collection_anchoring_chcli(chcli_context: usize) -> Option<usize> {
    let (module_base, image_size) = main_module_range()?;
    let module_end = module_base.checked_add(image_size)?;
    let mut collection = None;
    for_each_heap_window(0, |window_base, window| {
        let mut offset = 0usize;
        while offset + size_of::<usize>() <= window.len() {
            if window_qword(window, offset) == Some(chcli_context) {
                let Some(hit) = window_base.checked_add(offset) else {
                    break;
                };
                let Some(candidate) = hit.checked_sub(CONTEXT_COLLECTION_CH_CLI_CONTEXT) else {
                    offset += size_of::<usize>();
                    continue;
                };
                let mut read = |address: usize| read_process_memory::<usize>(address).ok();
                if content_store_anchored_by_module(candidate, module_base, module_end, &mut read) {
                    collection = Some(candidate);
                    return false;
                }
            }
            offset += size_of::<usize>();
        }
        true
    });
    collection
}

/// Validates the content store anchored at `collection +
/// GAME_CONTEXT_CONTENT_OFFSET` (nonzero, headed by a main-module vtable).
fn content_store_anchored_by_module(
    collection: usize,
    module_base: usize,
    module_end: usize,
    read: &mut impl FnMut(usize) -> Option<usize>,
) -> bool {
    let Some(anchor) = collection.checked_add(GAME_CONTEXT_CONTENT_OFFSET) else {
        return false;
    };
    let Some(content_store) = read(anchor).filter(|content_store| *content_store != 0) else {
        return false;
    };
    // Reject unrelated module-headed objects (for example a UI widget) whose
    // vtable happens to live in the client image: the store must also expose a
    // non-null second word and module-resident iterator entry points.
    content::validate_content_store(content_store, module_base, module_end, read).is_some()
}

/// Reads a `usize` from `window` at `offset`, or `None` when out of range.
fn window_qword(window: &[u8], offset: usize) -> Option<usize> {
    let bytes: [u8; 8] = window
        .get(offset..offset + size_of::<usize>())?
        .try_into()
        .ok()?;
    Some(usize::from_le_bytes(bytes))
}

/// Visits overlapping byte windows over the committed private writable memory
/// (the Windows heap).
///
/// `carry` bytes of the previous chunk are prepended so candidates spanning a
/// chunk boundary stay visible; `visit` receives `(window base, bytes)` and
/// returning `false` stops the walk.
fn for_each_heap_window(carry: usize, mut visit: impl FnMut(usize, &[u8]) -> bool) {
    let mut information = MaybeUninit::<MemoryBasicInformation>::zeroed();
    // The queried address always advances to the next region, so the first
    // region may still start below `MIN_SCANNED_ADDRESS` (the free region
    // around the null page contains it); the window math clamps each region
    // to its scannable overlap instead of rejecting the walk.
    let mut address = MIN_SCANNED_ADDRESS;
    loop {
        let written = unsafe {
            VirtualQuery(
                address as *const c_void,
                information.as_mut_ptr(),
                size_of::<MemoryBasicInformation>(),
            )
        };
        if written != size_of::<MemoryBasicInformation>() {
            return;
        }
        let information = unsafe { information.assume_init() };
        if information.region_size == 0 {
            return;
        }
        let Some(region_end) = information
            .base_address
            .checked_add(information.region_size)
        else {
            return;
        };
        if information.base_address > MAX_USER_ADDRESS || region_end <= address {
            return;
        }
        let scan_base = address.max(information.base_address);
        let scan_end = region_end.min(MAX_USER_ADDRESS + 1);
        address = region_end;
        let scan_length = scan_end.saturating_sub(scan_base);
        if information.state != MEM_COMMIT
            || information.memory_type != MEM_PRIVATE
            || !is_scannable_protection(information.protect)
            || scan_length <= carry
        {
            continue;
        }

        let mut tail = Vec::new();
        let mut offset = 0usize;
        while offset < scan_length {
            let length = (scan_length - offset).min(HEAP_SCAN_CHUNK);
            let Ok(bytes) = read_process_bytes(scan_base + offset, length) else {
                break;
            };
            let carried = tail.len();
            let Some(window_base) = scan_base
                .checked_add(offset)
                .and_then(|fresh_base| fresh_base.checked_sub(carried))
            else {
                break;
            };
            let mut window = std::mem::take(&mut tail);
            window.extend_from_slice(&bytes);
            if !visit(window_base, &window) {
                return;
            }
            let keep = carry.min(window.len());
            tail = window[window.len() - keep..].to_vec();
            offset += length;
        }
    }
}

fn is_scannable_protection(protect: u32) -> bool {
    matches!(
        protect,
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    )
}

fn context_from_tls_array(
    tls_array: usize,
    tls_index: usize,
    mut read_usize: impl FnMut(usize) -> Option<usize>,
) -> Option<usize> {
    let slot_address = tls_array.checked_add(tls_index.checked_mul(size_of::<usize>())?)?;
    let slot = read_usize(slot_address)?;
    let context = read_usize(slot.checked_add(CONTEXT_TLS_VALUE_OFFSET)?)?;
    (context != 0).then_some(context)
}

fn context_tls_index() -> Option<usize> {
    *CONTEXT_TLS_INDEX.get_or_init(discover_context_tls_index)
}

fn discover_context_tls_index() -> Option<usize> {
    let module_base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
    if module_base == 0 {
        return None;
    }
    let nt_header_offset =
        read_process_memory::<u32>(module_base.checked_add(DOS_HEADER_E_LFANEW_OFFSET)?).ok()?
            as usize;
    let image_size = read_process_memory::<u32>(
        module_base
            .checked_add(nt_header_offset)?
            .checked_add(PE_OPTIONAL_HEADER_SIZE_OF_IMAGE_OFFSET)?,
    )
    .ok()? as usize;
    if !(CONTEXT_GETTER_LEN..=0x4000_0000).contains(&image_size) {
        return None;
    }

    let mut offset = 0usize;
    while offset < image_size {
        let remaining = image_size.checked_sub(offset)?;
        let read_len = remaining.min(MODULE_SCAN_CHUNK + CONTEXT_GETTER_LEN - 1);
        let bytes = read_process_bytes(module_base.checked_add(offset)?, read_len).ok()?;
        if let Some(index_address) =
            bytes
                .windows(CONTEXT_GETTER_LEN)
                .enumerate()
                .find_map(|(index, bytes)| {
                    context_getter_tls_index_address(module_base + offset + index, bytes)
                })
        {
            let index = read_process_memory::<u32>(index_address).ok()? as usize;
            return (index < 4096).then_some(index);
        }
        offset = offset.checked_add(MODULE_SCAN_CHUNK)?;
    }
    None
}

fn context_getter_tls_index_address(getter_address: usize, bytes: &[u8]) -> Option<usize> {
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

pub(crate) fn read_process_bytes(address: usize, size: usize) -> Result<Vec<u8>, GameError> {
    let mut bytes = vec![0; size];
    let mut bytes_read = 0usize;
    let success = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address as *const u8,
            bytes.as_mut_ptr(),
            size,
            &mut bytes_read,
        )
    };
    if success == 0 {
        return Err(GameError::Windows(last_error()));
    }
    if bytes_read != size {
        return Err(GameError::PartialRead {
            expected: size,
            actual: bytes_read,
        });
    }
    Ok(bytes)
}

pub(crate) fn read_process_memory<T: Copy>(address: usize) -> Result<T, GameError> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut bytes_read = 0usize;
    let success = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            address as *const u8,
            value.as_mut_ptr().cast(),
            size_of::<T>(),
            &mut bytes_read,
        )
    };
    if success == 0 {
        return Err(GameError::Windows(last_error()));
    }
    if bytes_read != size_of::<T>() {
        return Err(GameError::PartialRead {
            expected: size_of::<T>(),
            actual: bytes_read,
        });
    }

    Ok(unsafe { value.assume_init() })
}

fn last_error() -> u32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default() as u32
}

#[derive(Debug)]
struct HandleGuard(Handle);

impl HandleGuard {
    fn into_raw(mut self) -> Handle {
        let handle = self.0;
        self.0 = 0;
        handle
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod capture_tests {
    use super::capture::{CaptureState, CapturedContext, resolve_context_tree};
    use super::hook::{CaptureOutcome, capture_collection};

    #[test]
    fn resolves_current_context_tree_through_the_cli_local_context() {
        let tree = resolve_context_tree(0x1000, |address| match address {
            0x1098 => Some(0x2000),
            0x20a0 => Some(0x3000),
            0x3020 => Some(0x4000),
            0x4098 => Some(0x5000),
            _ => None,
        });

        assert_eq!(tree, Some((0x2000, 0x4000)));
    }

    #[test]
    fn rejects_stale_capture() {
        let state = CaptureState::default();
        state.publish(CapturedContext::new(1, 2, 3, 100));

        assert!(state.latest_at(201, 100).is_none());
    }

    #[test]
    fn invalid_startup_tree_retains_a_fresh_collection_for_later_resolution() {
        let state = CaptureState::default();

        assert_eq!(
            capture_collection(&state, 100, 0x100_000, |_| None),
            CaptureOutcome::InvalidTree
        );

        assert_eq!(state.latest_collection_at(500, 1_000), Some(0x100_000));
        assert_eq!(state.latest_collection_at(1_101, 1_000), None);
    }
}

#[cfg(test)]
mod hook_tests {
    use super::capture::CaptureState;
    use super::hook::{
        CaptureOutcome, CaptureStatus, CaptureTelemetry, ContextGetterTarget, capture_collection,
        capture_is_due, context_getter_patch, context_getter_target, context_tls_index_address,
        relative_jump, relocate_tls_index_load, resolve_and_publish_context,
    };

    #[test]
    fn context_signature_decodes_tls_index_address() {
        let bytes = [
            0x8B, 0x0D, 0x10, 0, 0, 0, 0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0, 0xBA, 0x10, 0,
            0, 0, 0x48, 0x8B, 0x04, 0xC8, 0x48, 0x8B, 0x04, 0x02, 0xC3,
        ];

        assert_eq!(context_tls_index_address(0x1000, &bytes), Some(0x1016));
    }

    #[test]
    fn rel32_requires_reachable_destination() {
        assert!(relative_jump(0x1000, 0x2000).is_some());
        assert!(relative_jump(0x1000, usize::MAX).is_none());
    }

    #[test]
    fn getter_trampoline_keeps_the_tls_index_load_target() {
        let mut getter_prefix = [0x8B, 0x0D, 0x10, 0, 0, 0];

        assert!(relocate_tls_index_load(&mut getter_prefix, 0x1000, 0x2000));

        let displacement = i32::from_le_bytes(getter_prefix[2..].try_into().unwrap()) as isize;
        assert_eq!((0x2006usize).wrapping_add_signed(displacement), 0x1016);
    }

    #[test]
    fn direct_getter_patch_replaces_exactly_the_tls_load() {
        assert_eq!(
            context_getter_patch(0x1000, 0x2000),
            Some([0xE9, 0xFB, 0x0F, 0x00, 0x00, 0x90])
        );
    }

    #[test]
    fn existing_relative_getter_detour_is_chained() {
        let target = 0x1000;
        let chained = 0x3000;
        let mut bytes = [0u8; 29];
        bytes[..5].copy_from_slice(&relative_jump(target, chained).unwrap());

        assert_eq!(
            context_getter_target(target, &bytes),
            Some(ContextGetterTarget::Chained(chained))
        );
    }

    #[test]
    fn direct_getter_result_publishes_the_validated_context_tree() {
        let state = CaptureState::default();
        let outcome = capture_collection(&state, 100, 0x100000, |address| match address {
            0x100098 => Some(0x200000),
            0x2000a0 => Some(0x300000),
            0x300020 => Some(0x400000),
            0x400098 => Some(0x500000),
            _ => None,
        });

        assert_eq!(outcome, CaptureOutcome::Captured);
        assert_eq!(
            state.latest_at(100, 0),
            Some(super::capture::CapturedContext::new(
                0x100000, 0x200000, 0x400000, 100
            ))
        );
    }

    #[test]
    fn worker_can_publish_a_tls_collection_without_installing_a_getter_hook() {
        let state = CaptureState::default();

        let captured =
            resolve_and_publish_context(&state, 100, 0x100000, |address| match address {
                0x100098 => Some(0x200000),
                0x2000a0 => Some(0x300000),
                0x300020 => Some(0x400000),
                0x400098 => Some(0x500000),
                _ => None,
            });

        assert_eq!(
            captured,
            Some(super::capture::CapturedContext::new(
                0x100000, 0x200000, 0x400000, 100
            ))
        );
    }

    #[test]
    fn context_capture_runs_at_most_once_per_interval_across_tick_wrap() {
        assert!(!capture_is_due(1_000, 1_099));
        assert!(capture_is_due(1_000, 1_100));
        assert!(!capture_is_due(u32::MAX - 49, 25));
        assert!(capture_is_due(u32::MAX - 49, 50));
    }

    #[test]
    fn capture_telemetry_reports_missing_hook_callbacks() {
        assert_eq!(
            CaptureTelemetry::new(0, 0, 0, 0, 0).status(),
            CaptureStatus::NoHookCallbacks
        );
    }

    #[test]
    fn capture_telemetry_reports_a_rejected_context_getter_result() {
        assert_eq!(
            CaptureTelemetry::new(12, 3, 3, 0, 0).status(),
            CaptureStatus::ContextGetterRejected
        );
    }

    #[test]
    fn capture_telemetry_reports_a_rejected_context_tree() {
        assert_eq!(
            CaptureTelemetry::new(12, 3, 0, 3, 0).status(),
            CaptureStatus::ContextTreeRejected
        );
    }

    #[test]
    fn capture_telemetry_reports_a_successful_capture() {
        assert_eq!(
            CaptureTelemetry::new(12, 3, 0, 0, 3).status(),
            CaptureStatus::Captured
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn native_structures_match_the_recovered_abi() {
        assert_eq!(size_of::<ThreadEntry32>(), 0x1C);
        assert_eq!(size_of::<ThreadBasicInformation>(), 0x30);
        assert_eq!(size_of::<MemoryBasicInformation>(), 0x30);
        assert_eq!(GAME_THREAD_ACCESS, 0x4A);
    }

    const TEST_MODULE_BASE: usize = 0x1400_0000;
    const TEST_MODULE_END: usize = 0x1500_0000;
    const TEST_CHCLI_CONTEXT: usize = 0x2000;
    const TEST_LOCAL_CONTEXT: usize = 0x3000;
    const TEST_CHARACTER: usize = 0x4000;
    const TEST_CORE: usize = 0x6000;
    const TEST_HEALTH: usize = 0x7000;

    /// A live-shaped ChCliContext tree under the fake module range, laid out
    /// at the `TEST_*` addresses for the rejection tests to mutate.
    fn valid_chcli_memory() -> HashMap<usize, usize> {
        let mut memory = HashMap::new();
        memory.insert(TEST_CHCLI_CONTEXT, TEST_MODULE_BASE + 0x33F8);
        memory.insert(
            TEST_CHCLI_CONTEXT + CH_CLI_CONTEXT_LOCAL_CONTEXT,
            TEST_LOCAL_CONTEXT,
        );
        memory.insert(TEST_LOCAL_CONTEXT, TEST_MODULE_BASE + 0x3E08);
        memory.insert(TEST_LOCAL_CONTEXT + LOCAL_CONTEXT_PLAYER, TEST_CHARACTER);
        memory.insert(TEST_CHARACTER, TEST_MODULE_BASE + 0x6020);
        memory.insert(TEST_CHARACTER + CHARACTER_AGENT, 0x5000);
        memory.insert(TEST_CHARACTER + CHARACTER_CORE_STATS, TEST_CORE);
        // Profession 3 in the high half of the word before CORE_STATS_PROFESSION.
        memory.insert(TEST_CORE + CORE_STATS_PROFESSION - 4, 3usize << 32);
        memory.insert(TEST_CHARACTER + CHARACTER_HEALTH, TEST_HEALTH);
        memory.insert(
            TEST_HEALTH + HEALTH_STRUCT_MAX,
            19_292.0f32.to_bits() as usize,
        );
        memory
    }

    fn test_reader(memory: &HashMap<usize, usize>) -> impl FnMut(usize) -> Option<usize> + '_ {
        move |address| memory.get(&address).copied()
    }

    #[test]
    fn chcli_tree_validity_accepts_the_recovered_shape() {
        let memory = valid_chcli_memory();

        assert_eq!(
            chcli_tree_validity(
                TEST_CHCLI_CONTEXT,
                TEST_MODULE_BASE,
                TEST_MODULE_END,
                &mut test_reader(&memory),
            ),
            Some(())
        );
    }

    #[test]
    fn chcli_tree_validity_rejects_a_character_inside_foreign_module_code() {
        let mut memory = valid_chcli_memory();
        // Live TLS-slot false positive (2026-10-04): the "character" pointer
        // landed inside d3d11's code range and passed the non-null chain.
        memory.insert(TEST_LOCAL_CONTEXT + LOCAL_CONTEXT_PLAYER, 0x7FFA_BBB0_23E8);

        assert_eq!(
            chcli_tree_validity(
                TEST_CHCLI_CONTEXT,
                TEST_MODULE_BASE,
                TEST_MODULE_END,
                &mut test_reader(&memory),
            ),
            None
        );
    }

    #[test]
    fn chcli_tree_validity_rejects_an_out_of_range_profession() {
        let mut memory = valid_chcli_memory();
        // Live false positive (2026-10-04): the core slot carried the float
        // 1.0 bit pattern (0x3F800000), not a profession id.
        memory.insert(
            TEST_CORE + CORE_STATS_PROFESSION - 4,
            (0x3F80_0000u64 << 32) as usize,
        );

        assert_eq!(
            chcli_tree_validity(
                TEST_CHCLI_CONTEXT,
                TEST_MODULE_BASE,
                TEST_MODULE_END,
                &mut test_reader(&memory),
            ),
            None
        );
    }

    #[test]
    fn chcli_tree_validity_rejects_an_insane_health_maximum() {
        let mut memory = valid_chcli_memory();
        memory.insert(TEST_HEALTH + HEALTH_STRUCT_MAX, 0);

        assert_eq!(
            chcli_tree_validity(
                TEST_CHCLI_CONTEXT,
                TEST_MODULE_BASE,
                TEST_MODULE_END,
                &mut test_reader(&memory),
            ),
            None
        );
    }

    #[test]
    fn window_scan_tests_each_qword_exactly_once_across_the_carry() {
        let mut memory = valid_chcli_memory();
        let head = TEST_MODULE_BASE + 0x33F8;
        // The fully-visible candidate's tree: its head is re-read by the
        // validator and its local link points at the live-shaped fixture.
        memory.insert(0x1008, head);
        memory.insert(0x1008 + CH_CLI_CONTEXT_LOCAL_CONTEXT, TEST_LOCAL_CONTEXT);
        // Window A: [0x1000, 0x1040). The candidate at 0x1008 is fully
        // visible; the head at 0x1030 spans the window end, so only window B
        // (carry-overlapped, [0x1020, 0x1060)) sees its full head — and its
        // tree is absent, so it must be rejected.
        let mut window_a = vec![0u8; 0x40];
        let mut window_b = vec![0u8; 0x40];
        for address in [0x1008, 0x1010, 0x1018, 0x1030, 0x1038, 0x1040, 0x1048] {
            let base = if address < 0x1040 {
                (&mut window_a, 0x1000)
            } else {
                (&mut window_b, 0x1020)
            };
            let (window, window_base) = base;
            let offset = address - window_base;
            window[offset..offset + 8].copy_from_slice(&head.to_le_bytes());
        }

        let mut chcli_contexts = Vec::new();
        let cursor_a = scan_window_for_chcli_contexts(
            0,
            0x1000,
            &window_a,
            TEST_MODULE_BASE,
            TEST_MODULE_END,
            &mut test_reader(&memory),
            &mut chcli_contexts,
        );
        assert_eq!(cursor_a, 0x1030);
        assert_eq!(chcli_contexts, vec![0x1008]);

        let cursor_b = scan_window_for_chcli_contexts(
            cursor_a,
            0x1020,
            &window_b,
            TEST_MODULE_BASE,
            TEST_MODULE_END,
            &mut test_reader(&memory),
            &mut chcli_contexts,
        );
        assert_eq!(cursor_b, 0x1050);
        assert_eq!(chcli_contexts, vec![0x1008]);
    }

    #[test]
    fn content_store_validation_requires_a_module_headed_store() {
        let mut memory = valid_chcli_memory();
        let collection = 0x1_0000;
        let vtable = TEST_MODULE_BASE + 0x100;
        memory.insert(collection + GAME_CONTEXT_CONTENT_OFFSET, 0x8000);
        memory.insert(0x8000, vtable);
        memory.insert(0x8008, 0x1234);
        memory.insert(
            vtable + content::CONTENT_VTABLE_NEXT_OFFSET,
            TEST_MODULE_BASE + 0x200,
        );
        memory.insert(
            vtable + content::CONTENT_VTABLE_LOOKUP_OFFSET,
            TEST_MODULE_BASE + 0x300,
        );

        assert!(content_store_anchored_by_module(
            collection,
            TEST_MODULE_BASE,
            TEST_MODULE_END,
            &mut test_reader(&memory),
        ));

        // Addon-cache shape: the store exists but its head is a heap pointer.
        memory.insert(0x8000, 0x1234_5678_9ABC);
        assert!(!content_store_anchored_by_module(
            collection,
            TEST_MODULE_BASE,
            TEST_MODULE_END,
            &mut test_reader(&memory),
        ));

        // Missing store.
        memory.insert(collection + GAME_CONTEXT_CONTENT_OFFSET, 0);
        assert!(!content_store_anchored_by_module(
            collection,
            TEST_MODULE_BASE,
            TEST_MODULE_END,
            &mut test_reader(&memory),
        ));
    }

    #[test]
    fn context_probe_preserves_addresses() {
        let probe = ContextProbe {
            context_address: 0x1234,
            content_store_address: Some(0x5678),
        };
        assert_eq!(probe.context_address, 0x1234);
        assert_eq!(probe.content_store_address, Some(0x5678));
    }

    #[test]
    fn resolves_context_through_the_indexed_tls_slot() {
        let reads = [(0x1000, 0x2000), (0x2010, 0x3000)];
        let context = context_from_tls_array(0x1000, 0, |address| {
            reads
                .iter()
                .find_map(|(expected, value)| (*expected == address).then_some(*value))
        });

        assert_eq!(context, Some(0x3000));
    }

    #[test]
    fn finds_the_tls_index_global_from_the_context_getter() {
        let getter = [
            0x8B, 0x0D, 0x00, 0x02, 0x00, 0x00, 0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0x00, 0x00,
            0x00, 0xBA, 0x10, 0x00, 0x00, 0x00, 0x48, 0x8B, 0x04, 0xC8, 0x48, 0x8B, 0x04, 0x02,
            0xC3,
        ];

        assert_eq!(
            context_getter_tls_index_address(0x1000, &getter),
            Some(0x1206)
        );
    }
}
