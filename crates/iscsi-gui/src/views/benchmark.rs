use super::*;
use crate::state::BenchSettings;
use anyhow::{Context as _, Result};
use component::{
    IndexPath, Sizable,
    button::ButtonVariants,
    checkbox::Checkbox,
    input::{InputState, NumberInput},
    select::{Select, SelectState},
};
use gpui_kit::prelude::FluentBuilder;
use iscsi_core::BenchPattern;

const PATTERNS: [&str; 4] = ["顺序读", "随机读", "顺序写", "随机写"];

pub struct BenchForm {
    pub(super) pattern: Entity<SelectState<Vec<&'static str>>>,
    block: Entity<InputState>,
    queue: Entity<InputState>,
    seconds: Entity<InputState>,
    start: Entity<InputState>,
    range: Entity<InputState>,
    pub allow_write: bool,
}

impl BenchForm {
    pub fn set_connection_limits(
        &self,
        queue: usize,
        range: u128,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.queue.update(cx, |input, cx| {
            input.set_value(queue.min(32).to_string(), window, cx)
        });
        self.range.update(cx, |input, cx| {
            input.set_value(range.to_string(), window, cx)
        });
        self.start
            .update(cx, |input, cx| input.set_value("0", window, cx));
    }

    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            pattern: cx.new(|cx| {
                SelectState::new(PATTERNS.to_vec(), Some(IndexPath::default()), window, cx)
            }),
            block: editor::input("1024", window, cx),
            queue: editor::input("32", window, cx),
            seconds: editor::input("10", window, cx),
            start: editor::input("0", window, cx),
            range: editor::input("16384", window, cx),
            allow_write: false,
        }
    }

    fn pattern(&self, cx: &App) -> BenchPattern {
        match self
            .pattern
            .read(cx)
            .selected_index(cx)
            .map(|index| index.row)
        {
            Some(1) => BenchPattern::RandRead,
            Some(2) => BenchPattern::SeqWrite,
            Some(3) => BenchPattern::RandWrite,
            _ => BenchPattern::SeqRead,
        }
    }

    fn settings(&self, cx: &App) -> Result<BenchSettings> {
        Ok(BenchSettings {
            pattern: self.pattern(cx),
            block_kib: self
                .block
                .read(cx)
                .value()
                .parse()
                .context("块大小必须为正整数")?,
            queue_depth: self
                .queue
                .read(cx)
                .value()
                .parse()
                .context("队列深度必须为正整数")?,
            seconds: self
                .seconds
                .read(cx)
                .value()
                .parse()
                .context("时长必须为正整数")?,
            start_mib: self
                .start
                .read(cx)
                .value()
                .parse()
                .context("起始位置必须为非负整数")?,
            range_mib: self
                .range
                .read(cx)
                .value()
                .parse()
                .context("范围必须为正整数")?,
            allow_write: self.allow_write,
        })
    }
}

