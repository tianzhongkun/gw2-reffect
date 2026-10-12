# Runtime Reader Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore Reffect runtime memory publication on the current GW2 client without allowing a failed domain to clear unrelated data.

**Architecture:** A compact Windows detour invokes the TLS-backed Context getter only after `GameThreadUpdate` returns, validates and atomically publishes the current local-character tree, and is removed before unload. The existing worker consumes this captured pointer and applies independently decoded domains to `Context`.

**Tech Stack:** Rust 2024, Win32 FFI (`VirtualAlloc`, `VirtualProtect`, `FlushInstructionCache`), `ReadProcessMemory`, `loong_core::Context`, Nexus logging.

## Global Constraints

- Target is x86-64 Windows GW2 only; all foreign-memory access stays through `ReadProcessMemory`.
- `GameThreadUpdate` callback calls no allocation, locking, network, texture, or `Context` APIs.
- Hook teardown restores target bytes before releasing its trampoline.
- A failed Buff, target, resource, Build, or gear decoder must not suppress a successful independent domain.
- Keep `provider: UpdateProvider::None` in `reffect/src/lib.rs`.
- Do not commit or alter unrelated user changes.

---

### Task 1: Define testable capture and publication contracts

**Files:**
- Create: `loong_internal/src/game/capture.rs`
- Modify: `loong_internal/src/game.rs`
- Test: `loong_internal/src/game/capture.rs`

**Interfaces:**
- Produces `CapturedContext { collection: usize, cli: usize, local: usize, captured_at: u32 }`.
- Produces `CaptureState::publish(CapturedContext)`, `CaptureState::latest(max_age_ms) -> Option<CapturedContext>`, and `resolve_context_tree(context, read) -> Option<(usize, usize)>`.
- `state` consumes `CaptureState::latest` and never calls TLS discovery.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn resolves_current_context_tree_only_when_agent_is_present() {
    let tree = resolve_context_tree(0x1000, |address| match address {
        0x1098 => Some(0x2000),
        0x2098 => Some(0x3000),
        0x3098 => Some(0x4000),
        _ => None,
    });
    assert_eq!(tree, Some((0x2000, 0x3000)));
}

#[test]
fn rejects_stale_capture() {
    let state = CaptureState::default();
    state.publish(CapturedContext::new(1, 2, 3, 100));
    assert!(state.latest_at(201, 100).is_none());
}
```

- [ ] **Step 2: Verify red**

Run: `cargo test -p loong_internal capture::tests -- --nocapture`

Expected: compilation failure because `capture` and its public contracts do not exist.

- [ ] **Step 3: Implement minimal capture-state contracts**

```rust
pub(crate) fn resolve_context_tree(
    collection: usize,
    mut read: impl FnMut(usize) -> Option<usize>,
) -> Option<(usize, usize)> {
    let cli = read(collection.checked_add(0x98)?)?;
    let local = read(cli.checked_add(0x98)?)?;
    (read(local.checked_add(0x98)?)? != 0).then_some((cli, local))
}
```

Implement atomics for the four `CapturedContext` words and make stale reads return `None`.

- [ ] **Step 4: Verify green**

Run: `cargo test -p loong_internal capture::tests -- --nocapture`

Expected: both tests pass.

### Task 2: Install an unload-safe game-thread detour

**Files:**
- Create: `loong_internal/src/game/hook.rs`
- Modify: `loong_internal/src/game.rs`
- Modify: `loong_internal/Cargo.toml`
- Test: `loong_internal/src/game/hook.rs`

**Interfaces:**
- Produces `GameThreadCapture::install() -> Result<Self, GameError>` and `Drop` restoration.
- `GameThreadCapture` owns the target patch, original bytes, and trampoline.
- After every original tick, the hook calls `capture_context_on_game_thread()`.

- [ ] **Step 1: Write failing pure tests for signature and patch planning**

```rust
#[test]
fn context_signature_decodes_tls_index_address() {
    let bytes = [0x8B, 0x0D, 0x10, 0, 0, 0, 0x65, 0x48, 0x8B, 0x04, 0x25,
        0x58, 0, 0, 0, 0xBA, 0x10, 0, 0, 0, 0x48, 0x8B, 0x04, 0xC8,
        0x48, 0x8B, 0x04, 0x02, 0xC3];
    assert_eq!(context_tls_index_address(0x1000, &bytes), Some(0x1016));
}

