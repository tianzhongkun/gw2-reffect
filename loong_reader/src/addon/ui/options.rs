use super::Addon;
use crate::metadata::{BUILD_TIME, COMMIT, RUSTC};
use nexus::imgui::Ui;

impl Addon {
    /// Nexus 选项窗口。这里只保留版本信息与调试窗口开关。
    pub fn render_options(&mut self, ui: &Ui) {
        ui.text("Loong Reader");
        ui.same_line();
        copyable_text(
            ui,
            format!(
                "{} ({COMMIT} {})",
                Self::VERSION,
                BUILD_TIME.format("%Y-%m-%d %H:%M UTC")
            ),
        );

        ui.text("作者");
        ui.same_line();
        copyable_text(ui, "loong");

        ui.text("编译工具链");
        ui.same_line();
        copyable_text(ui, RUSTC);

        ui.spacing();
        ui.checkbox("调试窗口", &mut self.debug);

        #[cfg(feature = "profile")]
        {
            use crate::profiling;

            let mut enabled = profiling::enabled();
            if ui.checkbox("性能分析", &mut enabled) {
                profiling::toggle(enabled);
            }
        }
    }
}

fn copyable_text(ui: &Ui, text: impl AsRef<str>) {
    let text = text.as_ref();
    ui.text(text);
    if ui.is_item_hovered() {
        ui.tooltip_text("点击复制");
    }
    if ui.is_item_clicked() {
        ui.set_clipboard_text(text);
    }
}
