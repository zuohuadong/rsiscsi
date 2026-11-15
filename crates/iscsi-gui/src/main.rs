use gpui_kit::*;
use gpui_kit::component::{ActiveTheme, v_flex};

struct Desktop;

impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().p_6()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child("iSCSI · 目标管理")
            .child(gpui_kit::component::button::Button::new("close")
                .label("关闭").on_click(|_, window, _| window.remove_window()))
    }
}

fn main() {
    application().run(|cx| {
        gpui_kit::init(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        }).detach();
        if let Err(error) = open_window(WindowOptions::default(), cx, |window, cx| {
            window.set_window_title("iscsi-rs");
            cx.new(|_| Desktop)
        }) {
            eprintln!("无法打开窗口：{error}");
            cx.quit();
        }
        cx.activate(true);
    });
}
