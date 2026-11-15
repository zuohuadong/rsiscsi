use super::*;
use crate::i18n::tr;
use component::{Sizable, button::ButtonVariants};

impl Desktop {
    pub(super) fn session_view(&self, cx: &mut Context<Self>) -> Div {
        let mut panel = v_flex()
            .gap_3()
            .child(heading(tr!("会话", "Session")))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("connect")
                            .small()
                            .primary()
                            .icon(assets::IconName::Plug)
                            .label(tr!("连接", "Connect"))
                            .disabled(self.locked() || self.save.running() || self.load.running())
                            .on_click(cx.listener(|this, _, _, cx| this.start_connect(cx))),
                    )
                    .child(
                        Button::new("disconnect")
                            .small()
                            .icon(assets::IconName::Unplug)
                            .label(tr!("断开", "Disconnect"))
                            .disabled(
                                self.connection.is_none()
                                    || self.bench.running()
                                    || self.disconnect.running(),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.start_disconnect(cx))),
                    ),
            )
            .children(operation_line(tr!("连接", "Connect"), &self.connect, cx))
            .children(operation_line(
                tr!("断开", "Disconnect"),
                &self.disconnect,
                cx,
            ))
            .child(
                h_flex()
                    .gap_6()
                    .child(super::benchmark::metric(
                        tr!("吞吐量", "Throughput"),
                        format!("{:.1} MiB/s", self.mib),
                    ))
                    .child(super::benchmark::metric(
                        "IOPS",
                        format!("{:.1}", self.iops),
                    ))
                    .child(super::benchmark::metric(
                        tr!("错误", "Errors"),
                        self.errors.to_string(),
                    ))
                    .when(self.retries > 0, |row| {
                        row.child(super::benchmark::metric(
                            tr!("重试", "Retries"),
                            self.retries.to_string(),
                        ))
                    }),
            );
        if let Some(connection) = &self.connection {
            let cache = connection.session.cache_config();
            if cache.enabled() {
                let stats = connection.session.cache_stats();
                panel = panel
                    .child(pair(
                        tr!("本地读缓存", "Local read cache"),
                        tr!(
                            "{} MiB · 命中 {:.2}%",
                            "{} MiB · Hit rate {:.2}%",
                            cache.read_capacity_bytes / 1_048_576,
                            stats.hit_rate() * 100.
                        ),
                        cx,
                    ))
                    .child(pair(
                        tr!("本地写回缓存", "Local write-back cache"),
                        if cache.write_back_capacity_bytes > 0 {
                            tr!(
                                "{} MiB · 脏数据 {} B",
                                "{} MiB · Dirty data {} B",
                                cache.write_back_capacity_bytes / 1_048_576,
                                stats.dirty_bytes
                            )
                        } else {
                            tr!("关闭（直写）", "Off (write-through)").into()
                        },
                        cx,
                    ));
            }
            panel = panel.child(self.disk_section(cx));
            let inquiry = &connection.inquiry;
            panel = panel
                .child(heading(tr!("逻辑单元", "Logical unit")))
                .child(pair(tr!("厂商", "Vendor"), inquiry.vendor.clone(), cx))
                .child(pair(tr!("产品", "Product"), inquiry.product.clone(), cx))
                .child(pair(tr!("版本", "Revision"), inquiry.revision.clone(), cx))
                .child(pair(
                    tr!("序列号", "Serial number"),
                    inquiry
                        .serial
                        .clone()
                        .unwrap_or_else(|| tr!("未提供", "Not provided").into()),
                    cx,
                ))
                .child(pair("VPD 0x83", shorten_vpd(&inquiry.vpd83), cx))
                .child(pair(
                    tr!("容量", "Capacity"),
                    format!(
                        "{:.2} GiB · {} × {} B",
                        connection.capacity.bytes() as f64 / crate::state::GIB as f64,
                        connection.capacity.blocks,
                        connection.capacity.block_size
                    ),
                    cx,
                ))
                .child(pair(
                    tr!("写缓存", "Write cache"),
                    if connection.cache.write_cache {
                        tr!(
                            "目标启用易失缓存（WCE=1），掉电可能丢写",
                            "Target volatile cache enabled (WCE=1); power loss may lose writes"
                        )
                    } else {
                        tr!(
                            "目标未启用易失缓存（WCE=0）",
                            "Target volatile cache disabled (WCE=0)"
                        )
                    },
                    cx,
                ))
                .child(pair(
                    tr!("读缓存", "Read cache"),
                    if connection.cache.read_cache_disabled {
                        tr!(
                            "目标要求不缓存读（RCD=1）",
                            "Target requires uncached reads (RCD=1)"
                        )
                    } else {
                        tr!(
                            "允许客户端缓存读（RCD=0）",
                            "Client read caching allowed (RCD=0)"
                        )
                    },
                    cx,
                ))
                .child(heading(tr!("协商参数", "Negotiated parameters")));
            if let Ok(serde_json::Value::Object(parameters)) =
                serde_json::to_value(connection.session.negotiated())
            {
                panel = panel.children(parameters.into_iter().map(|(key, parameter)| {
                    pair(parameter_label(&key), parameter.to_string(), cx)
                }));
            }
        }
        panel.child(heading(tr!("活动日志", "Activity log"))).child(
            v_flex().gap_1().children(
                self.log
                    .iter()
                    .rev()
                    .map(|line| div().text_xs().child(line.clone())),
            ),
        )
    }

    fn disk_section(&self, cx: &mut Context<Self>) -> Div {
        let mut section = v_flex().gap_1().child(
            h_flex()
                .items_center()
                .gap_2()
                .child(heading(tr!("磁盘", "Disk")))
                .child(
                    Button::new("scan-disks")
                        .small()
                        .label(if self.disks_scanning {
                            tr!("扫描中…", "Scanning...")
                        } else {
                            tr!("刷新", "Refresh")
                        })
                        .disabled(self.disks_scanning)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_disks(cx))),
                ),
        );
        if let Some(error) = &self.disks_error {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        } else if self.disks.is_empty() {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("未发现新的块设备。本应用不创建磁盘；需 iSCSIKit 等 DriverKit 扩展把 LUN 挂成磁盘后才会出现在这里。", "No new block devices found. This app does not create disks; a DriverKit extension such as iSCSIKit must expose the LUN as a disk before it appears here.")),
            );
        }
        for (index, disk) in self.disks.iter().enumerate() {
            let mut row = v_flex().gap_1().py(px(4.)).child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .child(format!("{} · {}", disk.node(), disk.size_text())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if disk.mount_point().is_some() {
                                cx.theme().success
                            } else {
                                cx.theme().warning
                            })
                            .child(disk.status()),
                    ),
            );
            if let Some(mount) = disk.mount_point().cloned() {
                row = row.child(
                    Button::new(("finder", index))
                        .small()
                        .label(tr!("在 Finder 中打开", "Open in Finder"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(error) = crate::disks::reveal(&mount) {
                                this.log(tr!(
                                    "无法打开 Finder：{error}",
                                    "Failed to open Finder: {error}",
                                    error = error
                                ));
                            }
                            cx.notify();
                        })),
                );
            } else {
                row = row
                    .child(
                        Button::new(("disk-utility", index))
                            .small()
                            .label(tr!("打开磁盘工具", "Open Disk Utility"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Err(error) = crate::disks::open_disk_utility() {
                                    this.log(tr!(
                                        "无法打开磁盘工具：{error}",
                                        "Failed to open Disk Utility: {error}",
                                        error = error
                                    ));
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().text_xs().text_color(cx.theme().danger).child(tr!(
                        "初始化会清空该磁盘；本应用不会自动抹掉。",
                        "Initializing erases this disk; this app never erases it automatically."
                    )));
            }
            section = section.child(row);
        }
        section
    }

    fn start_connect(&mut self, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        match self.editor.target(cx) {
            Ok(target) => {
                self.connect = Operation::Running;
                self.disconnect = Operation::Idle;
                self.report = None;
                self.history.clear();
                self.errors = 0;
                self.retries = 0;
                self.bench_form.allow_write = false;
                self.log(tr!("正在连接目标", "Connecting to target"));
                self.engine.connect(target);
            }
            Err(error) => self.connect = Operation::Failed(error.to_string()),
        }
        cx.notify();
    }
}

