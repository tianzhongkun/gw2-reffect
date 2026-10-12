//! C ABI 导出：让进程内的其它模块（例如 ACR）直接读取当前角色状态。
//!
//! 数据由内存读取器持续写入 Context，下面的函数把它展平成 C 兼容的固定布局结构体。
//! reffect_snapshot 系列是旧名别名，保留以兼容既有调用方。

use loong_core::{
    context::Context,
    snapshot::{RfSnapshot, fill_snapshot},
};
use std::cell::RefCell;

thread_local! {
    static SNAPSHOT: RefCell<RfSnapshot> = RefCell::new(RfSnapshot::empty());
}

/// 返回当前线程的快照指针（推荐使用的名字）。
///
/// 每次调用都会刷新快照，指针在下次调用前保持有效，因此请先读取或拷贝再调用。
/// 不会返回 null；所有有效位为 0 只表示当前没有数据。
#[unsafe(no_mangle)]
pub extern "C" fn loong_snapshot() -> *const RfSnapshot {
    {
        let ctx = Context::lock();
        SNAPSHOT.with(|slot| {
            let mut snapshot = slot.borrow_mut();
            fill_snapshot(&mut snapshot, &ctx);
        });
    }
    SNAPSHOT.with(|slot| slot.as_ptr())
}

/// loong_snapshot 的旧名别名，供既有 ACR 代码继续使用。
#[unsafe(no_mangle)]
pub extern "C" fn reffect_snapshot() -> *const RfSnapshot {
    loong_snapshot()
}

/// 返回 RfSnapshot 的字节大小，供调用方自行分配缓冲区。
#[unsafe(no_mangle)]
pub extern "C" fn loong_snapshot_size() -> usize {
    std::mem::size_of::<RfSnapshot>()
}

/// loong_snapshot_size 的旧名别名。
#[unsafe(no_mangle)]
pub extern "C" fn reffect_snapshot_size() -> usize {
    loong_snapshot_size()
}
