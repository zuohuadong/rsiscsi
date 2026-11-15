use crate::i18n::tr;
use anyhow::{Context, Result, bail};
use iscsi_core::{
    BenchOptions, BenchPattern, CacheConfig, Capacity, ChapCredentials, SessionOptions, Target,
    TargetAddr, auto_read_cache_bytes, recommended_write_back_bytes,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const GIB: u64 = 1_073_741_824;

#[derive(Default, Clone, PartialEq, Eq)]
pub enum Operation {
    #[default]
    Idle,
    Running,
    Failed(String),
}

impl Operation {
    pub fn running(&self) -> bool {
        matches!(self, Self::Running)
    }
    pub fn label(&self) -> String {
        match self {
            Self::Idle => tr!("就绪", "Ready").into(),
            Self::Running => tr!("正在处理…", "Working...").into(),
            Self::Failed(message) => tr!("失败：{message}", "Failed: {message}", message = message),
        }
    }
}

// Deliberately not Debug: this model contains credentials.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTarget {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub iqn: String,
    pub lun: u64,
    pub chap_user: String,
    pub chap_secret: String,
    pub initiator_name: String,
    pub queue_depth: usize,
    pub header_digest: bool,
    pub data_digest: bool,
}

impl Default for SavedTarget {
    fn default() -> Self {
        Self {
            name: tr!("新目标", "New target").into(),
            host: String::new(),
            port: 3260,
            iqn: String::new(),
            lun: 0,
            chap_user: String::new(),
            chap_secret: String::new(),
            initiator_name: SessionOptions::default().initiator_name,
            queue_depth: 32,
            header_digest: false,
            data_digest: false,
        }
    }
}

impl SavedTarget {
    pub fn validate(&self) -> Result<()> {
        for text in [&self.host, &self.iqn, &self.initiator_name] {
            if text.is_empty() || text.chars().any(char::is_control) {
                bail!(tr!(
                    "主机、IQN 和发起端名称不能为空或包含控制字符",
                    "Host, IQN and initiator name must not be empty or contain control characters"
                ));
            }
        }
        if self.port == 0 || !(1..=128).contains(&self.queue_depth) {
            bail!(tr!(
                "端口须为 1–65535，队列深度须为 1–128",
                "Port must be 1-65535; queue depth must be 1-128"
            ));
        }
        if self.chap_user.is_empty() != self.chap_secret.is_empty() {
            bail!(tr!(
                "CHAP 用户名和密码必须同时填写",
                "CHAP username and password must both be provided"
            ));
        }
        Ok(())
    }

    pub fn target(&self) -> Target {
        Target {
            addr: TargetAddr {
                host: self.host.clone(),
                port: self.port,
            },
            iqn: self.iqn.clone(),
            lun: self.lun,
            chap: (!self.chap_user.is_empty()).then(|| ChapCredentials {
                user: self.chap_user.clone(),
                secret: self.chap_secret.clone(),
            }),
        }
    }

    pub fn options(&self) -> SessionOptions {
        SessionOptions {
            initiator_name: self.initiator_name.clone(),
            queue_depth: self.queue_depth,
            header_digest: self.header_digest,
            data_digest: self.data_digest,
            ..SessionOptions::default()
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Config {
    pub targets: Vec<SavedTarget>,
}

pub fn config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .context(tr!("无法确定用户主目录", "Cannot determine home directory"))?;
    Ok(config_path_in(Path::new(&home)))
}

fn config_path_in(home: &Path) -> PathBuf {
    home.join("Library/Application Support/rsiscsi/targets.toml")
}

pub fn load_config(path: &Path) -> Result<Config> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|_| {
            anyhow::anyhow!(tr!(
                "目标配置格式无效；原文件未改动",
                "Invalid target configuration; original file unchanged"
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match migrate_legacy_config(path) {
                Ok(config) => Ok(config),
                Err(error) => {
                    eprintln!(
                        "{}",
                        tr!(
                            "警告：迁移旧目标配置失败，使用空目标列表：{error}",
                            "Warning: failed to migrate legacy target configuration; using an empty target list: {error}",
                            error = error
                        )
                    );
                    Ok(Config::default())
                }
            }
        }
        Err(error) => Err(error).context(tr!(
            "读取目标配置失败",
            "Failed to read target configuration"
        )),
    }
}

fn migrate_legacy_config(path: &Path) -> Result<Config> {
    let Some(parent) = path
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "rsiscsi"))
    else {
        return Ok(Config::default());
    };
    let legacy_path = parent.with_file_name("iscsi-rs").join("targets.toml");
    let text = match fs::read_to_string(legacy_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(error).context(tr!(
                "读取旧目标配置失败",
                "Failed to read legacy target configuration"
            ));
        }
    };
    let config = toml::from_str(&text).map_err(|_| {
        anyhow::anyhow!(tr!(
            "旧目标配置格式无效；原文件未改动",
            "Invalid legacy target configuration; original file unchanged"
        ))
    })?;
    if let Some(warning) = save_config(path, &config)? {
        eprintln!(
            "{}",
            tr!("警告：{warning}", "Warning: {warning}", warning = warning)
        );
    }
    Ok(config)
}