#[test]
fn rel32_requires_reachable_destination() {
    assert!(relative_jump(0x1000, 0x2000).is_some());
    assert!(relative_jump(0x1000, usize::MAX).is_none());
}
```

- [ ] **Step 2: Verify red**

Run: `cargo test -p loong_internal hook::tests -- --nocapture`

Expected: compilation failure because hook helpers do not exist.

- [ ] **Step 3: Implement native hook and diagnostics**

Implement page protection, trampoline allocation within rel32 range, stolen-byte relocation for `E8`/`E9`, and restoration in `Drop`. Resolve `ContextCollection` using the verified byte pattern and resolve `GameThreadUpdate` via the stable RVA/pattern fallback. Publish original callback state before patching and restore bytes before freeing allocations.

- [ ] **Step 4: Verify green and compile native FFI**

Run: `cargo test -p loong_internal hook::tests -- --nocapture`

Expected: pure helper tests pass.

Run: `cargo check -p loong_internal --target x86_64-pc-windows-msvc`

Expected: Win32 FFI and hook module compile.

### Task 3: Make game-state decoding domain independent

**Files:**
- Modify: `loong_internal/src/game/state.rs`
- Test: `loong_internal/src/game/state.rs`

**Interfaces:**
- Replaces `RuntimeSnapshot` required `resources` and `buffs` with independent domain fields.
- Produces `RuntimeSnapshot::apply()` that leaves a successful player resource/skillbar update intact when Buffs returns `None`.
- `GameStateReader` uses the latest captured local pointer and a non-suspending process-memory reader.

- [ ] **Step 1: Write the failing regression test**

```rust
#[test]
fn unavailable_buffs_do_not_block_resources_or_skillbar_publication() {
    let snapshot = RuntimeSnapshot::for_test(
        Some(PlayerResources::empty()),
        None,
        Some(Skillbar::empty()),
    );
    let state = snapshot.apply_to(Context::empty());
    assert!(state.player.resources.is_ok());
    assert!(state.player.skillbar.is_ok());
    assert_eq!(state.player.buff_info, Err(Error::Buffs));
}
```

- [ ] **Step 2: Verify red**

Run: `cargo test -p loong_internal unavailable_buffs_do_not_block -- --nocapture`

Expected: test fails because the snapshot construction currently requires a `BuffMap` and `apply` has no isolated state seam.

- [ ] **Step 3: Implement minimum domain result model**

Create a testable `apply_to(&mut Context)` helper. Decode identity, resources, skillbar, build, gear, target and Buffs independently; map absent decoder results to their specific `Error` variant. Remove `GameThreadFinder::suspend()` from the state worker path; use the capture state’s validated local pointer with `ReadProcessMemory`.

- [ ] **Step 4: Verify green**

Run: `cargo test -p loong_internal unavailable_buffs_do_not_block -- --nocapture`

Expected: test passes and resource/skillbar remain `Ok`.

### Task 4: Restore current-client fields and runtime observability

**Files:**
- Modify: `loong_internal/src/game/state.rs`
- Modify: `loong_internal/src/lib.rs`
- Test: `loong_internal/src/game/state.rs`

**Interfaces:**
- `OpenInternal::init()` installs capture before starting state/catalog workers.
- `OpenInternal::deinit()` stops workers before dropping capture.
- Diagnostics emit `[reffect-runtime]` records only on transition or first failure.

- [ ] **Step 1: Write failing decoder tests for current offsets**

```rust
#[test]
fn current_health_layout_accepts_valid_float_pair() {
    assert!(valid_health_pair(11_645.0, 11_645.0));
    assert!(!valid_health_pair(f32::NAN, 11_645.0));
}

#[test]
fn locked_placeholder_skill_is_not_published() {
    assert!(!sane_skill_id(10_586));
}
```

- [ ] **Step 2: Verify red only for new health helper**

Run: `cargo test -p loong_internal current_health_layout_accepts_valid_float_pair -- --nocapture`

Expected: failure until the named helper is introduced.

- [ ] **Step 3: Implement lifecycle ordering and current fields**

Use verified current offsets for health, barrier, endurance, core identity, combatant skill definitions, pet skill fallback, Build and inventory/gear. Log hook install status, capture success count, and each first decoder miss under `[reffect-runtime]`.

- [ ] **Step 4: Verify package tests**

Run: `cargo test --workspace --all-targets`

Expected: all tests pass.

### Task 5: Build and hot-load against the running GW2 client

**Files:**
- Modify: `E:\\Games\\GuildWars2\\addons\\reffect.dll` (deployment copy only)
- Evidence: `E:\\Games\\GuildWars2\\addons\\Nexus\\Nexus.log`

**Interfaces:**
- The deployed DLL contains the current source build and continues to use `UpdateProvider::None`.
- Nexus log contains `[reffect-runtime] hook installed` and a positive capture count.

- [ ] **Step 1: Build release DLL**

Run:

```powershell
$tool='C:\\Users\\Loong\\AppData\\Local\\Temp\\gw2-reffect-rust-verify-20260726'
$env:CARGO_HOME="$tool\\cargo"
$env:RUSTUP_HOME="$tool\\rustup"
$env:PATH="$tool\\cargo\\bin;$env:PATH"
cargo build -p reffect --release
```

Expected: `target\\release\\reffect.dll` is created with no compile errors.

- [ ] **Step 2: Preserve deployment backup then deploy**

Verify the named official backup exists, then copy only the successful release DLL to `E:\\Games\\GuildWars2\\addons\\reffect.dll` while the addon is unloaded/reloaded through Nexus.

- [ ] **Step 3: Run runtime acceptance query**

Run:

```powershell
rg -n '\[reffect-runtime\].*(hook installed|capture ok|domain)' E:\\Games\\GuildWars2\\addons\\Nexus\\Nexus.log
```

Expected: positive hook/capture diagnostics and no repeated state-reader failure.

- [ ] **Step 4: Verify user-visible domains**

Open Reffect’s debug UI and verify player health and skillbar contain values. Verify an unavailable Buff path reports only Buffs unavailable while health and skillbar continue rendering.

