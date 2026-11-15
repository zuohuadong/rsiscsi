use clap::{Args, Parser, Subcommand, ValueEnum};
use iscsi_core::{
    BenchOptions, BenchPattern, ChapCredentials, Error, SessionOptions, Target, TargetAddr,
    benchmark, discover_with_chap, login,
};
use serde_json::json;
use std::{path::PathBuf, process::ExitCode, time::Duration};

#[derive(Parser)]
#[command(
    name = "iscsi-rs",
    version,
    about = "Rust iSCSI 用户态客户端（不挂载磁盘）"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// 以 JSON 输出结果
    #[arg(long, global = true)]
    json: bool,
    /// 启用 TCP_NODELAY，可用 --nodelay=false 关闭
    #[arg(long, global = true, default_value_t = true, action = clap::ArgAction::Set)]
    nodelay: bool,
    /// 启用头部 CRC32C
    #[arg(long, global = true)]
    header_digest: bool,
    /// 启用数据 CRC32C
    #[arg(long, global = true)]
    data_digest: bool,
}

#[derive(Subcommand)]
enum Command {
    /// 发现目标
    Discover(Portal),
    /// 列出逻辑单元
    Ls(TargetArgs),
    /// 查询设备标识、容量及协商参数
    Inq(TargetArgs),
    /// 读取逻辑块到文件
    Read {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long, default_value_t = 0)]
        lba: u64,
        #[arg(long)]
        blocks: u32,
        #[arg(long)]
        out: PathBuf,
    },
    /// 测试吞吐量、IOPS 和延迟（写入会破坏数据）
    Bench {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long, value_enum, default_value = "seq-read")]
        pattern: Pattern,
        #[arg(long, default_value = "1MiB", value_parser = parse_size)]
        bs: usize,
        /// 逗号分隔的队列深度
        #[arg(long, default_value = "16", value_delimiter = ',')]
        qd: Vec<usize>,
        /// 正式测试秒数
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// 预热秒数
        #[arg(long, default_value_t = 1)]
        warmup: u64,
        #[arg(long, default_value_t = 0)]
        start_lba: u64,
        /// 限定测试区域的逻辑块数（写入测试必填）
        #[arg(long)]
        range_blocks: Option<u64>,
        /// 确认破坏指定区域内的已有数据
        #[arg(long)]
        allow_destructive_write: bool,
    },
}

#[derive(Args)]
struct Portal {
    #[arg(long)]
    host: String,
    #[arg(long, default_value_t = 3260)]
    port: u16,
    #[arg(long, requires = "chap_secret")]
    chap_user: Option<String>,
    #[arg(long, requires = "chap_user", hide_possible_values = true)]
    chap_secret: Option<String>,
}

impl Portal {
    fn addr(&self) -> TargetAddr {
        TargetAddr {
            host: self.host.clone(),
            port: self.port,
        }
    }
    fn chap(&self) -> Option<ChapCredentials> {
        self.chap_user
            .as_ref()
            .zip(self.chap_secret.as_ref())
            .map(|(user, secret)| ChapCredentials {
                user: user.clone(),
                secret: secret.clone(),
            })
    }
}

#[derive(Args)]
struct TargetArgs {
    #[command(flatten)]
    portal: Portal,
    #[arg(long)]
    iqn: String,
    #[arg(long, default_value_t = 0)]
    lun: u64,
    #[arg(long)]
    initiator_name: Option<String>,
    #[arg(long, default_value_t = 128)]
    queue_depth: usize,
}

impl TargetArgs {
    fn target(&self) -> Target {
        Target {
            addr: self.portal.addr(),
            iqn: self.iqn.clone(),
            lun: self.lun,
            chap: self.portal.chap(),
        }
    }
}

#[derive(ValueEnum, Clone, Copy)]
enum Pattern {
    SeqRead,
    RandRead,
    SeqWrite,
    RandWrite,
}
impl From<Pattern> for BenchPattern {
    fn from(value: Pattern) -> Self {
        match value {
            Pattern::SeqRead => Self::SeqRead,
            Pattern::RandRead => Self::RandRead,
            Pattern::SeqWrite => Self::SeqWrite,
            Pattern::RandWrite => Self::RandWrite,
        }
    }
}

