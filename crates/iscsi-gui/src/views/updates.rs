use super::*;
use crate::engine::Event;

impl Desktop {
    pub(super) fn apply_event(
        &mut self,
        event: Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            Event::Loaded(outcome) => self.config_loaded(outcome, window, cx),
            Event::Saved(outcome) => self.config_saved(outcome),
            Event::Discovered(outcome) => match outcome {
                Ok(targets) => {
                    self.discovery = Operation::Idle;
                    self.log(format!("发现 {} 个目标", targets.len()));
                    self.discovered = targets;
                }
                Err(message) => self.discovery = Operation::Failed(message),
            },
            Event::Connected(outcome) => self.connected(outcome, window, cx),
            Event::Disconnected(outcome) => {
                self.disconnect = outcome.err().map(Operation::Failed).unwrap_or_default();
                self.log("会话已断开");
            }
            Event::Rates {
                generation,
                iops,
                mib,
                errors,
            } if self.connection_generation == Some(generation) => {
                self.iops = iops;
                self.mib = mib;
                self.errors = errors;
                if self.bench.running() {
                    if self.history.len() == 60 {
                        self.history.pop_front();
                    }
                    self.history.push_back(mib);
                }
            }
            Event::Rates { .. } => {}
            Event::ConnectionClosed(generation)
                if self.connection_generation == Some(generation) && !self.bench.running() =>
            {
                self.clear_connection();
                self.connect = Operation::Failed("连接已终止，请重新连接".into());
                self.bench_form.allow_write = false;
                self.log("连接已终止");
            }
            Event::ConnectionClosed(_) => {}
            Event::BenchFinished(outcome) => self.benchmark_finished(outcome),
        }
        cx.notify();
    }

    fn config_loaded(
        &mut self,
        outcome: Result<Config, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(config) => {
                if let Some(target) = config.targets.first() {
                    self.editor = Editor::new(target, window, cx);
                    self.selected = Some(0);
                }
                self.config = config;
                self.load = Operation::Idle;
                self.log("目标配置已载入");
            }
            Err(message) => self.load = Operation::Failed(message),
        }
    }

    fn config_saved(&mut self, outcome: Result<(Config, Option<String>), String>) {
        match outcome {
            Ok((config, warning)) => {
                if self.selected.is_none() {
                    self.selected = config.targets.len().checked_sub(1);
                }
                self.config = config;
                self.save = warning.map(Operation::Failed).unwrap_or_default();
                self.log("目标配置已原子保存");
            }
            Err(message) => self.save = Operation::Failed(message),
        }
    }

    fn connected(
        &mut self,
        outcome: Result<Connection, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(connection) => {
                self.connect = Operation::Idle;
                self.connection_generation = Some(self.engine.sample(&connection.session));
                let bounded_mib = (connection.capacity.bytes() / 1_048_576).min(16384);
                // Small LUNs receive an explicit bounded range as well.
                self.bench_form.set_connection_limits(
                    connection.session.queue_depth(),
                    bounded_mib,
                    window,
                    cx,
                );
                self.connection = Some(connection);
                self.log("连接成功；设备信息已读取");
            }
            Err(message) => {
                self.log("连接失败");
                self.connect = Operation::Failed(message);
            }
        }
    }

    fn benchmark_finished(&mut self, outcome: Result<Option<BenchReport>, String>) {
        self.bench_form.allow_write = false;
        match outcome {
            Ok(Some(report)) => {
                self.log("基准测试完成");
                self.report = Some(report);
                self.bench = Operation::Idle;
            }
            Ok(None) => {
                self.clear_connection();
                self.log("测试已取消，会话已断开；已发送写入无法撤回");
                self.bench = Operation::Idle;
            }
            Err(message) => {
                self.clear_connection();
                self.log("测试失败，会话已断开");
                self.bench = Operation::Failed(message);
            }
        }
    }
}
