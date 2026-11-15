mod abi;
mod iokit;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use iokit::{Arena, Config, Dequeue, Mapping, Region, RequestQueue, UserClient};
use iscsi_core::{Capacity, Session, SessionOptions, Target, TargetAddr};
use std::{
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering, fence},
    },
    time::Duration,
};
use tokio::{
    signal::unix::{Signal, SignalKind, signal},
    sync::Semaphore,
    task::{JoinError, JoinSet},
    time::{Instant, MissedTickBehavior, interval_at},
};
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "iscsi-blockd",
    version,
    about = "只读 iSCSI 块设备用户态守护进程"
)]
struct Cli {
    // iSCSI target host address.
    #[arg(help = "iSCSI 目标主机地址")]
    #[arg(long)]
    host: String,
    // iSCSI target port.
    #[arg(help = "iSCSI 目标端口")]
    #[arg(long, default_value_t = 3260, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    // iSCSI target IQN.
    #[arg(help = "iSCSI 目标 IQN")]
    #[arg(long)]
    iqn: String,
    // Target logical unit number.
    #[arg(help = "目标逻辑单元号")]
    #[arg(long, default_value_t = 1)]
    lun: u64,
    // Maximum concurrent reads (1-65536).
    #[arg(help = "最大并发读取数（1–65536）")]
    #[arg(long, default_value_t = 64, value_parser = parse_queue_depth)]
    queue_depth: usize,
    // Initiator IQN; generated automatically by default.
    #[arg(help = "发起端 IQN，默认自动生成")]
    #[arg(long)]
    initiator: Option<String>,
}

fn parse_queue_depth(input: &str) -> std::result::Result<usize, String> {
    input
        .parse::<usize>()
        .ok()
        .filter(|depth| (1..=65536).contains(depth))
        .ok_or_else(|| "队列深度必须介于 1 和 65536 之间".into())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let logging = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    if let Err(failure) = logging {
        eprintln!("日志初始化失败：{failure}");
        return ExitCode::FAILURE;
    }
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            error!(error = %format!("{failure:#}"), "守护进程退出");
            ExitCode::FAILURE
        }
    }
}

struct Device {
    queue: RequestQueue,
    arena: Arc<Arena>,
    client: Arc<UserClient>,
    config: Config,
}

impl Device {
    fn map(client: Arc<UserClient>) -> Result<Self> {
        let arena_mapping = Mapping::new(client.clone(), abi::MEMORY_ARENA)?;
        let queue_mapping = Mapping::new(client.clone(), abi::MEMORY_REQUESTS)?;
        let config = client.config(arena_mapping.len)?;
        let arena = Arena::new(arena_mapping, config.arena_bytes)?;
        let queue = RequestQueue::new(queue_mapping)?;
        Ok(Self {
            queue,
            arena,
            client,
            config,
        })
    }
}

async fn run(cli: Cli) -> Result<()> {
    let mut interrupt = signal(SignalKind::interrupt()).context("无法注册 Ctrl-C")?;
    let client = tokio::select! {
        biased;
        _ = interrupt.recv() => return Ok(()),
        connection = UserClient::connect() => connection?,
    };
    let mut device = Device::map(client)?;
    let target = Target {
        addr: TargetAddr {
            host: cli.host,
            port: cli.port,
        },
        iqn: cli.iqn,
        lun: cli.lun,
        chap: None,
    };
    let mut options = SessionOptions {
        queue_depth: cli.queue_depth,
        ..SessionOptions::default()
    };
    if let Some(initiator) = cli.initiator {
        options.initiator_name = initiator;
    }
    let session = tokio::select! {
        biased;
        _ = interrupt.recv() => return device.client.set_offline(),
        login = iscsi_core::login(&target, &options) => login.context("iSCSI 登录失败")?,
    };
    let startup = tokio::select! {
        biased;
        _ = interrupt.recv() => Ok(None),
        capacity = initialize(&device.client, &session) => capacity.map(Some),
    };
    let serving = match startup {
        Ok(Some(capacity)) => serve(&mut device, &session, capacity, &mut interrupt).await,
        Ok(None) => device.client.set_offline(),
        Err(failure) => {
            if let Err(offline) = device.client.set_offline() {
                warn!(%offline, "初始化失败后无法设置离线");
            }
            Err(failure)
        }
    };
    // All read tasks have joined before mappings and then the client are dropped.
    drop(device);
    let logout = session.logout().await.context("iSCSI 注销失败");
    serving.and(logout)
}

