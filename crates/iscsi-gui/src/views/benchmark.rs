use super::*;
use crate::i18n::tr;
use crate::state::{BenchSettings, CacheSettings, read_cache_presets, write_back_presets};
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

pub struct BenchForm {
    pub(super) pattern: Entity<SelectState<Vec<&'static str>>>,
    block: Entity<InputState>,
    queue: Entity<InputState>,
    seconds: Entity<InputState>,
    start: Entity<InputState>,
    range: Entity<InputState>,
    pub allow_write: bool,
    pub read_cache: bool,
    pub write_back: bool,
    read_preset: Entity<SelectState<Vec<&'static str>>>,
    write_preset: Entity<SelectState<Vec<&'static str>>>,
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
                SelectState::new(
                    vec![
                        tr!("顺序读", "Sequential read"),
                        tr!("随机读", "Random read"),
                        tr!("顺序写", "Sequential write"),
                        tr!("随机写", "Random write"),
                    ],
                    Some(IndexPath::default()),
                    window,
                    cx,
                )
            }),
            block: editor::input("1024", window, cx),
            queue: editor::input("32", window, cx),
            seconds: editor::input("10", window, cx),
            start: editor::input("0", window, cx),
            range: editor::input("16384", window, cx),
            allow_write: false,
            read_cache: true,
            write_back: false,
            read_preset: cx.new(|cx| {
                SelectState::new(
                    read_cache_presets()
                        .iter()
                        .map(|preset| preset.label)
                        .collect(),
                    Some(IndexPath::default()),
                    window,
                    cx,
                )
            }),
            write_preset: cx.new(|cx| {
                SelectState::new(
                    write_back_presets()
                        .iter()
                        .map(|preset| preset.label)
                        .collect(),
                    Some(IndexPath::default()),
                    window,
                    cx,
                )
            }),
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
            block_kib: self.block.read(cx).value().parse().context(tr!(
                "块大小必须为正整数",
                "Block size must be a positive integer"
            ))?,
            queue_depth: self.queue.read(cx).value().parse().context(tr!(
                "队列深度必须为正整数",
                "Queue depth must be a positive integer"
            ))?,
            seconds: self.seconds.read(cx).value().parse().context(tr!(
                "时长必须为正整数",
                "Duration must be a positive integer"
            ))?,
            start_mib: self.start.read(cx).value().parse().context(tr!(
                "起始位置必须为非负整数",
                "Start offset must be a non-negative integer"
            ))?,
            range_mib: self
                .range
                .read(cx)
                .value()
                .parse()
                .context(tr!("范围必须为正整数", "Range must be a positive integer"))?,
            allow_write: self.allow_write,
        })
    }

    fn cache_config(&self, cx: &App) -> Result<iscsi_core::CacheConfig> {
        CacheSettings {
            read_enabled: self.read_cache,
            read_preset: selected_row(&self.read_preset, cx),
            write_back_enabled: self.write_back,
            write_preset: selected_row(&self.write_preset, cx),
        }
        .config(self.allow_write)
    }
}

fn selected_row(state: &Entity<SelectState<Vec<&'static str>>>, cx: &App) -> usize {
    state
        .read(cx)
        .selected_index(cx)
        .map_or(0, |index| index.row)
}

