use clap::{Args, Parser, Subcommand, ValueEnum};
use iscsi_core::{
    BenchOptions, BenchPattern, CacheConfig, ChapCredentials, Error, SessionOptions, Target,
    TargetAddr, auto_read_cache_bytes, benchmark, discover_with_chap, login,
    recommended_write_back_bytes,
};
use serde_json::json;
use std::{path::PathBuf, process::ExitCode, time::Duration};

#[derive(Parser)]
#[command(
    name = "rsiscsi",
    version,
    about = "Rust iSCSI 用户态客户端（不挂载磁盘）"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    // Output results as JSON.
    #[arg(help = "以 JSON 输出结果")]
    #[arg(long, global = true)]
    json: bool,
    // Enable TCP_NODELAY; disable with --nodelay=false.
    #[arg(help = "启用 TCP_NODELAY，可用 --nodelay=false 关闭")]
    #[arg(long, global = true, default_value_t = true, action = clap::ArgAction::Set)]
    nodelay: bool,
    // Enable header CRC32C.
    #[arg(help = "启用头部 CRC32C")]
    #[arg(long, global = true)]
    header_digest: bool,
    // Enable data CRC32C.
    #[arg(help = "启用数据 CRC32C")]
    #[arg(long, global = true)]
    data_digest: bool,
    // Local read cache size or auto/off. Reads default to auto (1/8 of memory, 128 MiB-2 GiB);
    // benchmarks default to off so cache hits do not mask actual LUN throughput.
    #[arg(
        help = "本地读缓存容量：大小或 auto/off；read 默认 auto（内存 1/8，128 MiB–2 GiB）， bench 默认关闭以免命中掩盖 LUN 真实吞吐"
    )]
    #[arg(long, global = true, value_parser = parse_read_cache_size)]
    cache_size: Option<usize>,
    // Dangerous: in-memory write-back size; auto uses the recommendation.
    // A crash or power loss discards unflushed data.
    #[arg(help = "危险：内存写回容量，auto 取推荐值；崩溃或断电会丢失未刷写数据")]
    #[arg(long, global = true, default_value = "0", value_parser = parse_write_back_size)]
    write_back: usize,
    // Write-back flush interval in milliseconds; only applies when write-back is enabled.
    #[arg(help = "写回刷写间隔（毫秒），仅写回开启时有效")]
    #[arg(long, global = true)]
    flush_interval: Option<u64>,
    // Confirm destruction of existing data in the specified range.
    #[arg(help = "确认破坏指定区域内的已有数据")]
    #[arg(long, global = true)]
    allow_destructive_write: bool,
}

#[derive(Subcommand)]
enum Command {
    // Discover targets.
    #[command(about = "发现目标")]
    Discover(Portal),
    // List logical units.
    #[command(about = "列出逻辑单元")]
    Ls(TargetArgs),
    // Query device identification, capacity, and negotiated parameters.
    #[command(about = "查询设备标识、容量及协商参数")]
    Inq(TargetArgs),
    // Read logical blocks into a file.
    #[command(about = "读取逻辑块到文件")]
    Read {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long, default_value_t = 0)]
        lba: u64,
        #[arg(long)]
        blocks: u32,
        #[arg(long)]
        out: PathBuf,
        // Repeat reads within one session to verify local read cache hits.
        #[arg(help = "同一会话内重复读取；可验证本地读缓存命中")]
        #[arg(long, default_value_t = 1)]
        repeat: u32,
    },
    // Benchmark throughput, IOPS, and latency (writes destroy data).
    #[command(about = "测试吞吐量、IOPS 和延迟（写入会破坏数据）")]
    Bench {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long, value_enum, default_value = "seq-read")]
        pattern: Pattern,
        #[arg(long, default_value = "1MiB", value_parser = parse_size)]
        bs: usize,
        // Comma-separated queue depths.
        #[arg(help = "逗号分隔的队列深度")]
        #[arg(long, default_value = "16", value_delimiter = ',')]
        qd: Vec<usize>,
        // Measurement duration in seconds.
        #[arg(help = "正式测试秒数")]
        #[arg(long, default_value_t = 10)]
        duration: u64,
        // Warm-up duration in seconds.
        #[arg(help = "预热秒数")]
        #[arg(long, default_value_t = 1)]
        warmup: u64,
        #[arg(long, default_value_t = 0)]
        start_lba: u64,
        // Limit the test range in logical blocks; required for write benchmarks.
        #[arg(help = "限定测试区域的逻辑块数（写入测试必填）")]
        #[arg(long)]
        range_blocks: Option<u64>,
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
    let size = parse_byte_size(input)?;
    if size == 0 || size > u32::MAX as usize {
        Err("块大小超出范围".into())
    } else {
        Ok(size)
    }
}