async fn initialize(client: &UserClient, session: &Session) -> Result<Capacity> {
    let inquiry = session.inquiry().await.context("INQUIRY 失败")?;
    info!(vendor = %inquiry.vendor, product = %inquiry.product, revision = %inquiry.revision, "目标设备");
    let capacity = session.capacity().await.context("READ CAPACITY 失败")?;
    validate_geometry(capacity)?;
    info!(
        blocks = capacity.blocks,
        block_size = capacity.block_size,
        queue_depth = session.queue_depth(),
        "目标容量"
    );
    client.set_geometry(capacity.block_size, capacity.blocks)?;
    client.set_online()?;
    Ok(capacity)
}

fn validate_geometry(capacity: Capacity) -> Result<()> {
    ensure!(
        (512..=65536).contains(&capacity.block_size) && capacity.block_size.is_power_of_two(),
        "unsupported logical block size {}",
        capacity.block_size
    );
    ensure!(capacity.blocks > 0, "target has zero blocks");
    Ok(())
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
}

impl Counters {
    fn log(&self) {
        info!(
            requests = self.requests.load(Ordering::Relaxed),
            bytes = self.bytes.load(Ordering::Relaxed),
            errors = self.errors.load(Ordering::Relaxed),
            "只读 I/O 汇总"
        );
    }
}

struct Reads {
    client: Arc<UserClient>,
    arena: Arc<Arena>,
    capacity: Capacity,
    max_request_bytes: u32,
    permits: Arc<Semaphore>,
    tasks: JoinSet<()>,
    counters: Arc<Counters>,
}

impl Reads {
    fn new(device: &Device, session: &Session, capacity: Capacity) -> Self {
        Self {
            client: device.client.clone(),
            arena: device.arena.clone(),
            capacity,
            max_request_bytes: device.config.max_request_bytes,
            permits: Arc::new(Semaphore::new(session.queue_depth())),
            tasks: JoinSet::new(),
            counters: Arc::new(Counters::default()),
        }
    }

    fn dispatch(&mut self, request: abi::Request, session: Session) -> Result<()> {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let region = validate_request(&request, self.capacity, self.max_request_bytes)
            .and_then(|()| self.arena.reserve(request.arena_offset, request.length));
        let mut region = match region {
            Ok(region) => region,
            Err(failure) => {
                warn!(token = request.token, chunk = request.chunk, %failure, "拒绝无效请求");
                finish_request(&self.client, &self.counters, &request, Err(failure));
                return Ok(());
            }
        };
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .context("read queue is full")?;
        let client = self.client.clone();
        let counters = self.counters.clone();
        self.tasks.spawn(async move {
            let _permit = permit;
            let read = read_request(&session, &request, &mut region).await;
            // End the mutable borrow/lease before COMPLETE_IO lets the kernel
            // recycle this range and enqueue a new request.
            drop(region);
            finish_request(&client, &counters, &request, read);
        });
        Ok(())
    }

    fn joined(&self, joined: std::result::Result<(), JoinError>) -> Result<()> {
        if let Err(failure) = joined {
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
            return Err(failure).context("read task terminated unexpectedly");
        }
        Ok(())
    }
}

fn validate_request(request: &abi::Request, capacity: Capacity, max_bytes: u32) -> Result<()> {
    ensure!(
        request.direction == abi::TRANSFER_READ,
        "non-read direction {}",
        request.direction
    );
    ensure!(
        request.block_size == capacity.block_size,
        "request block size differs from target"
    );
    ensure!(
        request.length > 0
            && request.length <= max_bytes
            && request.length.is_multiple_of(capacity.block_size),
        "invalid read length {}",
        request.length
    );
    ensure!(
        request
            .lba
            .checked_add(u64::from(request.length / capacity.block_size))
            .is_some_and(|end| end <= capacity.blocks),
        "read exceeds target capacity"
    );
    Ok(())
}

async fn read_request(
    session: &Session,
    request: &abi::Request,
    region: &mut Region,
) -> Result<()> {
    session
        .read_into(
            request.lba,
            request.length / request.block_size,
            region.bytes()?,
        )
        .await
        .context("iSCSI 读取失败")
}

