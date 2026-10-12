mod ui;

use crate::{
    ADDON_DISPLAY_NAME,
    context::Context,
    internal::{Interface, Internal},
    links::Links,
    worker::StoppableWorker,
};
use nexus::gui::{RenderType, register_render, render};
use std::sync::{Mutex, MutexGuard, OnceLock};

static ADDON: OnceLock<Mutex<Addon>> = OnceLock::new();

/// 插件运行时状态。只保留读取与调试所需的最小字段。
#[derive(Debug)]
pub struct Addon {
    debug: bool,
    links: Links,
    worker: Option<StoppableWorker>,
}

impl Addon {
    /// 编译版本号，Cargo 版本号即日期（例如 2026.10.13）。
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");

    pub fn new() -> Self {
        Self {
            debug: false,
            links: Links::load(),
            worker: None,
        }
    }

    pub fn lock() -> MutexGuard<'static, Addon> {
        ADDON
            .get_or_init(|| Mutex::new(Addon::new()))
            .lock()
            .unwrap()
    }

    pub fn release(&mut self) {
        *self = Self::new();
    }

    pub fn load() {
        log::info!("{ADDON_DISPLAY_NAME} v{} 加载", Self::VERSION);

        register_render(RenderType::Render, render!(|ui| Addon::lock().render(ui)))
            .revert_on_unload();

        register_render(
            RenderType::OptionsRender,
            render!(|ui| Addon::lock().render_options(ui)),
        )
        .revert_on_unload();

        // 先启动内存读取器，再启动上下文线程（顺序由 loong_internal 的测试固定）。
        Internal::init();

        let mut addon = Self::lock();
        addon.worker = Context::create_worker(addon.links.clone());
    }

    pub fn unload() {
        log::info!("{ADDON_DISPLAY_NAME} v{} 卸载", Self::VERSION);

        let mut addon = Self::lock();

        Internal::deinit();

        if let Some(worker) = addon.worker.take() {
            worker.exit_and_wait();
        }

        addon.release();
        Context::unload();
    }
}
