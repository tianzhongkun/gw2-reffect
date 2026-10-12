//! Lock-free publication of the game-thread ContextCollection tree.
//!
//! The TLS-backed ContextCollection getter may only run on the game thread.
//! This module therefore publishes the validated pointer chain for worker
//! readers without ever exposing a Rust reference to game-owned memory.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

const CONTEXT_COLLECTION_CH_CLI_CONTEXT: usize = 0x98;
const CH_CLI_CONTEXT_LOCAL_CONTEXT: usize = 0xA0;
const LOCAL_CONTEXT_PLAYER: usize = 0x20;
const CHARACTER_AGENT: usize = 0x98;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapturedContext {
    pub(crate) collection: usize,
    pub(crate) cli: usize,
    pub(crate) local: usize,
    captured_at: u32,
}

impl CapturedContext {
    #[must_use]
    pub(crate) const fn new(collection: usize, cli: usize, local: usize, captured_at: u32) -> Self {
        Self {
            collection,
            cli,
            local,
            captured_at,
        }
    }
}

/// Atomically stores the most recently validated game-thread context.
///
/// `sequence` is an odd/even writer marker. Readers retry if a capture overlaps
/// their loads, preventing a mixed tree from being published to a worker.
#[derive(Debug, Default)]
pub(crate) struct CaptureState {
    sequence: AtomicU64,
    collection: AtomicUsize,
    collection_captured_at: AtomicU32,
    cli: AtomicUsize,
    local: AtomicUsize,
    captured_at: AtomicU32,
}

impl CaptureState {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            collection: AtomicUsize::new(0),
            collection_captured_at: AtomicU32::new(0),
            cli: AtomicUsize::new(0),
            local: AtomicUsize::new(0),
            captured_at: AtomicU32::new(0),
        }
    }

    /// Records a valid collection returned by the game-thread TLS getter.
    ///
    /// During a loading transition its child tree may not contain the local
    /// character yet, so this deliberately does not require it to validate.
    pub(crate) fn publish_collection(&self, collection: usize, captured_at: u32) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.collection.store(collection, Ordering::Relaxed);
        self.collection_captured_at
            .store(captured_at, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn publish(&self, context: CapturedContext) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.collection.store(context.collection, Ordering::Relaxed);
        self.collection_captured_at
            .store(context.captured_at, Ordering::Relaxed);
        self.cli.store(context.cli, Ordering::Relaxed);
        self.local.store(context.local, Ordering::Relaxed);
        self.captured_at
            .store(context.captured_at, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    #[must_use]
    pub(crate) fn latest_collection_at(&self, now: u32, maximum_age: u32) -> Option<usize> {
        for _ in 0..3 {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                continue;
            }
            let collection = self.collection.load(Ordering::Relaxed);
            let captured_at = self.collection_captured_at.load(Ordering::Relaxed);
            let after = self.sequence.load(Ordering::Acquire);
            if before != after || after & 1 != 0 {
                continue;
            }
            if collection == 0 || captured_at == 0 || now.wrapping_sub(captured_at) > maximum_age {
                return None;
            }
            return Some(collection);
        }
        None
    }

    #[must_use]
    pub(crate) fn latest_at(&self, now: u32, maximum_age: u32) -> Option<CapturedContext> {
        for _ in 0..3 {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                continue;
            }

            let context = CapturedContext::new(
                self.collection.load(Ordering::Relaxed),
                self.cli.load(Ordering::Relaxed),
                self.local.load(Ordering::Relaxed),
                self.captured_at.load(Ordering::Relaxed),
            );
            let after = self.sequence.load(Ordering::Acquire);
            if before != after || after & 1 != 0 {
                continue;
            }
            if context.collection == 0
                || context.cli == 0
                || context.local == 0
                || context.captured_at == 0
                || now.wrapping_sub(context.captured_at) > maximum_age
            {
                return None;
            }
            return Some(context);
        }
        None
    }
}

/// Validates the current live layout:
/// `ContextCollection + 0x98 -> ChCliContext + 0xA0 -> local context + 0x20
/// -> local character`.
pub(crate) fn resolve_context_tree(
    collection: usize,
    mut read: impl FnMut(usize) -> Option<usize>,
) -> Option<(usize, usize)> {
    if collection == 0 {
        return None;
    }
    let cli = read(collection.checked_add(CONTEXT_COLLECTION_CH_CLI_CONTEXT)?)?;
    if cli == 0 {
        return None;
    }
    let local_context = read(cli.checked_add(CH_CLI_CONTEXT_LOCAL_CONTEXT)?)?;
    if local_context == 0 {
        return None;
    }
    let local = read(local_context.checked_add(LOCAL_CONTEXT_PLAYER)?)?;
    if local == 0 {
        return None;
    }
    (read(local.checked_add(CHARACTER_AGENT)?)? != 0).then_some((cli, local))
}
