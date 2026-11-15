mod benchmark;
mod editor;
mod session;
mod sidebar;
mod updates;

use crate::{
    engine::{Connection, Engine},
    state::{Config, Operation},
};
use editor::Editor;
use gpui_kit::{
    component::{ActiveTheme, Disableable, button::Button, h_flex, v_flex},
    prelude::FluentBuilder,
    *,
};
use iscsi_core::{BenchReport, DiscoveredTarget};
use std::{collections::VecDeque, time::Instant};
use tokio::runtime::Handle;

pub struct Desktop {
    engine: Engine,
    config: Config,
    selected: Option<usize>,
    editor: Editor,
    bench_form: benchmark::BenchForm,
    connection: Option<Connection>,
    connection_generation: Option<u64>,
    discovered: Vec<DiscoveredTarget>,
    discovery_portal: Option<iscsi_core::TargetAddr>,
    discovery_host: Entity<component::input::InputState>,
    load: Operation,
    save: Operation,
    edit: Operation,
    discovery: Operation,
    connect: Operation,
    disconnect: Operation,
    bench: Operation,
    tab: usize,
    iops: f64,
    mib: f64,
    errors: u64,
    retries: u64,
    history: VecDeque<f64>,
    report: Option<BenchReport>,
    log: VecDeque<String>,
    started: Instant,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Desktop {
    pub fn new(runtime: Handle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (mut engine, mut events) = Engine::new(runtime);
        engine.load();
        let bench_form = benchmark::BenchForm::new(window, cx);
        let subscriptions = vec![cx.observe(&bench_form.pattern, |_, _, cx| cx.notify())];
        let event_task = cx.spawn_in(window, async move |weak, cx| {
            while let Some(event) = events.recv().await {
                if weak
                    .update_in(cx, |this, window, cx| this.apply_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            engine,
            config: Config::default(),
            selected: None,
            editor: Editor::new(&crate::state::SavedTarget::default(), window, cx),
            bench_form,
            connection: None,
            connection_generation: None,
            discovered: Vec::new(),
            discovery_portal: None,
            discovery_host: editor::input("", window, cx),
            load: Operation::Running,
            save: Operation::Idle,
            edit: Operation::Idle,
            discovery: Operation::Idle,
            connect: Operation::Idle,
            disconnect: Operation::Idle,
            bench: Operation::Idle,
            tab: 0,
            iops: 0.,
            mib: 0.,
            errors: 0,
            retries: 0,
            history: VecDeque::new(),
            report: None,
            log: VecDeque::new(),
            started: Instant::now(),
            _events: event_task,
            _subscriptions: subscriptions,
        }
    }

    fn locked(&self) -> bool {
        self.connection.is_some()
            || self.connect.running()
            || self.disconnect.running()
            || self.bench.running()
    }

    fn log(&mut self, message: impl AsRef<str>) {
        if self.log.len() == 200 {
            self.log.pop_front();
        }
        self.log.push_back(format!(
            "{:>5}s  {}",
            self.started.elapsed().as_secs(),
            message.as_ref()
        ));
    }

    fn clear_connection(&mut self) {
        self.engine.stop_sampling();
        self.connection = None;
        self.connection_generation = None;
        self.iops = 0.;
        self.mib = 0.;
    }
}

impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.tab {
            1 => self.session_view(cx).into_any_element(),
            2 => self.benchmark_view(cx).into_any_element(),
            _ => self.editor_view(cx).into_any_element(),
        };
        let connected = self.connection.is_some();
        let mut tabs = h_flex().gap_1();
        for (index, label) in ["目标", "会话", "基准"].into_iter().enumerate() {
            let active = self.tab == index;
            tabs = tabs.child(
                div()
                    .id(("tab", index))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |tab| {
                        tab.bg(cx.theme().accent).text_color(cx.theme().foreground)
                    })
                    .when(!active, |tab| tab.text_color(cx.theme().muted_foreground))
                    .hover(|tab| tab.bg(cx.theme().accent))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = index;
                        cx.notify();
                    })),
            );
        }
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(12.))
            .child(
                h_flex()
                    .h(px(46.))
                    .pl(px(84.))
                    .pr_4()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("iSCSI"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("用户态客户端 · 不挂载磁盘"),
                    )
                    .child(div().flex_1())
                    .child(self.status_pill(connected, cx)),
            )
            .child(
                h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar_view(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(
                                h_flex()
                                    .px_4()
                                    .py(px(6.))
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(tabs),
                            )
                            .child(
                                div()
                                    .id("content")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .child(v_flex().p_4().gap_3().child(content)),
                            )
                            .child(self.status_bar(cx)),
                    ),
            )
    }
}

impl Desktop {
    fn status_pill(&self, connected: bool, cx: &Context<Self>) -> Div {
        let (color, text) = if connected {
            (cx.theme().success, "已连接")
        } else if self.connect.running() {
            (cx.theme().warning, "正在连接")
        } else if matches!(self.connect, Operation::Failed(_)) {
            (cx.theme().danger, "连接失败")
        } else {
            (cx.theme().muted_foreground, "未连接")
        };
        h_flex()
            .gap_1()
            .px_2()
            .py(px(3.))
            .rounded_full()
            .bg(cx.theme().accent)
            .text_xs()
            .text_color(cx.theme().foreground)
            .child(div().size(px(6.)).rounded_full().bg(color))
            .child(text)
    }

    fn status_bar(&self, cx: &Context<Self>) -> Div {
        h_flex()
            .h(px(26.))
            .px_3()
            .gap_4()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(if self.connection.is_some() {
                "单 TCP 连接 · 队列深度 32"
            } else {
                "空闲"
            })
            .child(div().flex_1())
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(format!("{:.1} MiB/s", self.mib)),
            )
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(format!("{:.1} IOPS", self.iops)),
            )
    }
}

/// Renders nothing while the operation is idle, so the panel stays quiet.
fn operation_line(label: &'static str, operation: &Operation, cx: &App) -> Option<Div> {
    (!matches!(operation, Operation::Idle)).then(|| {
        div()
            .text_xs()
            .text_color(if matches!(operation, Operation::Failed(_)) {
                cx.theme().danger
            } else {
                cx.theme().muted_foreground
            })
            .child(format!("{label}：{}", operation.label()))
    })
}

fn heading(title: &'static str) -> Div {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
}

fn field(label: &'static str, control: impl IntoElement, cx: &App) -> Div {
    v_flex()
        .min_w_0()
        .gap_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(div().child(label))
        .child(control)
}

fn pair(label: impl Into<SharedString>, text: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .items_start()
        .gap_3()
        .py(px(3.))
        .text_xs()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .text_color(cx.theme().muted_foreground)
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(text.into()))
}
