mod benchmark;
mod editor;
mod session;
mod sidebar;
mod updates;

use crate::i18n::tr;
use crate::{
    disks::DiskEntry,
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
    closing: bool,
    pending_target: Option<usize>,
    last_cache_error: Option<String>,
    known_disks: Vec<String>,
    disks: Vec<DiskEntry>,
    disks_error: Option<String>,
    disks_scanning: bool,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Desktop {
    pub fn new(
        runtime: Handle,
        mut quit_requests: tokio::sync::mpsc::UnboundedReceiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| this.request_close(window, cx))
                .unwrap_or(false)
        });
        App::on_action(cx, |_: &crate::Quit, _| crate::termination::request());
        let (mut engine, mut events) = Engine::new(runtime);
        engine.load();
        let bench_form = benchmark::BenchForm::new(window, cx);
        let subscriptions = vec![cx.observe(&bench_form.pattern, |_, _, cx| cx.notify())];
        let event_task = cx.spawn_in(window, async move |weak, cx| {
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let Some(event) = event else { break };
                        if weak.update_in(cx, |this, window, cx| this.apply_event(event, window, cx)).is_err() { break; }
                    }
                    quit = quit_requests.recv() => {
                        if quit.is_none() { break; }
                        if weak.update_in(cx, |this, window, cx| {
                            if this.request_close(window, cx) { window.remove_window(); }
                        }).is_err() { break; }
                    }
                }
            }
        });
        Self {
            engine,
            config: Config::default(),
            selected: None,
            editor: Editor::new_target(window, cx),
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
            closing: false,
            pending_target: None,
            last_cache_error: None,
            known_disks: crate::disks::whole_disks().unwrap_or_default(),
            disks: Vec::new(),
            disks_error: None,
            disks_scanning: false,
            _events: event_task,
            _subscriptions: subscriptions,
        }
    }

    fn locked(&self) -> bool {
        self.connection.is_some()
            || self.connect.running()
            || self.disconnect.running()
            || self.bench.running()
            || self.closing
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
        self.bench_form.allow_write = false;
        self.bench_form.write_back = false;
        self.last_cache_error = None;
        self.disks.clear();
        self.disks_error = None;
        self.disks_scanning = false;
    }

    fn refresh_disks(&mut self, cx: &mut Context<Self>) {
        self.disks_scanning = true;
        self.disks_error = None;
        self.engine.scan_disks(self.known_disks.clone());
        cx.notify();
    }

    fn start_disconnect(&mut self, cx: &mut Context<Self>) {
        if self.bench.running() || self.disconnect.running() {
            return;
        }
        if let Some(connection) = &self.connection {
            let session = connection.session.clone();
            self.disconnect = Operation::Running;
            self.log(tr!(
                "正在刷写并断开会话",
                "Flushing and disconnecting session"
            ));
            self.engine.disconnect(session);
            cx.notify();
        }
    }

    fn request_close(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.connection.is_none() && !self.connect.running() && !self.bench.running() {
            return true;
        }
        self.closing = true;
        self.pending_target = None;
        if self.bench.running() {
            self.engine.cancel_benchmark();
            self.log(tr!(
                "退出前正在停止测试并刷写",
                "Stopping benchmark and flushing before quitting"
            ));
        } else {
            self.start_disconnect(cx);
        }
        cx.notify();
        false
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
        for (index, label) in [
            tr!("目标", "Target"),
            tr!("会话", "Session"),
            tr!("基准", "Benchmark"),
        ]
        .into_iter()
        .enumerate()
        {
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
            .key_context("iSCSI")
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
                            .child(tr!(
                                "用户态客户端 · 不挂载磁盘",
                                "User-space client · No disk mounting"
                            )),
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
            (cx.theme().success, tr!("已连接", "Connected"))
        } else if self.connect.running() {
            (cx.theme().warning, tr!("正在连接", "Connecting"))
        } else if matches!(self.connect, Operation::Failed(_)) {
            (cx.theme().danger, tr!("连接失败", "Connection failed"))
        } else {
            (cx.theme().muted_foreground, tr!("未连接", "Not connected"))
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
                tr!(
                    "单 TCP 连接 · 队列深度 32",
                    "Single TCP connection · Queue depth 32"
                )
            } else {
                tr!("空闲", "Idle")
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
            .child(tr!(
                "{label}：{}",
                "{label}: {}",
                operation.label(),
                label = label
            ))
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
