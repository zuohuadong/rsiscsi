//! 只读探测 macOS 是否已把某个 LUN 挂成块设备。
//!
//! 本应用不创建块设备，也不执行初始化或抹掉；这里只读取 `diskutil` 的 plist
//! 输出，把用户引导到「磁盘工具」或 Finder。
use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskState {
    /// 没有分区表或文件系统，磁盘工具会显示为未初始化。
    Blank,
    /// 有分区表但没有可挂载的卷。
    NoVolumes,
    /// 至少一个卷已挂载。
    Mounted(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskEntry {
    pub id: String,
    pub size: u64,
    pub state: DiskState,
    pub volumes: Vec<PathBuf>,
}

impl DiskEntry {
    pub fn node(&self) -> String {
        format!("/dev/{}", self.id)
    }

    pub fn size_text(&self) -> String {
        let bytes = self.size as f64;
        if bytes >= 1e12 {
            format!("{:.2} TB", bytes / 1e12)
        } else if bytes >= 1e9 {
            format!("{:.1} GB", bytes / 1e9)
        } else {
            format!("{:.0} MB", bytes / 1e6)
        }
    }

    pub fn status(&self) -> String {
        match &self.state {
            DiskState::Blank => "未初始化".into(),
            DiskState::NoVolumes => "无可用文件系统".into(),
            DiskState::Mounted(_) => {
                let mounts = self
                    .volumes
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("、");
                format!("已挂载：{mounts}")
            }
        }
    }

    pub fn mount_point(&self) -> Option<&PathBuf> {
        match &self.state {
            DiskState::Mounted(path) => Some(path),
            _ => None,
        }
    }
}

/// 记录当前已有磁盘，之后只把新出现的或仍为空白的外置盘报给用户。
pub fn whole_disks() -> Result<Vec<String>> {
    let value = diskutil_json(&["list", "-plist"])?;
    Ok(value
        .get("WholeDisks")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

pub fn discover(known: &[String]) -> Result<Vec<DiskEntry>> {
    entries_from_json(&diskutil_json(&["list", "-plist"])?, known)
}

fn entries_from_json(value: &Value, known: &[String]) -> Result<Vec<DiskEntry>> {
    let disks = value
        .get("AllDisksAndPartitions")
        .and_then(Value::as_array)
        .context("diskutil 输出缺少磁盘列表")?;
    let mut entries = Vec::new();
    for disk in disks {
        let Some(id) = disk.get("DeviceIdentifier").and_then(Value::as_str) else {
            continue;
        };
        // 只要整盘：分区形如 diskNsM，合成的 APFS 容器带 APFSPhysicalStores。
        let suffix = id.strip_prefix("disk").unwrap_or(id);
        if suffix.contains('s') || disk.get("APFSPhysicalStores").is_some() {
            continue;
        }
        let blank = disk
            .get("Content")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if !blank && known.iter().any(|seen| seen == id) {
            continue;
        }
        let mut partitions = Vec::new();
        let mut volumes = Vec::new();
        if let Some(parts) = disk.get("Partitions").and_then(Value::as_array) {
            for part in parts {
                if let Some(partition) = part.get("DeviceIdentifier").and_then(Value::as_str) {
                    partitions.push(partition.to_owned());
                }
                if let Some(mount) = part.get("MountPoint").and_then(Value::as_str) {
                    volumes.push(PathBuf::from(mount));
                }
            }
        }
        for container in disks {
            let linked = container
                .get("APFSPhysicalStores")
                .and_then(Value::as_array)
                .is_some_and(|stores| {
                    stores.iter().any(|store| {
                        store
                            .get("DeviceIdentifier")
                            .and_then(Value::as_str)
                            .is_some_and(|store| partitions.iter().any(|part| part == store))
                    })
                });
            if !linked {
                continue;
            }
            if let Some(apfs) = container.get("APFSVolumes").and_then(Value::as_array) {
                for volume in apfs {
                    if let Some(mount) = volume.get("MountPoint").and_then(Value::as_str) {
                        volumes.push(PathBuf::from(mount));
                    }
                }
            }
        }
        let state = if blank {
            DiskState::Blank
        } else if let Some(mount) = volumes.first() {
            DiskState::Mounted(mount.clone())
        } else {
            DiskState::NoVolumes
        };
        entries.push(DiskEntry {
            id: id.to_owned(),
            size: disk.get("Size").and_then(Value::as_u64).unwrap_or(0),
            state,
            volumes,
        });
    }
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(entries)
}

fn diskutil_json(args: &[&str]) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let output = Command::new("/usr/sbin/diskutil")
            .args(args)
            .output()
            .context("无法执行 diskutil")?;
        if !output.status.success() {
            bail!(
                "diskutil 失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut child = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("无法执行 plutil")?;
        child
            .stdin
            .as_mut()
            .context("无法写入 plutil")?
            .write_all(&output.stdout)
            .context("无法写入 plutil")?;
        let converted = child.wait_with_output().context("plutil 未完成")?;
        if !converted.status.success() {
            bail!(
                "plutil 失败：{}",
                String::from_utf8_lossy(&converted.stderr).trim()
            );
        }
        serde_json::from_slice(&converted.stdout).context("无法解析 diskutil JSON")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        bail!("磁盘探测仅支持 macOS")
    }
}

pub fn open_disk_utility() -> Result<()> {
    open(&["-b", "com.apple.DiskUtility"])
}

pub fn reveal(path: &Path) -> Result<()> {
    open(&[&path.display().to_string()])
}

fn open(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("open")
        .args(args)
        .status()
        .context("无法执行 open")?;
    if !status.success() {
        bail!("open 退出码 {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn blank_disk_is_reported_even_when_already_known() {
        let value = json!({
            "AllDisksAndPartitions": [
                { "DeviceIdentifier": "disk4", "Size": 4_000_000_000_000u64 }
            ]
        });
        let entries = entries_from_json(&value, &["disk4".into()]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, DiskState::Blank);
        assert_eq!(entries[0].node(), "/dev/disk4");
        assert_eq!(entries[0].size_text(), "4.00 TB");
    }

    #[test]
    fn known_partitioned_disk_is_skipped_unless_it_is_new() {
        let value = json!({
            "AllDisksAndPartitions": [
                { "DeviceIdentifier": "disk4", "Size": 1000, "Content": "GUID_partition_scheme",
                  "Partitions": [{ "DeviceIdentifier": "disk4s1", "Content": "EFI" }] }
            ]
        });
        assert!(
            entries_from_json(&value, &["disk4".into()])
                .unwrap()
                .is_empty()
        );
        let entries = entries_from_json(&value, &[]).unwrap();
        assert_eq!(entries[0].state, DiskState::NoVolumes);
    }

    #[test]
    fn apfs_container_mount_is_linked_to_its_whole_disk() {
        let value = json!({
            "AllDisksAndPartitions": [
                { "DeviceIdentifier": "disk9", "Size": 4_000_000_000_000u64,
                  "Content": "GUID_partition_scheme",
                  "Partitions": [
                    { "DeviceIdentifier": "disk9s1", "Content": "EFI" },
                    { "DeviceIdentifier": "disk9s2", "Content": "Apple_APFS" }
                  ] },
                { "DeviceIdentifier": "disk10", "Size": 3_999_000_000_000u64,
                  "Content": "Apple_APFS_Container",
                  "APFSPhysicalStores": [{ "DeviceIdentifier": "disk9s2" }],
                  "APFSVolumes": [
                    { "DeviceIdentifier": "disk10s1", "VolumeName": "Datas", "MountPoint": "/Volumes/Datas" }
                  ] }
            ]
        });
        let entries = entries_from_json(&value, &[]).unwrap();
        assert_eq!(entries.len(), 1, "容器本身不应再算一个磁盘");
        assert_eq!(
            entries[0].state,
            DiskState::Mounted(PathBuf::from("/Volumes/Datas"))
        );
        assert_eq!(entries[0].status(), "已挂载：/Volumes/Datas");
    }
}
