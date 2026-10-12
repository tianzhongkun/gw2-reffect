mod debug;
mod options;

use super::Addon;
use crate::context::Context;
use nexus::imgui::Ui;

impl Addon {
    /// 每帧渲染：刷新快速上下文（时间、挂载、游戏 UI 状态），并按需绘制调试窗口。
    pub fn render(&mut self, ui: &Ui) {
        let mut ctx = Context::lock();

        ctx.prepare_render(&self.links);

        if self.debug {
            self.render_debug(ui, &ctx);
        }

        ctx.reset_after_render();
    }
}
