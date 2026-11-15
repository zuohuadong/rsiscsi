use super::*;
use crate::i18n::tr;
use crate::state::SavedTarget;
use anyhow::{Context as _, Result};
use component::{
    Sizable,
    button::ButtonVariants,
    checkbox::Checkbox,
    input::{Input, InputContentType, InputState, NumberInput},
};
use iscsi_core::{TargetAddr, TargetUrl};

#[derive(Clone, Copy)]
pub enum Field {
    Name,
    Host,
    Port,
    Iqn,
    Lun,
    User,
    Secret,
    Initiator,
    Queue,
}

pub struct Editor {
    inputs: [Entity<InputState>; 9],
    pub(super) is_new: bool,
    pub header_digest: bool,
    pub data_digest: bool,
}

impl Editor {
    pub fn new(target: &SavedTarget, window: &mut Window, cx: &mut App) -> Self {
        let texts = [
            target.name.clone(),
            target.host.clone(),
            target.port.to_string(),
            target.iqn.clone(),
            target.lun.to_string(),
            target.chap_user.clone(),
            target.chap_secret.clone(),
            target.initiator_name.clone(),
            target.queue_depth.to_string(),
        ];
        let mut index = 0;
        let inputs = texts.map(|text| {
            let masked = index == Field::Secret as usize;
            index += 1;
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(text)
                    .masked(masked)
            })
        });
        Self {
            inputs,
            is_new: false,
            header_digest: target.header_digest,
            data_digest: target.data_digest,
        }
    }

    pub(super) fn new_target(window: &mut Window, cx: &mut App) -> Self {
        Self {
            is_new: true,
            ..Self::new(&SavedTarget::default(), window, cx)
        }
    }

    pub(super) fn focus_host(&mut self, window: &mut Window, cx: &mut App) {
        window.focus(&self.inputs[Field::Host as usize].focus_handle(cx), cx);
    }

    pub(super) fn has_input(&self, cx: &App) -> bool {
        let defaults = SavedTarget::default();
        // Ignore prefilled defaults when checking for input that would be discarded.
        [Field::Host, Field::Iqn, Field::User, Field::Secret]
            .into_iter()
            .any(|field| !self.text(field, cx).is_empty())
            || self.text(Field::Name, cx) != defaults.name
            || self.text(Field::Initiator, cx) != defaults.initiator_name
            || self.text(Field::Port, cx) != defaults.port.to_string()
            || self.text(Field::Lun, cx) != defaults.lun.to_string()
            || self.text(Field::Queue, cx) != defaults.queue_depth.to_string()
            || self.header_digest
            || self.data_digest
    }

    pub fn text(&self, field: Field, cx: &App) -> String {
        self.inputs[field as usize].read(cx).value().to_string()
    }

    pub fn target(&self, cx: &App) -> Result<SavedTarget> {
        let target = SavedTarget {
            name: self.text(Field::Name, cx),
            host: self.text(Field::Host, cx),
            port: self
                .text(Field::Port, cx)
                .parse()
                .context(tr!("端口必须为整数", "Port must be an integer"))?,
            iqn: self.text(Field::Iqn, cx),
            lun: self.text(Field::Lun, cx).parse().context(tr!(
                "LUN 必须为非负整数",
                "LUN must be a non-negative integer"
            ))?,
            chap_user: self.text(Field::User, cx),
            chap_secret: self.text(Field::Secret, cx),
            initiator_name: self.text(Field::Initiator, cx),
            queue_depth: self
                .text(Field::Queue, cx)
                .parse()
                .context(tr!("队列深度必须为整数", "Queue depth must be an integer"))?,
            header_digest: self.header_digest,
            data_digest: self.data_digest,
        };
        target.validate()?;
        Ok(target)
    }

    fn control(&self, field: Field, disabled: bool, label: &'static str) -> AnyElement {
        let entity = &self.inputs[field as usize];
        match field {
            Field::Port | Field::Lun | Field::Queue => NumberInput::new(entity)
                .small()
                .accessibility_label(label)
                .disabled(disabled)
                .into_any_element(),
            Field::Secret => Input::new(entity)
                .small()
                .aria_label(label)
                .content_type(InputContentType::Password)
                .disabled(disabled)
                .into_any_element(),
            _ => Input::new(entity)
                .small()
                .aria_label(label)
                .disabled(disabled)
                .into_any_element(),
        }
    }
}

pub fn input(text: &str, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).default_value(text.to_owned()))
}

