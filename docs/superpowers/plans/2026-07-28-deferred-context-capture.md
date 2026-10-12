# Deferred Context Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve a valid game-thread `ContextCollection` captured during startup and resolve its local-character chain once the client finishes initializing it.

**Architecture:** The hook records a fresh, user-space collection pointer even when its child pointers are temporarily unavailable. The worker resolves `collection + 0x98 -> cli + 0x98 -> local` at poll time and publishes only a fully validated local-character snapshot. This keeps all foreign-memory reads bounded and prevents an early loading-screen rejection from permanently disabling Buff, resource, and cooldown readers.

**Tech Stack:** Rust 2024, `ReadProcessMemory`, atomics, existing `CaptureState` and runtime state worker tests.

## Global Constraints

- Do not alter Buff/resource/cooldown field offsets without runtime evidence.
- Do not publish a local character until `local + 0x98` contains a nonzero agent pointer.
- Treat the collection anchor as stale after the existing 1,000 ms maximum age.
- Keep the hook callback allocation-free and lock-free.
- Do not commit, deploy, or modify unrelated user changes.

---

### Task 1: Retain and resolve a deferred collection anchor

**Files:**

- Modify: `loong_internal/src/game/capture.rs`
- Modify: `loong_internal/src/game/hook.rs`
- Test: `loong_internal/src/game.rs`

**Interfaces:**

- Produces `CaptureState::publish_collection(collection, captured_at)`.
- Produces `CaptureState::latest_collection_at(now, maximum_age) -> Option<usize>`.
- `capture_collection` records a valid collection before it attempts full-tree resolution.
- `hook::latest_context()` resolves a fresh collection anchor with `ReadProcessMemory` before returning `CapturedContext`.

- [ ] **Step 1: Write the failing regression test**

```rust
#[test]
fn invalid_startup_tree_retains_a_fresh_collection_for_later_resolution() {
    let state = CaptureState::default();

    state.publish_collection(0x100_000, 100);

    assert_eq!(state.latest_collection_at(500, 1_000), Some(0x100_000));
    assert_eq!(state.latest_collection_at(1_101, 1_000), None);
}
```

The production break this catches is dropping a valid TLS getter result because `cli`, `local`, or its agent pointer was still zero during startup.

- [ ] **Step 2: Run the test to verify RED**

Run:

```powershell
$tool='C:\Users\Loong\AppData\Local\Temp\gw2-reffect-rust-verify-20260726'
$env:CARGO_HOME="$tool\cargo"; $env:RUSTUP_HOME="$tool\rustup"; $env:PATH="$tool\cargo\bin;$env:PATH"
cargo test -p loong_internal invalid_startup_tree_retains_a_fresh_collection_for_later_resolution -- --nocapture
```

Expected: compilation failure because the collection-anchor API is absent.

- [ ] **Step 3: Implement the minimum deferred-resolution path**

```rust
state.publish_collection(collection, captured_at);
if let Some((cli, local)) = resolve_context_tree(collection, read) {
    state.publish(CapturedContext::new(collection, cli, local, captured_at));
}
```

Store the collection timestamp independently from the fully validated snapshot. In `latest_context`, read the latest fresh collection and call the existing `resolve_context_tree`; only return a context when the complete tree validates.

- [ ] **Step 4: Run the focused test to verify GREEN**

Run the command from Step 2.

Expected: the regression test passes, proving the anchor survives initialization and expires after its freshness window.

- [ ] **Step 5: Run state-reader regression coverage**

Run:

```powershell
$tool='C:\Users\Loong\AppData\Local\Temp\gw2-reffect-rust-verify-20260726'
$env:CARGO_HOME="$tool\cargo"; $env:RUSTUP_HOME="$tool\rustup"; $env:PATH="$tool\cargo\bin;$env:PATH"
cargo test -p loong_internal --lib
```

Expected: all capture, Buff, resource, and ability cooldown tests pass.

## Self-Review

- Scope is limited to context capture freshness; no gameplay offsets or decoder rules change.
- The sole new test observes a consumer-relevant lifetime contract and fails if the anchor is discarded or never expires.
- `CaptureState` API names and `CapturedContext` construction match the existing modules.