impl Desktop {
    pub(super) fn benchmark_view(&self, cx: &mut Context<Self>) -> Div {
        let running = self.bench.running();
        let write_locked = self.bench_form.pattern(cx).is_write() && !self.bench_form.allow_write;
        let form = &self.bench_form;
        let controls = [
            ("块大小 (KiB)", &form.block),
            ("队列深度", &form.queue),
            ("时长 (秒)", &form.seconds),
            ("起始位置 (MiB)", &form.start),
            ("范围 (MiB)", &form.range),
        ];
        v_flex()
            .gap_3()
            .child(heading("基准测试"))
            .child(
                div().w(px(220.)).child(field(
                    "模式",
                    Select::new(&form.pattern)
                        .small()
                        .accessibility_label("测试模式")
                        .disabled(running),
                    cx,
                )),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .children(controls.map(|(label, entity)| {
                        field(
                            label,
                            NumberInput::new(entity)
                                .small()
                                .accessibility_label(label)
                                .disabled(running),
                            cx,
                        )
                    })),
            )
            .when(write_locked, |panel| {
                panel.child(
                    Checkbox::new("allow-write")
                        .small()
                        .label("允许写入（会破坏 LUN 上的数据）")
                        .checked(form.allow_write)
                        .disabled(running)
                        .on_change(cx.listener(|this, checked, _, cx| {
                            this.bench_form.allow_write = *checked;
                            cx.notify();
                        })),
                )
            })
            .when(write_locked, |panel| {
                panel.child(
                    div()
                        .text_color(cx.theme().danger)
                        .child("写入已禁用：必须先勾选破坏性写入确认。"),
                )
            })
            .when(self.connection.is_none(), |panel| {
                panel.child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("尚未连接目标"),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("run-benchmark")
                            .small()
                            .primary()
                            .icon(assets::IconName::Play)
                            .label("运行")
                            .disabled(running || write_locked || self.connection.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.start_benchmark(cx))),
                    )
                    .child(
                        Button::new("cancel-benchmark")
                            .small()
                            .icon(assets::IconName::Square)
                            .label("取消")
                            .disabled(!running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.engine.cancel_benchmark();
                                this.log("请求取消测试；会话将断开");
                                cx.notify();
                            })),
                    ),
            )
            .children(operation_line("测试", &self.bench, cx))
            .child(self.benchmark_results(cx))
    }

    fn start_benchmark(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = &self.connection else {
            return;
        };
        let options = self.bench_form.settings(cx).and_then(|settings| {
            settings.options(connection.capacity, connection.session.queue_depth())
        });
        match options {
            Ok(options) => {
                self.bench = Operation::Running;
                self.report = None;
                self.history.clear();
                self.log("开始单连接基准测试");
                if let Some(connection) = &self.connection {
                    self.engine.benchmark(connection.session.clone(), options);
                }
            }
            Err(error) => self.bench = Operation::Failed(error.to_string()),
        }
        cx.notify();
    }

    fn benchmark_results(&self, cx: &App) -> Div {
        let mut results = v_flex().gap_3().child(heading("结果"));
        if let Some(report) = &self.report {
            for measurement in &report.results {
                results = results
                    .child(
                        h_flex()
                            .gap_6()
                            .child(metric(
                                "吞吐量",
                                format!("{:.2} MiB/s", measurement.mib_per_second),
                            ))
                            .child(metric("IOPS", format!("{:.2}", measurement.iops))),
                    )
                    .child(pair(
                        "p50 / p95 / p99",
                        format!(
                            "{:.2} / {:.2} / {:.2} ms",
                            measurement.p50_latency_us / 1000.,
                            measurement.p95_latency_us / 1000.,
                            measurement.p99_latency_us / 1000.
                        ),
                        cx,
                    ))
                    .child(pair(
                        "完成请求 / 测量时间",
                        format!(
                            "{} / {:.3} 秒",
                            measurement.commands, measurement.elapsed_seconds
                        ),
                        cx,
                    ))
                    .child(pair(
                        "队列深度 / 连接数",
                        format!("{} / 1", measurement.queue_depth),
                        cx,
                    ));
            }
            if report.flush_seconds > 0. {
                results = results.child(pair(
                    "缓存同步耗时",
                    format!("{:.3} 秒", report.flush_seconds),
                    cx,
                ));
            }
        } else {
            results = results.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("暂无完成结果"),
            );
        }
        results.children(self.sparkline(cx))
    }

    fn sparkline(&self, cx: &App) -> Option<Div> {
        if self.history.is_empty() {
            return None;
        }
        let peak = self.history.iter().copied().fold(1_f64, f64::max);
        let bars = (0..60).map(|index| {
            let sample = self.history.get(index).copied().unwrap_or(0.);
            div()
                .flex_1()
                .min_w_0()
                .h(px((sample / peak * 44.).max(1.) as f32))
                .bg(if index < self.history.len() {
                    cx.theme().primary
                } else {
                    cx.theme().muted
                })
        });
        Some(
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("吞吐量 · 最近 60 次采样 · 峰值 {peak:.1} MiB/s")),
                )
                .child(
                    h_flex()
                        .items_end()
                        .gap(px(2.))
                        .h(px(44.))
                        .w_full()
                        .overflow_hidden()
                        .children(bars),
                ),
        )
    }
}

pub(super) fn metric(label: &'static str, measurement: String) -> Div {
    v_flex()
        .min_w_0()
        .gap(px(2.))
        .child(div().text_xs().child(label))
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .child(measurement),
        )
}