pub fn save_config(path: &Path, config: &Config) -> Result<Option<String>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path.parent().context(tr!(
        "配置路径没有父目录",
        "Configuration path has no parent directory"
    ))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let directory = fs::File::open(parent)?;
    let encoded = toml::to_string_pretty(config).context(tr!(
        "无法编码目标配置",
        "Failed to encode target configuration"
    ))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temporary = parent.join(format!(".targets.{}.{nonce}.tmp", std::process::id()));
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let outcome = (|| -> Result<Option<String>> {
        output.write_all(encoded.as_bytes())?;
        output.sync_all()?;
        fs::rename(&temporary, path)?;
        // Rename has committed. A durability warning must not roll memory back.
        Ok(directory.sync_all().err().map(|_| {
            tr!(
                "配置已保存，但目录持久化确认失败",
                "Configuration saved, but directory durability could not be confirmed"
            )
            .into()
        }))
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    outcome
}

pub struct BenchSettings {
    pub pattern: BenchPattern,
    pub block_kib: usize,
    pub queue_depth: usize,
    pub seconds: u64,
    pub start_mib: u64,
    pub range_mib: u64,
    pub allow_write: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CachePreset {
    pub label: &'static str,
    /// `None` selects automatic sizing: local memory for reads, the recommendation for write-back.
    pub mib: Option<usize>,
}

pub fn read_cache_presets() -> [CachePreset; 6] {
    [
        CachePreset {
            label: tr!("自动（推荐）", "Automatic (recommended)"),
            mib: None,
        },
        CachePreset {
            label: "64 MiB",
            mib: Some(64),
        },
        CachePreset {
            label: "128 MiB",
            mib: Some(128),
        },
        CachePreset {
            label: "256 MiB",
            mib: Some(256),
        },
        CachePreset {
            label: "512 MiB",
            mib: Some(512),
        },
        CachePreset {
            label: "1 GiB",
            mib: Some(1024),
        },
    ]
}

pub fn write_back_presets() -> [CachePreset; 4] {
    [
        CachePreset {
            label: tr!("推荐（自动）", "Recommended (automatic)"),
            mib: None,
        },
        CachePreset {
            label: "64 MiB",
            mib: Some(64),
        },
        CachePreset {
            label: "256 MiB",
            mib: Some(256),
        },
        CachePreset {
            label: "512 MiB",
            mib: Some(512),
        },
    ]
}

pub struct CacheSettings {
    pub read_enabled: bool,
    pub read_preset: usize,
    pub write_back_enabled: bool,
    pub write_preset: usize,
}

impl Default for CacheSettings {
    /// Enable automatic read caching by default; keep write-back off to preserve pre-cache write behavior.
    fn default() -> Self {
        Self {
            read_enabled: true,
            read_preset: 0,
            write_back_enabled: false,
            write_preset: 0,
        }
    }
}

impl CacheSettings {
    pub fn read_capacity(&self) -> Result<usize> {
        Self::preset_bytes(
            &read_cache_presets(),
            self.read_preset,
            auto_read_cache_bytes(),
        )
    }

    pub fn write_back_capacity(&self) -> Result<usize> {
        Self::preset_bytes(
            &write_back_presets(),
            self.write_preset,
            recommended_write_back_bytes(),
        )
    }

    fn preset_bytes(presets: &[CachePreset], index: usize, automatic: usize) -> Result<usize> {
        let preset = presets
            .get(index)
            .context(tr!("无效的缓存容量选项", "Invalid cache capacity option"))?;
        match preset.mib {
            None => Ok(automatic),
            Some(mib) => mib
                .checked_mul(1_048_576)
                .context(tr!("缓存容量溢出", "Cache capacity overflow")),
        }
    }

    pub fn config(&self, allow_write: bool) -> Result<CacheConfig> {
        if self.write_back_enabled && !allow_write {
            bail!(tr!(
                "写回缓存必须同时确认允许写入",
                "Write-back cache requires confirmation to allow writes"
            ));
        }
        Ok(CacheConfig {
            read_capacity_bytes: if self.read_enabled {
                self.read_capacity()?
            } else {
                0
            },
            write_back_capacity_bytes: if self.write_back_enabled {
                self.write_back_capacity()?
            } else {
                0
            },
            flush_interval: None,
        })
    }
}

impl BenchSettings {
    pub fn options(&self, capacity: Capacity, session_qd: usize) -> Result<BenchOptions> {
        if self.pattern.is_write() && !self.allow_write {
            bail!(tr!(
                "写入已禁用：请先确认允许破坏 LUN 上的数据",
                "Writes disabled: confirm that data on the LUN may be destroyed first"
            ));
        }
        if !(1..=3600).contains(&self.seconds)
            || self.queue_depth == 0
            || self.queue_depth > session_qd
        {
            bail!(tr!(
                "时长须为 1–3600 秒；队列深度不能超过当前会话",
                "Duration must be 1-3600 s; queue depth must not exceed the current session"
            ));
        }
        let block_size = self
            .block_kib
            .checked_mul(1024)
            .context(tr!("块大小溢出", "Block size overflow"))?;
        if block_size == 0
            || block_size > 16 * 1024 * 1024
            || !block_size.is_multiple_of(capacity.block_size as usize)
        {
            bail!(tr!(
                "块大小须为逻辑块的整数倍，且不超过 16 MiB",
                "Block size must be a multiple of the logical block size and no larger than 16 MiB"
            ));
        }
        let start = self
            .start_mib
            .checked_mul(1_048_576)
            .context(tr!("起始位置溢出", "Start offset overflow"))?;
        let length = self
            .range_mib
            .checked_mul(1_048_576)
            .context(tr!("范围溢出", "Range overflow"))?;
        if length < block_size as u64 || start as u128 + length as u128 > capacity.bytes() {
            bail!(tr!(
                "测试范围超出 LUN 或小于一个请求块",
                "Benchmark range exceeds the LUN or is smaller than one request block"
            ));
        }
        if !start.is_multiple_of(u64::from(capacity.block_size))
            || !length.is_multiple_of(u64::from(capacity.block_size))
        {
            bail!(tr!(
                "范围未对齐逻辑块",
                "Range is not aligned to the logical block size"
            ));
        }
        Ok(BenchOptions {
            pattern: self.pattern,
            block_size,
            queue_depths: vec![self.queue_depth],
            duration: Duration::from_secs(self.seconds),
            warmup: Duration::from_secs(1),
            start_lba: start / u64::from(capacity.block_size),
            range_blocks: Some(length / u64::from(capacity.block_size)),
            allow_destructive_write: self.allow_write,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Result<Self> {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "iscsi-gui-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn config_path(&self) -> PathBuf {
            config_path_in(&self.0)
        }

        fn legacy_path(&self) -> PathBuf {
            self.0
                .join("Library/Application Support/iscsi-rs/targets.toml")
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn saved_config(name: &str) -> Config {
        Config {
            targets: vec![SavedTarget {
                name: name.into(),
                host: "127.0.0.1".into(),
                iqn: "iqn.2026-11.local.test:target".into(),
                ..SavedTarget::default()
            }],
        }
    }

    #[test]
    fn new_config_is_preferred_when_both_exist() -> Result<()> {
        let home = TempHome::new()?;
        save_config(&home.config_path(), &saved_config("new"))?;
        save_config(&home.legacy_path(), &saved_config("legacy"))?;
        let legacy = fs::read(home.legacy_path())?;

        assert_eq!(load_config(&home.config_path())?.targets[0].name, "new");
        assert_eq!(fs::read(home.legacy_path())?, legacy);
        Ok(())
    }

    #[test]
    fn legacy_config_is_migrated_once_and_remains_untouched() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let home = TempHome::new()?;
        let path = home.config_path();
        let config = saved_config("legacy");
        save_config(&home.legacy_path(), &config)?;
        let legacy = fs::read(home.legacy_path())?;

        let loaded = load_config(&path)?;
        assert_eq!(toml::to_string(&loaded)?, toml::to_string(&config)?);
        assert_eq!(fs::read(&path)?, legacy);
        assert_eq!(fs::read(home.legacy_path())?, legacy);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(path.parent().unwrap())?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read_dir(path.parent().unwrap())?.count(), 1);

        fs::write(home.legacy_path(), "not valid TOML")?;
        assert_eq!(load_config(&path)?.targets[0].name, "legacy");
        Ok(())
    }

    #[test]
    fn missing_configs_return_empty_defaults_without_migration() -> Result<()> {
        let home = TempHome::new()?;
        assert_eq!(
            home.config_path(),
            home.0
                .join("Library/Application Support/rsiscsi/targets.toml")
        );
        assert!(load_config(&home.config_path())?.targets.is_empty());
        assert!(!home.config_path().parent().unwrap().exists());
        assert!(!home.legacy_path().exists());
        Ok(())
    }

    #[test]
    fn garbage_legacy_config_returns_empty_defaults() -> Result<()> {
        let home = TempHome::new()?;
        let legacy_path = home.legacy_path();
        fs::create_dir_all(legacy_path.parent().unwrap())?;
        fs::write(&legacy_path, "not valid TOML")?;

        assert!(load_config(&home.config_path())?.targets.is_empty());
        assert!(!home.config_path().exists());
        assert_eq!(fs::read_to_string(legacy_path)?, "not valid TOML");
        Ok(())
    }

    #[test]
    fn unreadable_legacy_config_returns_empty_defaults() -> Result<()> {
        let home = TempHome::new()?;
        // A directory deterministically fails read_to_string, even when tests run as root.
        fs::create_dir_all(home.legacy_path())?;

        assert!(load_config(&home.config_path())?.targets.is_empty());
        assert!(!home.config_path().exists());
        assert!(home.legacy_path().is_dir());
        Ok(())
    }

    #[test]
    fn failed_migration_write_returns_empty_defaults() -> Result<()> {
        let home = TempHome::new()?;
        save_config(&home.legacy_path(), &saved_config("legacy"))?;
        let legacy = fs::read(home.legacy_path())?;
        std::os::unix::fs::symlink(
            home.0.join("missing-directory"),
            home.config_path().parent().unwrap(),
        )?;

        assert!(load_config(&home.config_path())?.targets.is_empty());
        assert!(!home.config_path().exists());
        assert_eq!(fs::read(home.legacy_path())?, legacy);
        Ok(())
    }

    #[test]
    fn config_roundtrip_preserves_iqn_and_is_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let home = TempHome::new()?;
        let path = home.config_path();
        let iqn = "iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836";
        let config = Config {
            targets: vec![SavedTarget {
                iqn: iqn.into(),
                ..SavedTarget::default()
            }],
        };
        save_config(&path, &config)?;
        save_config(&path, &config)?;
        assert_eq!(load_config(&path)?.targets[0].iqn, iqn);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }
    #[test]
    fn safety_rejects_unconfirmed_writes_and_unbounded_ranges() -> Result<()> {
        let capacity = Capacity {
            blocks: 64 * GIB / 512,
            block_size: 512,
        };
        let mut settings = BenchSettings {
            pattern: BenchPattern::SeqRead,
            block_kib: 1024,
            queue_depth: 32,
            seconds: 10,
            start_mib: 0,
            range_mib: 16384,
            allow_write: false,
        };
        assert_eq!(
            settings.options(capacity, 32)?.range_blocks,
            Some(16 * GIB / 512)
        );
        settings.pattern = BenchPattern::SeqWrite;
        assert!(settings.options(capacity, 32).is_err());
        settings.allow_write = true;
        assert!(settings.options(capacity, 32).is_ok());
        settings.range_mib = 0;
        assert!(settings.options(capacity, 32).is_err());
        settings.range_mib = u64::MAX;
        assert!(settings.options(capacity, 32).is_err());
        Ok(())
    }

    #[test]
    fn caches_default_to_auto_read_and_closed_write_back() -> Result<()> {
        let mut settings = CacheSettings::default();
        let defaults = settings.config(false)?;
        assert_eq!(defaults.read_capacity_bytes, auto_read_cache_bytes());
        assert_eq!(defaults.write_back_capacity_bytes, 0);
        settings.read_preset = 2;
        assert_eq!(settings.config(false)?.read_capacity_bytes, 128 * 1_048_576);
        settings.read_enabled = false;
        assert_eq!(settings.config(false)?.read_capacity_bytes, 0);
        settings.write_back_enabled = true;
        assert!(settings.config(false).is_err());
        assert_eq!(
            settings.config(true)?.write_back_capacity_bytes,
            recommended_write_back_bytes()
        );
        settings.write_preset = 3;
        assert_eq!(
            settings.config(true)?.write_back_capacity_bytes,
            512 * 1_048_576
        );
        settings.read_preset = usize::MAX;
        settings.read_enabled = true;
        assert!(settings.config(true).is_err());
        Ok(())
    }
}
