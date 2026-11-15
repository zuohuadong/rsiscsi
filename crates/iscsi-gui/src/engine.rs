use crate::{
    disks::DiskEntry,
    state::{Config, SavedTarget, config_path, load_config, save_config},
};
use iscsi_core::{
    BenchOptions, BenchReport, BenchmarkCancellation, CacheConfig, CacheMode, Capacity,
    DiscoveredTarget, InquiryData, Session, benchmark_cancellable, discover_with_chap, login,
};
use std::{future::Future, time::Duration};
use tokio::{runtime::Handle, sync::mpsc, task::AbortHandle};

pub struct Connection {
    pub session: Session,
    pub inquiry: InquiryData,
    pub capacity: Capacity,
    pub cache: CacheMode,
}

pub enum Event {
    Loaded(Result<Config, String>),
    Saved(Result<(Config, Option<String>), String>),
    Discovered(Result<Vec<DiscoveredTarget>, String>),
    Connected(Result<Connection, String>),
    Disconnected(Result<(), String>),
    Rates {
        generation: u64,
        iops: f64,
        mib: f64,
        errors: u64,
        retries: u64,
        cache_error: Option<String>,
    },
    ConnectionClosed(u64),
    BenchFinished(Result<Option<BenchReport>, String>),
    Disks(Result<Vec<DiskEntry>, String>),
}

pub struct Engine {
    pub runtime: Handle,
    tx: mpsc::UnboundedSender<Event>,
    tasks: Vec<AbortHandle>,
    sampler: Option<AbortHandle>,
    cancel: Option<BenchmarkCancellation>,
    generation: u64,
}

impl Engine {
    pub fn new(runtime: Handle) -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                runtime,
                tx,
                tasks: Vec::new(),
                sampler: None,
                cancel: None,
                generation: 0,
            },
            rx,
        )
    }

    fn spawn(&mut self, future: impl Future<Output = Event> + Send + 'static) {
        self.tasks.retain(|task| !task.is_finished());
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
            let _ = tx.send(future.await);
        });
        self.tasks.push(task.abort_handle());
    }

    pub fn load(&mut self) {
        let runtime = self.runtime.clone();
        self.spawn(async move {
            let loaded = runtime
                .spawn_blocking(|| config_path().and_then(|path| load_config(&path)))
                .await;
            Event::Loaded(flatten(loaded))
        });
    }

    pub fn save(&mut self, config: Config) {
        let runtime = self.runtime.clone();
        self.spawn(async move {
            let saved = runtime
                .spawn_blocking(move || {
                    let warning = save_config(&config_path()?, &config)?;
                    Ok((config, warning))
                })
                .await;
            Event::Saved(flatten(saved))
        });
    }

    pub fn discover(&mut self, target: SavedTarget) {
        self.spawn(async move {
            let portal = target.target();
            Event::Discovered(
                discover_with_chap(&portal.addr, &target.options(), portal.chap.as_ref())
                    .await
                    .map_err(|error| error.to_string()),
            )
        });
    }

    pub fn connect(&mut self, target: SavedTarget) {
        self.spawn(async move {
            Event::Connected(connect(target).await.map_err(|error| error.to_string()))
        });
    }

    /// 只读扫描：找出连接后新出现的块设备。不会初始化或挂载任何东西。
    pub fn scan_disks(&mut self, known: Vec<String>) {
        let runtime = self.runtime.clone();
        self.spawn(async move {
            let found = runtime
                .spawn_blocking(move || crate::disks::discover(&known))
                .await;
            Event::Disks(flatten(found))
        });
    }

    pub fn sample(&mut self, session: &Session) -> u64 {
        self.stop_sampling();
        self.generation += 1;
        let generation = self.generation;
        let mut sampler = session.live_stats();
        let session = session.clone();
        let tx = self.tx.clone();
        self.sampler = Some(
            self.runtime
                .spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        if session.is_closed() {
                            let _ = tx.send(Event::ConnectionClosed(generation));
                            break;
                        }
                        let snapshot = sampler.sample();
                        if tx
                            .send(Event::Rates {
                                generation,
                                iops: sampler.iops,
                                mib: sampler.mib_per_second,
                                errors: snapshot.errors,
                                retries: snapshot.retries,
                                cache_error: session.cache_flush_error(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .abort_handle(),
        );
        generation
    }

    pub fn stop_sampling(&mut self) {
        if let Some(task) = self.sampler.take() {
            task.abort();
        }
    }

    pub fn disconnect(&mut self, session: Session) {
        self.stop_sampling();
        self.spawn(async move {
            Event::Disconnected(session.logout().await.map_err(|error| error.to_string()))
        });
    }

    pub fn benchmark(&mut self, mut session: Session, options: BenchOptions, cache: CacheConfig) {
        let cancellation = BenchmarkCancellation::default();
        self.cancel = Some(cancellation.clone());
        self.spawn(async move {
            if session.cache_config() != cache {
                let configured = async {
                    if session.cache_stats().dirty_bytes > 0 {
                        session.flush().await?;
                    }
                    session.try_enable_cache(cache)
                }
                .await;
                if let Err(error) = configured {
                    return Event::BenchFinished(Err(error.to_string()));
                }
            }
            let outcome = benchmark_cancellable(&session, options, cancellation.clone())
                .await
                .map_err(|error| error.to_string());
            if cancellation.is_cancelled() {
                // Workers have joined; only now is Logout allowed to race no I/O.
                let closed = session.logout().await.map_err(|error| error.to_string());
                return Event::BenchFinished(finish_cancelled_benchmark(outcome, closed));
            }
            Event::BenchFinished(outcome)
        });
    }

    pub fn cancel_benchmark(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}

fn finish_cancelled_benchmark(
    outcome: Result<Option<BenchReport>, String>,
    closed: Result<(), String>,
) -> Result<Option<BenchReport>, String> {
    match (outcome, closed) {
        (Err(error), Err(cleanup)) => Err(format!("{error}；刷写或退出失败：{cleanup}")),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Ok(_), Ok(())) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_never_hides_write_or_cleanup_failures() {
        assert_eq!(
            finish_cancelled_benchmark(Err("write failed".into()), Ok(())).unwrap_err(),
            "write failed"
        );
        let failure =
            finish_cancelled_benchmark(Err("write failed".into()), Err("sync failed".into()))
                .unwrap_err();
        assert!(failure.contains("write failed"));
        assert!(failure.contains("sync failed"));
        assert_eq!(
            finish_cancelled_benchmark(Ok(None), Err("sync failed".into())).unwrap_err(),
            "sync failed"
        );
        assert!(
            finish_cancelled_benchmark(Ok(None), Ok(()))
                .unwrap()
                .is_none()
        );
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.cancel_benchmark();
        self.stop_sampling();
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn connect(target: SavedTarget) -> Result<Connection, iscsi_core::Error> {
    let session = login(&target.target(), &target.options()).await?;
    session.test_unit_ready().await?;
    let inquiry = session.inquiry().await?;
    let capacity = session.capacity().await?;
    // A missing caching page (some targets hide it) must not abort connecting.
    let cache = session.cache_mode().await.unwrap_or(CacheMode {
        write_cache: false,
        read_cache_disabled: false,
    });
    Ok(Connection {
        session,
        inquiry,
        capacity,
        cache,
    })
}

fn flatten<T>(outcome: Result<anyhow::Result<T>, tokio::task::JoinError>) -> Result<T, String> {
    outcome
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}