fn parse_byte_size(input: &str) -> Result<usize, String> {
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
        .ok_or_else(|| "块大小超出范围".into())
}

fn parse_read_cache_size(input: &str) -> Result<usize, String> {
    match input.to_ascii_lowercase().as_str() {
        "auto" => Ok(auto_read_cache_bytes()),
        "off" | "none" => Ok(0),
        _ => parse_byte_size(input),
    }
}

fn parse_write_back_size(input: &str) -> Result<usize, String> {
    match input.to_ascii_lowercase().as_str() {
        "auto" => Ok(recommended_write_back_bytes()),
        "off" | "none" => Ok(0),
        _ => parse_byte_size(input),
    }
}

fn cache_config(cli: &Cli) -> Result<CacheConfig, Error> {
    if cli.write_back > 0 && !cli.allow_destructive_write {
        return Err(Error::Config(
            "--write-back 必须同时指定 --allow-destructive-write".into(),
        ));
    }
    if cli.flush_interval.is_some() && cli.write_back == 0 {
        return Err(Error::Config(
            "--flush-interval 仅在 --write-back 开启时有效".into(),
        ));
    }
    if cli.flush_interval == Some(0) {
        return Err(Error::Config("--flush-interval 必须大于 0".into()));
    }
    let read_capacity_bytes = cli.cache_size.unwrap_or(match cli.command {
        Command::Read { .. } => auto_read_cache_bytes(),
        _ => 0,
    });
    for (name, bytes) in [
        ("--cache-size", read_capacity_bytes),
        ("--write-back", cli.write_back),
    ] {
        if bytes > 0 && bytes < iscsi_core::cache::SEGMENT_BYTES {
            return Err(Error::Config(format!(
                "{name} 至少为 {} KiB",
                iscsi_core::cache::SEGMENT_BYTES / 1024
            )));
        }
    }
    Ok(CacheConfig {
        read_capacity_bytes,
        write_back_capacity_bytes: cli.write_back,
        flush_interval: cli.flush_interval.map(Duration::from_millis),
    })
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
    let cache_config = cache_config(cli)?;
    if let Command::Read { repeat: 0, .. } = cli.command {
        return Err(Error::Config("--repeat 必须大于 0".into()).into());
    }
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
        range_blocks,
        qd,
        duration,
        ..
    } = &cli.command
    {
        if BenchPattern::from(*pattern).is_write()
            && (!cli.allow_destructive_write || range_blocks.is_none())
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
    let mut session = login(&args.target(), &opts).await?;
    session.try_enable_cache(cache_config)?;
    if cli.write_back > 0 {
        eprintln!("警告：写回缓存已开启，崩溃或断电会丢失未刷写的写入");
    }
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
                let cache = session.cache_mode().await.map_err(io_failure)?;
                if cli.json {
                    println!("{}", json!({"inquiry": inquiry, "capacity": capacity, "cache": cache, "negotiated": session.negotiated()}));
                } else {
                    println!("厂商：{}\n产品：{}\n版本：{}\n序列号：{}\n容量：{} 块 × {} 字节（{:.2} GiB）",
                        inquiry.vendor, inquiry.product, inquiry.revision,
                        inquiry.serial.as_deref().unwrap_or("未提供"), capacity.blocks, capacity.block_size,
                        capacity.bytes() as f64 / 1_073_741_824.0);
                    println!("写缓存：{}  WCE={}", if cache.write_cache { "目标有易失缓存" } else { "目标无易失缓存" }, u8::from(cache.write_cache));
                    println!("读缓存：{}  RCD={}", if cache.read_cache_disabled { "目标要求不缓存读" } else { "允许缓存读" }, u8::from(cache.read_cache_disabled));
                    println!("协商参数：{}", serde_json::to_string_pretty(session.negotiated()).map_err(io_failure)?);
                }
            }
            Command::Read { lba, blocks, out, repeat, .. } => {
                let capacity = session.capacity().await.map_err(io_failure)?;
                if lba.checked_add(u64::from(*blocks)).is_none_or(|end| end > capacity.blocks) {
                    return Err(Error::Config("读取范围超出逻辑单元".into()).into());
                }
                let mut bytes = 0;
                for _ in 0..*repeat {
                    if session.cache_config().read_capacity_bytes > 0 {
                        let data = session.read_cached(*lba, *blocks).await.map_err(command_failure)?;
                        bytes = data.len();
                        tokio::fs::write(out, &data).await.map_err(io_failure)?;
                    } else {
                        let data = session.read_pooled(*lba, *blocks).await.map_err(command_failure)?;
                        bytes = data.len();
                        tokio::fs::write(out, &*data).await.map_err(io_failure)?;
                    }
                }
                if cli.json {
                    let mut output = json!({"bytes": bytes, "lba": lba, "blocks": blocks, "out": out});
                    if session.cache_config().read_capacity_bytes > 0 {
                        output["cache"] = json!(session.cache_stats());
                        output["cache_hit_rate"] = json!(session.cache_stats().hit_rate());
                    }
                    println!("{output}");
                } else {
                    println!("已读取 {} 字节至 {}", bytes, out.display());
                    let capacity = session.cache_config().read_capacity_bytes;
                    if capacity > 0 {
                        let stats = session.cache_stats();
                        println!("本地读缓存 {} MiB，命中率 {:.2}%（命中 {} B / LUN 读取 {} B）",
                            capacity / 1_048_576, stats.hit_rate() * 100., stats.hits, stats.misses);
                    }
                }
            }
            Command::Bench { pattern, bs, qd, duration, warmup, start_lba, range_blocks, .. } => {
                let report = benchmark(&session, BenchOptions {
                    pattern: (*pattern).into(), block_size: *bs, queue_depths: qd.clone(),
                    duration: Duration::from_secs(*duration), warmup: Duration::from_secs(*warmup),
                    start_lba: *start_lba, range_blocks: *range_blocks,
                    allow_destructive_write: cli.allow_destructive_write,
                }).await.map_err(command_failure)?;
                if cli.json { println!("{}", json!(report)); }
                else {
                    println!("{:>4} {:>12} {:>12} {:>12} {:>12} {:>12}", "QD", "MiB/s", "IOPS", "p50(μs)", "p95(μs)", "p99(μs)");
                    for r in &report.results {
                        println!("{:>4} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>12.2}",
                            r.queue_depth, r.mib_per_second, r.iops, r.p50_latency_us, r.p95_latency_us, r.p99_latency_us);
                    }
                    if report.flush_seconds > 0.0 { println!("缓存同步：{:.3} 秒", report.flush_seconds); }
                    if session.cache_config().enabled() {
                        println!("本地缓存：命中 {:.2}% / 脏数据 {} B / 同步 {} 次",
                            report.cache.hit_rate() * 100., report.cache.dirty_bytes, report.cache.flushes);
                    }
                }
            }
            Command::Discover(_) => {}
        }
        Ok(())
    }.await;
    let logout_result = session.clone().logout().await.map_err(io_failure);
    let dirty = session.cache_stats().dirty_bytes;
    finish_session_result(result, logout_result, dirty)
}

