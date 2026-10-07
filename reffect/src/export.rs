//! C ABI exports so other in-process modules can read the current game state.
//!
//! The data is captured continuously by the reader and published into
//! [`Context`](reffect_core::context::Context); the following functions expose
//! that snapshot as a C-compatible struct, mirroring the `Omni_GetEngine`
//! pattern used by OmniReader.

use reffect_core::{
    context::Context,
    snapshot::{RfSnapshot, fill_snapshot},
};
use std::cell::RefCell;

thread_local! {
    static SNAPSHOT: RefCell<RfSnapshot> = RefCell::new(RfSnapshot::empty());
}

/// Returns a pointer to this thread's snapshot of the current game state.
///
/// The snapshot is refreshed on every call and lives until the next call on the
/// same thread, so read (or copy) it before calling again. `null` is never
/// returned; a snapshot with all validity flags off simply means no data is
/// currently available. Pointer stays valid on the calling thread.
#[unsafe(no_mangle)]
pub extern "C" fn reffect_snapshot() -> *const RfSnapshot {
    {
        let ctx = Context::lock();
        SNAPSHOT.with(|slot| {
            let mut snapshot = slot.borrow_mut();
            fill_snapshot(&mut snapshot, &ctx);
        });
    }
    SNAPSHOT.with(|slot| slot.as_ptr())
}

/// Returns the size in bytes of [`RfSnapshot`], so a consumer can either use
/// [`reffect_snapshot`] directly or allocate its own buffer of this size.
#[unsafe(no_mangle)]
pub extern "C" fn reffect_snapshot_size() -> usize {
    std::mem::size_of::<RfSnapshot>()
}