/// The full identifier does not fit the compact table; keep a recognisable prefix.
fn shorten_vpd(bytes: &[u8]) -> String {
    let mut text = bytes
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if bytes.len() > 12 {
        text.push('…');
    }
    text
}

fn parameter_label(key: &str) -> SharedString {
    match key {
        "header_digest" => tr!("头部摘要", "Header digest"),
        "data_digest" => tr!("数据摘要", "Data digest"),
        "max_recv_data_segment_length" => tr!("接收段上限", "Max receive segment length"),
        "target_max_recv_data_segment_length" => {
            tr!("目标接收段上限", "Target max receive segment length")
        }
        "max_burst_length" => tr!("最大突发长度", "Max burst length"),
        "first_burst_length" => tr!("首突发长度", "First burst length"),
        "initial_r2t" => tr!("初始 R2T", "Initial R2T"),
        "immediate_data" => tr!("即时数据", "Immediate data"),
        "default_time2wait" => "Time2Wait",
        "default_time2retain" => "Time2Retain",
        "error_recovery_level" => tr!("错误恢复级别", "Error recovery level"),
        "max_connections" => tr!("最大连接数", "Max connections"),
        "max_outstanding_r2t" => tr!("未完成 R2T 上限", "Max outstanding R2T"),
        other => return SharedString::from(other.to_owned()),
    }
    .into()
}
