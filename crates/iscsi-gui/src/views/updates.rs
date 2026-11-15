use super::*;
use crate::{disks::DiskState, engine::Event};

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
            Event::Disconnected(outcome) => match outcome {
                Ok(()) => {
                    self.disconnect = Operation::Idle;
                    self.clear_connection();
                    self.log("会话已断开");
                    if self.closing {
                        window.remove_window();
                    } else if let Some(index) = self.pending_target.take() {
                        self.select_target(index, window, cx);
                    }
                }
                Err(message) => {
                    self.log(format!(
                        "刷写或断开失败：{message}；会话及脏数据已保留，请重试断开"
                    ));
                    self.disconnect = Operation::Failed(message);
                    self.closing = false;
                    self.pending_target = None;
                    self.tab = 1;
                    if let Some(connection) = &self.connection {
                        self.connection_generation = Some(self.engine.sample(&connection.session));
                    }
                }
            },
            Event::Rates {
                generation,
                iops,
                mib,
                errors,
                retries,
                cache_error,
            } if self.connection_generation == Some(generation) => {
                self.iops = iops;
                self.mib = mib;
                self.errors = errors;
                self.retries = retries;
                if cache_error != self.last_cache_error {
                    if let Some(error) = &cache_error {
                        self.log(format!("定时刷写失败：{error}；脏数据已保留"));
                    }
                    self.last_cache_error = cache_error;
                }
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
                let dirty = self
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.session.cache_stats().dirty_bytes > 0);
                if dirty {
                    self.engine.stop_sampling();
                    self.log("连接已终止，未刷写数据仍在内存；禁止换目标或退出");
                } else {
                    self.clear_connection();
                }
                self.connect = Operation::Failed("连接已终止，请重新连接".into());
                self.bench_form.allow_write = false;
                self.log("连接已终止");
            }
            Event::ConnectionClosed(_) => {}
            Event::BenchFinished(outcome) => {
                self.benchmark_finished(outcome);
                if self.closing {
                    if self.connection.is_some() {
                        self.start_disconnect(cx);
                    } else {
                        window.remove_window();
                    }
                }
            }
            Event::Disks(outcome) => {
                self.disks_scanning = false;
                match outcome {
                    Ok(disks) => {
                        for disk in &disks {
                            if let DiskState::Blank = disk.state {
                                self.log(format!(
                                    "{} 未初始化；请在磁盘工具中确认后抹掉",
                                    disk.node()
                                ));
                            }
                        }
                        if !disks.is_empty() {
                            self.log(format!("发现 {} 个新的块设备", disks.len()));
                        }
                        self.disks = disks;
                        self.disks_error = None;
                    }
                    Err(message) => {
                        self.log(format!("磁盘探测失败：{message}"));
                        self.disks_error = Some(message);
                    }
                }
            }
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
                self.refresh_disks(cx);
                if self.closing {
                    self.start_disconnect(cx);
                }
            }
            Err(message) => {
                self.log("连接失败");
                self.connect = Operation::Failed(message);
                if self.closing {
                    window.remove_window();
                }
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
                let retain = self.connection.as_ref().is_some_and(|connection| {
                    !connection.session.is_closed()
                        || connection.session.cache_stats().dirty_bytes > 0
                });
                if !retain {
                    self.clear_connection();
                }
                self.log(format!(
                    "测试或刷写失败：{message}；未刷写数据保留，请重试断开"
                ));
                self.closing = false;
                self.pending_target = None;
                self.tab = 1;
                self.bench = Operation::Failed(message);
            }
        }
    }
}