fn parse_size(input: &str) -> Result<usize, String> {
    let split = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    let number = input[..split]
        .parse::<usize>()
        .map_err(|_| "无效的块大小")?;
    let multiplier: usize = match input[split..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "ki" | "kib" => 1024,
        "m" | "mi" | "mib" => 1024 * 1024,
        "g" | "gi" | "gib" => 1024 * 1024 * 1024,
        "kb" => 1000,
        "mb" => 1_000_000,
        _ => return Err("大小单位应为 B、KiB、MiB 或 GiB".into()),
    };
    number
        .checked_mul(multiplier)
        .filter(|n| *n > 0 && *n <= u32::MAX as usize)
        .ok_or_else(|| "块大小超出范围".into())
}

struct Failure {
    code: u8,
    message: String,
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        let code = match &error {
            Error::Config(_) => 2,
            Error::Transport(_) | Error::Timeout => 3,
            Error::Login(_) => 4,
            _ => 5,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}
fn io_failure(error: impl std::fmt::Display) -> Failure {
    Failure {
        code: 5,
        message: error.to_string(),
    }
}

fn command_failure(error: Error) -> Failure {
    match error {
        Error::Config(_) => error.into(),
        _ => io_failure(error),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    match run(&cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            if cli.json {
                eprintln!(
                    "{}",
                    json!({"error": failure.message, "exit_code": failure.code})
                );
            } else {
                eprintln!("错误：{}", failure.message);
            }
            ExitCode::from(failure.code)
        }
    }
}

async fn run(cli: &Cli) -> Result<(), Failure> {
    let mut opts = SessionOptions {
        nodelay: cli.nodelay,
        header_digest: cli.header_digest,
        data_digest: cli.data_digest,
        ..SessionOptions::default()
    };
    let args = match &cli.command {
        Command::Discover(portal) => {
            let targets = discover_with_chap(&portal.addr(), &opts, portal.chap().as_ref()).await?;
            if cli.json {
                println!("{}", json!(targets));
            } else {
                for target in targets {
                    println!(
                        "目标：{}\n地址：{}",
                        target.iqn,
                        target.addresses.join(", ")
                    );
                }
            }
            return Ok(());
        }
        Command::Ls(args) | Command::Inq(args) => args,
        Command::Read { target, .. } | Command::Bench { target, .. } => target,
    };
    opts.queue_depth = args.queue_depth;
    if let Some(name) = &args.initiator_name {
        opts.initiator_name = name.clone();
    }
    // Reject a destructive benchmark before any network or SCSI activity.
    if let Command::Bench {
        pattern,
        allow_destructive_write,
        range_blocks,
        qd,
        duration,
        ..
    } = &cli.command
    {
        if BenchPattern::from(*pattern).is_write()
            && (!allow_destructive_write || range_blocks.is_none())
        {
            return Err(Error::Config(
                "写入测试必须指定 --allow-destructive-write 和 --range-blocks".into(),
            )
            .into());
        }
        if *duration == 0 || qd.is_empty() || qd.iter().any(|q| *q == 0 || *q > opts.queue_depth) {
            return Err(Error::Config("无效的测试时长或队列深度".into()).into());
        }
    }
    let session = login(&args.target(), &opts).await?;
    let result: Result<(), Failure> = async {
        match &cli.command {
            Command::Ls(_) => {
                let luns = session.report_luns().await.map_err(io_failure)?;
                if cli.json { println!("{}", json!({"luns": luns, "negotiated": session.negotiated()})); }
                else { for lun in luns { println!("逻辑单元：{lun}"); } }
            }
            Command::Inq(_) => {
                session.test_unit_ready().await.map_err(io_failure)?;
                let inquiry = session.inquiry().await.map_err(io_failure)?;
                let capacity = session.capacity().await.map_err(io_failure)?;
                if cli.json {
                    println!("{}", json!({"inquiry": inquiry, "capacity": capacity, "negotiated": session.negotiated()}));
                } else {
                    println!("厂商：{}\n产品：{}\n版本：{}\n序列号：{}\n容量：{} 块 × {} 字节（{:.2} GiB）",
                        inquiry.vendor, inquiry.product, inquiry.revision,
                        inquiry.serial.as_deref().unwrap_or("未提供"), capacity.blocks, capacity.block_size,
                        capacity.bytes() as f64 / 1_073_741_824.0);
                    println!("协商参数：{}", serde_json::to_string_pretty(session.negotiated()).map_err(io_failure)?);
                }
            }
            Command::Read { lba, blocks, out, .. } => {
                let capacity = session.capacity().await.map_err(io_failure)?;
                if lba.checked_add(u64::from(*blocks)).is_none_or(|end| end > capacity.blocks) {
                    return Err(Error::Config("读取范围超出逻辑单元".into()).into());
                }
                let data = session.read_pooled(*lba, *blocks).await.map_err(command_failure)?;
                tokio::fs::write(out, &*data).await.map_err(io_failure)?;
                if cli.json { println!("{}", json!({"bytes": data.len(), "lba": lba, "blocks": blocks, "out": out})); }
                else { println!("已读取 {} 字节至 {}", data.len(), out.display()); }
            }
            Command::Bench { pattern, bs, qd, duration, warmup, start_lba, range_blocks, allow_destructive_write, .. } => {
                let report = benchmark(&session, BenchOptions {
                    pattern: (*pattern).into(), block_size: *bs, queue_depths: qd.clone(),
                    duration: Duration::from_secs(*duration), warmup: Duration::from_secs(*warmup),
                    start_lba: *start_lba, range_blocks: *range_blocks,
                    allow_destructive_write: *allow_destructive_write,
                }).await.map_err(command_failure)?;
                if cli.json { println!("{}", json!(report)); }
                else {
                    println!("{:>4} {:>12} {:>12} {:>12} {:>12} {:>12}", "QD", "MiB/s", "IOPS", "p50(μs)", "p95(μs)", "p99(μs)");
                    for r in &report.results {
                        println!("{:>4} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>12.2}",
                            r.queue_depth, r.mib_per_second, r.iops, r.p50_latency_us, r.p95_latency_us, r.p99_latency_us);
                    }
                    if report.flush_seconds > 0.0 { println!("缓存同步：{:.3} 秒", report.flush_seconds); }
                }
            }
            Command::Discover(_) => {}
        }
        Ok(())
    }.await;
    let logout_result = session.logout().await.map_err(io_failure);
    result.and(logout_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clap_contract() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        assert!(
            Cli::try_parse_from([
                "iscsi-rs",
                "inq",
                "--host",
                "h",
                "--iqn",
                "iqn.a:b:c",
                "--nodelay=false"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "iscsi-rs", "bench", "--host", "h", "--iqn", "i", "--bs", "1MiB", "--qd", "1,16"
            ])
            .is_ok()
        );
    }
    #[test]
    fn sizes() {
        assert_eq!(parse_size("1MiB").unwrap(), 1_048_576);
        assert_eq!(parse_size("4KiB").unwrap(), 4096);
        assert!(parse_size("0").is_err());
        assert!(parse_size("999999999999GiB").is_err());
    }

    #[test]
    fn stable_error_exit_codes() {
        assert_eq!(Failure::from(Error::Config("bad".into())).code, 2);
        assert_eq!(
            Failure::from(Error::Transport(
                std::io::ErrorKind::ConnectionRefused.into()
            ))
            .code,
            3
        );
        assert_eq!(Failure::from(Error::Login("rejected".into())).code, 4);
        assert_eq!(command_failure(Error::Timeout).code, 5);
        assert_eq!(command_failure(Error::Config("bad".into())).code, 2);
        assert_eq!(
            command_failure(Error::Scsi {
                status: 2,
                sense: vec![]
            })
            .code,
            5
        );
    }
}