fn finish_request(
    client: &UserClient,
    counters: &Counters,
    request: &abi::Request,
    read: Result<()>,
) {
    let (status, bytes) = match read {
        Ok(()) => {
            fence(Ordering::Release);
            (abi::STATUS_SUCCESS, u64::from(request.length))
        }
        Err(failure) => {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            warn!(token = request.token, chunk = request.chunk, %failure, "读取请求失败");
            (abi::STATUS_FAILURE, 0)
        }
    };
    if client.complete(request, status, bytes).is_err() {
        if status == abi::STATUS_SUCCESS {
            counters.errors.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        counters.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

async fn serve(
    device: &mut Device,
    session: &Session,
    capacity: Capacity,
    interrupt: &mut Signal,
) -> Result<()> {
    let mut reads = Reads::new(device, session, capacity);
    let serving = drive_queue(&mut device.queue, session, &mut reads, interrupt).await;
    let offline = device.client.set_offline();
    // Do not cancel reads or unmap memory while an async task can still use it.
    let mut drained = Ok(());
    while let Some(joined) = reads.tasks.join_next().await {
        if let Err(failure) = reads.joined(joined) {
            error!(%failure, "关闭时读取任务失败");
            drained = Err(failure);
        }
    }
    reads.counters.log();
    serving.and(offline).and(drained)
}

async fn drive_queue(
    queue: &mut RequestQueue,
    session: &Session,
    reads: &mut Reads,
    interrupt: &mut Signal,
) -> Result<()> {
    let mut summary = interval_at(
        Instant::now() + Duration::from_secs(10),
        Duration::from_secs(10),
    );
    summary.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        if reads.client.is_detached() {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = interrupt.recv() => {
                info!("收到 Ctrl-C，正在关闭");
                return Ok(());
            }
            _ = summary.tick() => reads.counters.log(),
            Some(joined) = reads.tasks.join_next(), if !reads.tasks.is_empty() => reads.joined(joined)?,
            _ = std::future::ready(()), if reads.permits.available_permits() > 0 => {
                match queue.dequeue()? {
                    Dequeue::Request(request) => reads.dispatch(request, session.clone())?,
                    Dequeue::Skipped(size) => {
                        reads.counters.requests.fetch_add(1, Ordering::Relaxed);
                        reads.counters.errors.fetch_add(1, Ordering::Relaxed);
                        warn!(size, "跳过 ABI 长度错误的队列条目");
                    }
                    Dequeue::Empty => {
                        tokio::select! {
                            biased;
                            _ = interrupt.recv() => return Ok(()),
                            _ = tokio::time::sleep(Duration::from_micros(200)) => {}
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_contract_and_limits() {
        Cli::command().debug_assert();
        let args = [
            "iscsi-blockd",
            "--host",
            "localhost",
            "--iqn",
            "iqn.example:test",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(cli.port, 3260);
        assert_eq!(cli.lun, 1);
        assert_eq!(cli.queue_depth, 64);
        assert!(cli.initiator.is_none());
        assert!(Cli::try_parse_from(["iscsi-blockd"]).is_err());
        for depth in ["0", "65537", "invalid"] {
            let mut invalid = args.to_vec();
            invalid.extend(["--queue-depth", depth]);
            assert!(Cli::try_parse_from(invalid).is_err());
        }
    }

    #[test]
    fn geometry_boundaries() {
        for block_size in [512, 4096, 65536] {
            assert!(
                validate_geometry(Capacity {
                    blocks: 100,
                    block_size
                })
                .is_ok()
            );
        }
        for block_size in [0, 256, 513, 131072] {
            assert!(
                validate_geometry(Capacity {
                    blocks: 100,
                    block_size
                })
                .is_err()
            );
        }
        assert!(
            validate_geometry(Capacity {
                blocks: 0,
                block_size: 512
            })
            .is_err()
        );
    }

    #[test]
    fn request_validation_never_accepts_writes_or_invalid_ranges() {
        let capacity = Capacity {
            blocks: 100,
            block_size: 512,
        };
        let request = abi::Request {
            block_size: 512,
            length: 512,
            lba: 99,
            ..abi::Request::default()
        };
        let validate = |request: &abi::Request| validate_request(request, capacity, 1024);
        assert!(validate(&request).is_ok());
        for invalid in [
            abi::Request {
                direction: 1,
                ..request
            },
            abi::Request {
                direction: u32::MAX,
                ..request
            },
            abi::Request {
                block_size: 0,
                ..request
            },
            abi::Request {
                length: 0,
                ..request
            },
            abi::Request {
                length: 513,
                ..request
            },
            abi::Request {
                length: 1536,
                lba: 0,
                ..request
            },
            abi::Request {
                lba: 100,
                ..request
            },
            abi::Request {
                lba: u64::MAX,
                ..request
            },
        ] {
            assert!(validate(&invalid).is_err(), "{invalid:?}");
        }
    }
}