fn finish_session_result(
    result: Result<(), Failure>,
    logout_result: Result<(), Failure>,
    dirty: usize,
) -> Result<(), Failure> {
    match (result, logout_result) {
        (Err(mut failure), Err(logout)) => {
            failure.code = logout.code;
            failure.message = format!(
                "{}；退出失败：{}；未刷写 {} B，进程退出将丢失这些内存数据",
                failure.message, logout.message, dirty
            );
            Err(failure)
        }
        (Ok(()), Err(mut failure)) => {
            failure.message = format!(
                "{}；未刷写 {} B，进程退出将丢失这些内存数据",
                failure.message, dirty
            );
            Err(failure)
        }
        (Err(failure), Ok(())) => Err(failure),
        (Ok(()), Ok(())) => Ok(()),
    }
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
                "rsiscsi",
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
                "rsiscsi", "bench", "--host", "h", "--iqn", "i", "--bs", "1MiB", "--qd", "1,16"
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
        assert_eq!(parse_read_cache_size("0").unwrap(), 0);
        assert_eq!(parse_read_cache_size("off").unwrap(), 0);
        assert_eq!(parse_write_back_size("OFF").unwrap(), 0);
        assert_eq!(parse_read_cache_size("64KiB").unwrap(), 65536);
        assert_eq!(parse_read_cache_size("8GiB").unwrap(), 8 * 1_073_741_824);
        assert_eq!(
            parse_read_cache_size("auto").unwrap(),
            auto_read_cache_bytes()
        );
        assert_eq!(
            parse_write_back_size("auto").unwrap(),
            recommended_write_back_bytes()
        );
        assert!(parse_size("8GiB").is_err());
    }

    #[test]
    fn read_defaults_to_auto_cache_and_bench_stays_direct() {
        let read = Cli::try_parse_from([
            "rsiscsi", "read", "--host", "h", "--iqn", "i", "--blocks", "1", "--out", "/tmp/x",
        ])
        .unwrap();
        assert_eq!(
            cache_config(&read).unwrap().read_capacity_bytes,
            auto_read_cache_bytes()
        );
        let off = Cli::try_parse_from([
            "rsiscsi",
            "read",
            "--host",
            "h",
            "--iqn",
            "i",
            "--blocks",
            "1",
            "--out",
            "/tmp/x",
            "--cache-size",
            "off",
        ])
        .unwrap();
        assert_eq!(cache_config(&off).unwrap().read_capacity_bytes, 0);
        let bench = Cli::try_parse_from(["rsiscsi", "bench", "--host", "h", "--iqn", "i"]).unwrap();
        assert_eq!(cache_config(&bench).unwrap().read_capacity_bytes, 0);
        let tiny = Cli::try_parse_from([
            "rsiscsi",
            "read",
            "--host",
            "h",
            "--iqn",
            "i",
            "--blocks",
            "1",
            "--out",
            "/tmp/x",
            "--cache-size",
            "4KiB",
        ])
        .unwrap();
        assert!(cache_config(&tiny).is_err());
    }

    #[tokio::test]
    async fn cache_safety_rejects_before_network_and_preserves_defaults() {
        let defaults =
            Cli::try_parse_from(["rsiscsi", "inq", "--host", "invalid", "--iqn", "i"]).unwrap();
        assert_eq!(cache_config(&defaults).unwrap(), CacheConfig::default());
        for flags in [
            vec!["--write-back", "1MiB"],
            vec!["--flush-interval", "10"],
            vec![
                "--write-back",
                "1MiB",
                "--allow-destructive-write",
                "--flush-interval",
                "0",
            ],
        ] {
            let mut args = vec!["rsiscsi", "inq", "--host", "invalid", "--iqn", "i"];
            args.extend(flags);
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(run(&cli).await.err().unwrap().code, 2);
        }
        let cli = Cli::try_parse_from([
            "rsiscsi",
            "--allow-destructive-write",
            "bench",
            "--host",
            "invalid",
            "--iqn",
            "i",
            "--write-back",
            "1MiB",
        ])
        .unwrap();
        assert_eq!(
            cache_config(&cli).unwrap().write_back_capacity_bytes,
            1_048_576
        );
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
        let failure = finish_session_result(
            Err(Failure::from(Error::Config("command invalid".into()))),
            Err(io_failure("sync failed")),
            65536,
        )
        .err()
        .unwrap();
        assert_eq!(failure.code, 5);
        assert!(failure.message.contains("command invalid"));
        assert!(failure.message.contains("sync failed"));
        assert!(failure.message.contains("65536"));
        assert_eq!(
            finish_session_result(Err(Failure::from(Error::Config("bad".into()))), Ok(()), 0,)
                .err()
                .unwrap()
                .code,
            2
        );
    }
}
