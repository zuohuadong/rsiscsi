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
            Self::Idle => "就绪".into(),
            Self::Running => "正在处理…".into(),
            Self::Failed(message) => format!("失败：{message}"),
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
            name: "新目标".into(),
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
                bail!("主机、IQN 和发起端名称不能为空或包含控制字符");
            }
        }
        if self.port == 0 || !(1..=128).contains(&self.queue_depth) {
            bail!("端口须为 1–65535，队列深度须为 1–128");
        }
        if self.chap_user.is_empty() != self.chap_secret.is_empty() {
            bail!("CHAP 用户名和密码必须同时填写");
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
    let home = std::env::var_os("HOME").context("无法确定用户主目录")?;
    Ok(PathBuf::from(home).join("Library/Application Support/iscsi-rs/targets.toml"))
}

pub fn load_config(path: &Path) -> Result<Config> {
    match fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).map_err(|_| anyhow::anyhow!("目标配置格式无效；原文件未改动"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error).context("读取目标配置失败"),
    }
}

pub fn save_config(path: &Path, config: &Config) -> Result<Option<String>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path.parent().context("配置路径没有父目录")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let directory = fs::File::open(parent)?;
    let encoded = toml::to_string_pretty(config).context("无法编码目标配置")?;
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
        Ok(directory
            .sync_all()
            .err()
            .map(|_| "配置已保存，但目录持久化确认失败".into()))
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
    /// `None` 表示自动：读缓存按本机内存计算，写回取推荐值。
    pub mib: Option<usize>,
}

pub const READ_CACHE_PRESETS: [CachePreset; 6] = [
    CachePreset {
        label: "自动（推荐）",
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
];

pub const WRITE_BACK_PRESETS: [CachePreset; 4] = [
    CachePreset {
        label: "推荐（自动）",
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
];

pub struct CacheSettings {
    pub read_enabled: bool,
    pub read_preset: usize,
    pub write_back_enabled: bool,
    pub write_preset: usize,
}

impl Default for CacheSettings {
    /// 读缓存默认开启自动容量；写回保持关闭，行为与未引入缓存时一致。
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
            &READ_CACHE_PRESETS,
            self.read_preset,
            auto_read_cache_bytes(),
        )
    }

    pub fn write_back_capacity(&self) -> Result<usize> {
        Self::preset_bytes(
            &WRITE_BACK_PRESETS,
            self.write_preset,
            recommended_write_back_bytes(),
        )
    }

    fn preset_bytes(presets: &[CachePreset], index: usize, automatic: usize) -> Result<usize> {
        let preset = presets.get(index).context("无效的缓存容量选项")?;
        match preset.mib {
            None => Ok(automatic),
            Some(mib) => mib.checked_mul(1_048_576).context("缓存容量溢出"),
        }
    }

    pub fn config(&self, allow_write: bool) -> Result<CacheConfig> {
        if self.write_back_enabled && !allow_write {
            bail!("写回缓存必须同时确认允许写入");
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
            bail!("写入已禁用：请先确认允许破坏 LUN 上的数据");
        }
        if !(1..=3600).contains(&self.seconds)
            || self.queue_depth == 0
            || self.queue_depth > session_qd
        {
            bail!("时长须为 1–3600 秒；队列深度不能超过当前会话");
        }
        let block_size = self.block_kib.checked_mul(1024).context("块大小溢出")?;
        if block_size == 0
            || block_size > 16 * 1024 * 1024
            || !block_size.is_multiple_of(capacity.block_size as usize)
        {
            bail!("块大小须为逻辑块的整数倍，且不超过 16 MiB");
        }
        let start = self
            .start_mib
            .checked_mul(1_048_576)
            .context("起始位置溢出")?;
        let length = self.range_mib.checked_mul(1_048_576).context("范围溢出")?;
        if length < block_size as u64 || start as u128 + length as u128 > capacity.bytes() {
            bail!("测试范围超出 LUN 或小于一个请求块");
        }
        if !start.is_multiple_of(u64::from(capacity.block_size))
            || !length.is_multiple_of(u64::from(capacity.block_size))
        {
            bail!("范围未对齐逻辑块");
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
    #[test]
    fn config_roundtrip_preserves_iqn_and_is_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("iscsi-gui-test-{}", std::process::id()));
        let path = directory.join("targets.toml");
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
        fs::remove_dir_all(directory)?;
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
