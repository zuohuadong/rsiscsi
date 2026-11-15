use super::*;
use crate::i18n::tr;
use crate::state::SavedTarget;
use component::{Sizable, input::Input};

impl Desktop {
    pub(super) fn sidebar_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let locked = self.locked() || self.save.running() || self.load.running();
        v_flex()
            .id("sidebar")
            .w(px(196.))
            .flex_shrink_0()
            .min_h_0()
            .overflow_y_scroll()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .child(caption(tr!("目标", "Target"), cx))
                    .when(self.config.targets.is_empty(), |list| {
                        list.child(
                            div()
                                .px_2()
                                .py_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(tr!("尚无保存的目标", "No saved targets")),
                        )
                    })
                    .children(
                        self.config
                            .targets
                            .iter()
                            .enumerate()
                            .map(|(index, target)| {
                                let selected = self.selected == Some(index);
                                let color = if selected && self.connection.is_some() {
                                    cx.theme().success
                                } else if selected && self.connect.running() {
                                    cx.theme().warning
                                } else if selected && matches!(self.connect, Operation::Failed(_)) {
                                    cx.theme().danger
                                } else {
                                    cx.theme().muted
                                };
                                h_flex()
                                    .id(("target", index))
                                    .gap_2()
                                    .px_2()
                                    .py(px(5.))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(selected, |row| row.bg(cx.theme().sidebar_accent))
                                    .when(!selected, |row| {
                                        row.hover(|row| row.bg(cx.theme().sidebar_accent))
                                    })
                                    .child(div().size(px(6.)).rounded_full().bg(color))
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap(px(1.))
                                            .child(div().truncate().child(target.name.clone()))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .truncate()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(format!(
                                                        "{}:{} · LUN {}",
                                                        target.host, target.port, target.lun
                                                    )),
                                            ),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select_target(index, window, cx);
                                    }))
                            }),
                    )
                    .child(
                        h_flex()
                            .id("new-target")
                            .gap_2()
                            .px_2()
                            .py(px(5.))
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|row| {
                                row.bg(cx.theme().sidebar_accent)
                                    .text_color(cx.theme().foreground)
                            })
                            .child(component::Icon::new(assets::IconName::Plus).xsmall())
                            .child(tr!("新增目标", "New target"))
                            .when(locked, |row| row.opacity(0.5))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.locked() || this.save.running() || this.load.running() {
                                    let reason = if this.closing {
                                        tr!("正在关闭，请稍后再新增目标", "Closing; wait before adding a target")
                                    } else if this.bench.running() {
                                        tr!("正在进行基准测试，请先停止测试并断开会话再新增目标", "Benchmark running; stop it and disconnect the session before adding a target")
                                    } else if this.disconnect.running() {
                                        tr!("正在断开会话，请稍后再新增目标", "Disconnecting session; wait before adding a target")
                                    } else if this.connect.running() {
                                        tr!("正在连接，请等待连接结束并断开会话再新增目标", "Connecting; wait for completion and disconnect the session before adding a target")
                                    } else if this.connection.is_some() {
                                        tr!("已连接，请先断开会话再新增目标", "Connected; disconnect the session before adding a target")
                                    } else if this.save.running() {
                                        tr!("正在保存目标，请稍后再新增目标", "Saving target; wait before adding a target")
                                    } else {
                                        tr!("正在载入目标，请稍后再新增目标", "Loading target; wait before adding a target")
                                    };
                                    this.log(reason);
                                    cx.notify();
                                    return;
                                }
                                if this.editor.has_input(cx) {
                                    this.log(tr!("已放弃未保存的编辑", "Unsaved edits discarded"));
                                }
                                this.selected = None;
                                this.editor = Editor::new_target(window, cx);
                                this.edit = Operation::Idle;
                                this.save = Operation::Idle;
                                this.tab = 0;
                                this.editor.focus_host(window, cx);
                                this.log(tr!("已新建目标表单", "New target form opened"));
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .mt_2()
                            .pt_2()
                            .gap_1()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(caption(tr!("发现目标", "Discover targets"), cx))
                            .child(
                                Input::new(&self.discovery_host)
                                    .aria_label(tr!("发现主机", "Discovery host"))
                                    .small()
                                    .disabled(self.discovery.running()),
                            )
                            .child(
                                Button::new("discover")
                                    .xsmall()
                                    .icon(assets::IconName::Search)
                                    .label(tr!("发现", "Discover"))
                                    .disabled(self.discovery.running() || self.load.running())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.start_discovery(cx)),
                                    ),
                            )
                            .children(operation_line(tr!("发现", "Discovery"), &self.discovery, cx))
                            .children(self.discovered.iter().enumerate().map(|(index, target)| {
                                v_flex()
                                    .gap_1()
                                    .py_2()
                                    .child(div().text_xs().truncate().child(target.iqn.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(target.addresses.join(", ")),
                                    )
                                    .child(
                                        Button::new(("use-discovery", index))
                                            .xsmall()
                                            .label(tr!("编辑此目标", "Edit this target"))
                                            .disabled(locked)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.use_discovered(index, window, cx)
                                            })),
                                    )
                            })),
                    ),
            )
    }

    pub(super) fn select_target(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.bench.running()
            || self.connect.running()
            || self.disconnect.running()
            || self.closing
        {
            return;
        }
        if self.connection.is_some() {
            self.pending_target = Some(index);
            self.start_disconnect(cx);
            return;
        }
        if let Some(target) = self.config.targets.get(index) {
            self.editor = Editor::new(target, window, cx);
            self.selected = Some(index);
            self.edit = Operation::Idle;
            self.save = Operation::Idle;
            self.connect = Operation::Idle;
            self.disconnect = Operation::Idle;
            self.report = None;
            self.history.clear();
            self.bench_form.allow_write = false;
            self.tab = 0;
            cx.notify();
        }
    }

    fn start_discovery(&mut self, cx: &mut Context<Self>) {
        let host = self.discovery_host.read(cx).value().to_string();
        if host.is_empty() || host.chars().any(char::is_control) {
            self.discovery =
                Operation::Failed(tr!("请填写发现主机", "Enter a discovery host").into());
        } else {
            let target = SavedTarget {
                host,
                chap_user: self.editor.text(editor::Field::User, cx),
                chap_secret: self.editor.text(editor::Field::Secret, cx),
                ..SavedTarget::default()
            };
            self.discovery_portal = Some(target.target().addr);
            self.discovered.clear();
            self.discovery = Operation::Running;
            self.engine.discover(target);
        }
        cx.notify();
    }

    fn use_discovered(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.discovered.get(index)
            && let Some(portal) = &self.discovery_portal
        {
            let saved = SavedTarget {
                host: portal.host.clone(),
                port: portal.port,
                iqn: target.iqn.clone(),
                chap_user: self.editor.text(editor::Field::User, cx),
                chap_secret: self.editor.text(editor::Field::Secret, cx),
                ..SavedTarget::default()
            };
            self.editor = Editor::new(&saved, window, cx);
            self.editor.is_new = true;
            self.selected = None;
            self.tab = 0;
            self.edit = Operation::Idle;
            cx.notify();
        }
    }
}

fn caption(text: &'static str, cx: &App) -> Div {
    div()
        .px_2()
        .text_size(px(10.5))
        .text_color(cx.theme().muted_foreground)
        .child(text)
}