impl Desktop {
    pub(super) fn benchmark_view(&self, cx: &mut Context<Self>) -> Div {
        let running = self.bench.running() || self.disconnect.running() || self.closing;
        let needs_confirmation =
            self.bench_form.pattern(cx).is_write() || self.bench_form.write_back;
        let write_locked = needs_confirmation && !self.bench_form.allow_write;
        let form = &self.bench_form;
        let controls = [
            (tr!("块大小 (KiB)", "Block size (KiB)"), &form.block),
            (tr!("队列深度", "Queue depth"), &form.queue),
            (tr!("时长 (秒)", "Duration (s)"), &form.seconds),
            (tr!("起始位置 (MiB)", "Start offset (MiB)"), &form.start),
            (tr!("范围 (MiB)", "Range (MiB)"), &form.range),
        ];
        v_flex()
            .gap_3()
            .child(heading(tr!("基准测试", "Benchmark")))
            .child(
                div().w(px(220.)).child(field(
                    tr!("模式", "Pattern"),
                    Select::new(&form.pattern)
                        .small()
                        .accessibility_label(tr!("测试模式", "Benchmark pattern"))
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
            .child(
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_3()
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_1()
                            .child(
                                Checkbox::new("read-cache")
                                    .small()
                                    .label(tr!("读缓存", "Read cache"))
                                    .checked(form.read_cache)
                                    .disabled(running)
                                    .on_change(cx.listener(|this, checked, _, cx| {
                                        this.bench_form.read_cache = *checked;
                                        cx.notify();
                                    })),
                            )
                            .child(field(
                                tr!("读缓存容量", "Read cache capacity"),
                                Select::new(&form.read_preset)
                                    .small()
                                    .accessibility_label(tr!("读缓存容量", "Read cache capacity"))
                                    .disabled(running || !form.read_cache),
                                cx,
                            )),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_1()
                            .child(
                                Checkbox::new("write-back")
                                    .small()
                                    .label(tr!("写回缓存（危险）", "Write-back cache (dangerous)"))
                                    .checked(form.write_back)
                                    .disabled(running)
                                    .on_change(cx.listener(|this, checked, _, cx| {
                                        this.bench_form.write_back = *checked;
                                        cx.notify();
                                    })),
                            )
                            .child(field(
                                tr!("写回容量", "Write-back capacity"),
                                Select::new(&form.write_preset)
                                    .small()
                                    .accessibility_label(tr!("写回容量", "Write-back capacity"))
                                    .disabled(running || !form.write_back),
                                cx,
                            )),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!(
                        "自动容量：读 {} MiB（本机内存 1/8，128 MiB–2 GiB）；写回推荐 {} MiB",
                        "Automatic capacity: read {} MiB (1/8 of system memory, 128 MiB-2 GiB); recommended write-back {} MiB",
                        iscsi_core::auto_read_cache_bytes() / 1_048_576,
                        iscsi_core::recommended_write_back_bytes() / 1_048_576
                    )),
            )
            .when(form.write_back, |panel| {
                panel.child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(tr!("崩溃或断电会丢失未刷写的写入", "A crash or power loss will lose unflushed writes")),
                )
            })
            .when(needs_confirmation || form.allow_write, |panel| {
                panel.child(
                    Checkbox::new("allow-write")
                        .small()
                        .label(tr!("允许写入（会破坏 LUN 上的数据）", "Allow writes (destroys data on the LUN)"))
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
                        .child(tr!("写入已禁用：必须先勾选破坏性写入确认。", "Writes disabled: confirm destructive writes first.")),
                )
            })
            .when(self.connection.is_none(), |panel| {
                panel.child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(tr!("尚未连接目标", "No target connected")),
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
                            .label(tr!("运行", "Run"))
                            .disabled(running || write_locked || self.connection.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.start_benchmark(cx))),
                    )
                    .child(
                        Button::new("cancel-benchmark")
                            .small()
                            .icon(assets::IconName::Square)
                            .label(tr!("取消", "Cancel"))
                            .disabled(!running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.engine.cancel_benchmark();
                                this.log(tr!("请求取消测试；会话将断开", "Benchmark cancellation requested; the session will disconnect"));
                                cx.notify();
                            })),
                    ),
            )
            .children(operation_line(tr!("测试", "Benchmark"), &self.bench, cx))
            .child(self.benchmark_results(cx))
    }

    fn start_benchmark(&mut self, cx: &mut Context<Self>) {
        if self.bench.running() || self.disconnect.running() || self.closing {
            return;
        }
        let Some(connection) = &self.connection else {
            return;
        };
        let options = self.bench_form.settings(cx).and_then(|settings| {
            Ok((
                settings.options(connection.capacity, connection.session.queue_depth())?,
                self.bench_form.cache_config(cx)?,
            ))
        });
        match options {
            Ok((options, cache)) => {
                self.bench = Operation::Running;
                self.report = None;
                self.history.clear();
                self.log(tr!(
                    "开始单连接基准测试",
                    "Starting single-connection benchmark"
                ));
                if let Some(connection) = &self.connection {
                    self.engine
                        .benchmark(connection.session.clone(), options, cache);
                }
            }
            Err(error) => self.bench = Operation::Failed(error.to_string()),
        }
        cx.notify();
    }

    fn benchmark_results(&self, cx: &App) -> Div {
        let mut results = v_flex().gap_3().child(heading(tr!("结果", "Results")));
        if let Some(report) = &self.report {
            for measurement in &report.results {
                results = results
                    .child(
                        h_flex()
                            .gap_6()
                            .child(metric(
                                tr!("吞吐量", "Throughput"),
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
                        tr!("完成请求 / 测量时间", "Completed requests / elapsed time"),
                        tr!(
                            "{} / {:.3} 秒",
                            "{} / {:.3} s",
                            measurement.commands,
                            measurement.elapsed_seconds
                        ),
                        cx,
                    ))
                    .child(pair(
                        tr!("队列深度 / 连接数", "Queue depth / connections"),
                        format!("{} / 1", measurement.queue_depth),
                        cx,
                    ));
            }
            if report.flush_seconds > 0. {
                results = results.child(pair(
                    tr!("缓存同步耗时", "Cache sync time"),
                    tr!("{:.3} 秒", "{:.3} s", report.flush_seconds),
                    cx,
                ));
            }
        } else {
            results = results.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("暂无完成结果", "No completed results")),
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
                        .child(tr!(
                            "吞吐量 · 最近 60 次采样 · 峰值 {peak:.1} MiB/s",
                            "Throughput · Last 60 samples · Peak {peak:.1} MiB/s",
                            peak = peak
                        )),
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
