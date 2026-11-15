use super::*;
use component::{Sizable, button::ButtonVariants};

impl Desktop {
    pub(super) fn session_view(&self, cx: &mut Context<Self>) -> Div {
        let mut panel = v_flex()
            .gap_3()
            .child(heading("会话"))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("connect")
                            .small()
                            .primary()
                            .icon(assets::IconName::Plug)
                            .label("连接")
                            .disabled(self.locked() || self.save.running() || self.load.running())
                            .on_click(cx.listener(|this, _, _, cx| this.start_connect(cx))),
                    )
                    .child(
                        Button::new("disconnect")
                            .small()
                            .icon(assets::IconName::Unplug)
                            .label("断开")
                            .disabled(
                                self.connection.is_none()
                                    || self.bench.running()
                                    || self.disconnect.running(),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(connection) = this.connection.take() {
                                    this.disconnect = Operation::Running;
                                    this.connection_generation = None;
                                    this.engine.disconnect(connection.session);
                                    this.iops = 0.;
                                    this.mib = 0.;
                                    this.bench_form.allow_write = false;
                                    cx.notify();
                                }
                            })),
                    ),
            )
            .children(operation_line("连接", &self.connect, cx))
            .children(operation_line("断开", &self.disconnect, cx))
            .child(
                h_flex()
                    .gap_6()
                    .child(super::benchmark::metric(
                        "吞吐量",
                        format!("{:.1} MiB/s", self.mib),
                    ))
                    .child(super::benchmark::metric(
                        "IOPS",
                        format!("{:.1}", self.iops),
                    ))
                    .child(super::benchmark::metric("错误", self.errors.to_string()))
                    .when(self.retries > 0, |row| {
                        row.child(super::benchmark::metric("重试", self.retries.to_string()))
                    }),
            );
        if let Some(connection) = &self.connection {
            let inquiry = &connection.inquiry;
            panel = panel
                .child(heading("逻辑单元"))
                .child(pair("厂商", inquiry.vendor.clone(), cx))
                .child(pair("产品", inquiry.product.clone(), cx))
                .child(pair("版本", inquiry.revision.clone(), cx))
                .child(pair(
                    "序列号",
                    inquiry.serial.clone().unwrap_or_else(|| "未提供".into()),
                    cx,
                ))
                .child(pair("VPD 0x83", shorten_vpd(&inquiry.vpd83), cx))
                .child(pair(
                    "容量",
                    format!(
                        "{:.2} GiB · {} × {} B",
                        connection.capacity.bytes() as f64 / crate::state::GIB as f64,
                        connection.capacity.blocks,
                        connection.capacity.block_size
                    ),
                    cx,
                ))
                .child(pair(
                    "写缓存",
                    if connection.cache.write_cache {
                        "目标启用易失缓存（WCE=1），掉电可能丢写"
                    } else {
                        "目标未启用易失缓存（WCE=0）"
                    },
                    cx,
                ))
                .child(pair(
                    "读缓存",
                    if connection.cache.read_cache_disabled {
                        "目标要求不缓存读（RCD=1）"
                    } else {
                        "允许客户端缓存读（RCD=0）"
                    },
                    cx,
                ))
                .child(heading("协商参数"));
            if let Ok(serde_json::Value::Object(parameters)) =
                serde_json::to_value(connection.session.negotiated())
            {
                panel = panel.children(parameters.into_iter().map(|(key, parameter)| {
                    pair(parameter_label(&key), parameter.to_string(), cx)
                }));
            }
        }
        panel.child(heading("活动日志")).child(
            v_flex().gap_1().children(
                self.log
                    .iter()
                    .rev()
                    .map(|line| div().text_xs().child(line.clone())),
            ),
        )
    }

    fn start_connect(&mut self, cx: &mut Context<Self>) {
        match self.editor.target(cx) {
            Ok(target) => {
                self.connect = Operation::Running;
                self.disconnect = Operation::Idle;
                self.report = None;
                self.history.clear();
                self.errors = 0;
                self.retries = 0;
                self.bench_form.allow_write = false;
                self.log("正在连接目标");
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
        "header_digest" => "头部摘要",
        "data_digest" => "数据摘要",
        "max_recv_data_segment_length" => "接收段上限",
        "target_max_recv_data_segment_length" => "目标接收段上限",
        "max_burst_length" => "最大突发长度",
        "first_burst_length" => "首突发长度",
        "initial_r2t" => "初始 R2T",
        "immediate_data" => "即时数据",
        "default_time2wait" => "Time2Wait",
        "default_time2retain" => "Time2Retain",
        "error_recovery_level" => "错误恢复级别",
        "max_connections" => "最大连接数",
        "max_outstanding_r2t" => "未完成 R2T 上限",
        other => return SharedString::from(other.to_owned()),
    }
    .into()
}