impl Desktop {
    pub(super) fn editor_view(&self, cx: &mut Context<Self>) -> Div {
        let disabled = self.locked() || self.save.running() || self.load.running();
        let controls = [
            (tr!("名称", "Name"), Field::Name),
            (tr!("主机", "Host"), Field::Host),
            (tr!("端口", "Port"), Field::Port),
            ("IQN", Field::Iqn),
            ("LUN", Field::Lun),
            (tr!("CHAP 用户", "CHAP user"), Field::User),
            (tr!("CHAP 密码", "CHAP password"), Field::Secret),
            (tr!("发起端名称", "Initiator name"), Field::Initiator),
            (tr!("连接队列深度", "Connection queue depth"), Field::Queue),
        ];
        v_flex()
            .gap_3()
            .child(heading(if self.editor.is_new {
                tr!("新增目标", "New target")
            } else {
                tr!("编辑目标", "Edit target")
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("paste")
                            .small()
                            .icon(assets::IconName::ClipboardPaste)
                            .label(tr!("粘贴 URL", "Paste URL"))
                            .disabled(disabled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.paste_url(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("save")
                            .small()
                            .primary()
                            .icon(assets::IconName::Save)
                            .label(tr!("保存目标", "Save target"))
                            .disabled(disabled || matches!(self.load, Operation::Failed(_)))
                            .on_click(cx.listener(|this, _, _, cx| this.save_target(cx))),
                    ),
            )
            .children(operation_line(tr!("载入", "Load"), &self.load, cx))
            .children(operation_line(tr!("导入", "Import"), &self.edit, cx))
            .children(operation_line(tr!("保存", "Save"), &self.save, cx))
            .child(
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_3()
                    .children(controls.map(|(label, field_id)| {
                        field(label, self.editor.control(field_id, disabled, label), cx)
                    })),
            )
            .child(
                h_flex()
                    .gap_5()
                    .child(
                        Checkbox::new("header-digest")
                            .small()
                            .label(tr!("头部 CRC32C", "Header CRC32C"))
                            .checked(self.editor.header_digest)
                            .disabled(disabled)
                            .on_change(cx.listener(|this, checked, _, cx| {
                                this.editor.header_digest = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("data-digest")
                            .small()
                            .label(tr!("数据 CRC32C", "Data CRC32C"))
                            .checked(self.editor.data_digest)
                            .disabled(disabled)
                            .on_change(cx.listener(|this, checked, _, cx| {
                                this.editor.data_digest = *checked;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn paste_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fallback = TargetAddr {
            host: self.editor.text(Field::Host, cx),
            port: self.editor.text(Field::Port, cx).parse().unwrap_or(3260),
        };
        let imported = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text());
        let parsed = imported
            .as_deref()
            .ok_or(tr!("剪贴板没有 URL", "No URL in clipboard"))
            .and_then(|text| {
                TargetUrl::parse(text, Some(&fallback)).map_err(|_| {
                    tr!(
                        "URL 无效；原配置未改动",
                        "Invalid URL; original configuration unchanged"
                    )
                })
            });
        match parsed {
            Ok(url) => {
                let target = url.into_target();
                let chap = target.chap.unwrap_or(iscsi_core::ChapCredentials {
                    user: String::new(),
                    secret: String::new(),
                });
                let saved = SavedTarget {
                    host: target.addr.host,
                    port: target.addr.port,
                    iqn: target.iqn,
                    lun: target.lun,
                    chap_user: chap.user,
                    chap_secret: chap.secret,
                    name: self.editor.text(Field::Name, cx),
                    initiator_name: self.editor.text(Field::Initiator, cx),
                    queue_depth: self.editor.text(Field::Queue, cx).parse().unwrap_or(32),
                    header_digest: self.editor.header_digest,
                    data_digest: self.editor.data_digest,
                };
                let is_new = self.editor.is_new;
                self.editor = Editor::new(&saved, window, cx);
                self.editor.is_new = is_new;
                self.edit = Operation::Idle;
                self.log(tr!("已导入目标 URL", "Target URL imported"));
            }
            Err(message) => self.edit = Operation::Failed(message.into()),
        }
        cx.notify();
    }

    fn save_target(&mut self, cx: &mut Context<Self>) {
        match self.editor.target(cx) {
            Ok(target) => {
                let mut config = self.config.clone();
                match self
                    .selected
                    .and_then(|index| config.targets.get_mut(index))
                {
                    Some(saved) => *saved = target,
                    None => config.targets.push(target),
                }
                self.save = Operation::Running;
                self.engine.save(config);
            }
            Err(error) => self.save = Operation::Failed(error.to_string()),
        }
        cx.notify();
    }
}
