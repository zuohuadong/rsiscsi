mod engine;
mod state;
mod views;

use gpui_kit::*;
use std::{process::ExitCode, time::Duration};
use tokio::runtime::Runtime;

fn main() -> ExitCode {
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("无法启动异步运行时：{error}");
            return ExitCode::FAILURE;
        }
    };
    let handle = runtime.handle().clone();
    application().with_assets(assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        component::set_locale("zh-CN");
        theme::install(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(900.), px(620.)),
                cx,
            ))),
            window_min_size: Some(size(px(760.), px(540.))),
            titlebar: Some(TitlebarOptions {
                title: Some("iSCSI".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(17.))),
            }),
            ..WindowOptions::default()
        };
        if let Err(error) = open_window(options, cx, |window, cx| {
            window.set_window_title("iSCSI");
            cx.new(|cx| views::Desktop::new(handle, window, cx))
        }) {
            eprintln!("无法打开窗口：{error}");
            cx.quit();
        }
        cx.activate(true);
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    ExitCode::SUCCESS
}

/// Dark, near-monochrome palette in the spirit of Volt Native.
mod theme {
    use gpui_kit::component::{Theme, ThemeMode};
    use gpui_kit::*;

    pub fn install(cx: &mut App) {
        Theme::change(ThemeMode::Dark, None, cx);
        let theme = Theme::global_mut(cx);
        theme.font_size = px(13.);
        theme.radius = px(6.);
        theme.radius_lg = px(10.);
        theme.background = hsla(240., 0.03, 0.075, 1.);
        theme.foreground = hsla(240., 0.06, 0.93, 1.);
        theme.border = hsla(240., 0.04, 0.18, 1.);
        theme.muted = hsla(240., 0.04, 0.22, 1.);
        theme.muted_foreground = hsla(240., 0.04, 0.56, 1.);
        theme.accent = hsla(240., 0.04, 0.16, 1.);
        theme.accent_foreground = hsla(240., 0.06, 0.93, 1.);
        theme.secondary = hsla(240., 0.04, 0.19, 1.);
        theme.secondary_foreground = hsla(240., 0.06, 0.93, 1.);
        theme.primary = hsla(240., 0.06, 0.90, 1.);
        theme.primary_hover = hsla(240., 0.06, 0.82, 1.);
        theme.primary_active = hsla(240., 0.06, 0.75, 1.);
        theme.primary_foreground = hsla(240., 0.03, 0.10, 1.);
        theme.popover = hsla(240., 0.03, 0.10, 1.);
        theme.popover_foreground = theme.foreground;
        theme.input = hsla(240., 0.04, 0.15, 1.);
        theme.ring = hsla(240., 0.05, 0.42, 1.);
        theme.selection = hsla(240., 0.05, 0.26, 1.);
        theme.sidebar = hsla(240., 0.03, 0.055, 1.);
        theme.sidebar_foreground = theme.foreground;
        theme.sidebar_accent = hsla(240., 0.04, 0.15, 1.);
        theme.sidebar_border = theme.border;
        theme.success = hsla(145., 0.45, 0.52, 1.);
        theme.warning = hsla(38., 0.75, 0.55, 1.);
        theme.danger = hsla(0., 0.52, 0.55, 1.);
    }
}
