# rsiscsi

[English](README.en.md) | 简体中文

Rust 实现的 iSCSI 发起端：协议引擎 + CLI + 原生桌面应用（GPUI / gpui-kit），
外加可选的自研 kext，用于把 LUN 只读挂载成本地磁盘。
除显式加载自研 kext 外，不修改 iSCSIKit、不安装系统扩展、不更改系统设置。

## 构建与测试

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace
cargo test -p iscsi-core -p iscsi-gui
cargo clippy -p iscsi-core -p iscsi-cli --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p iscsi-cli -- --help
cargo run -p iscsi-gui
```

`gpui-kit` 0.9.1 需要 Rust ≥ 1.100（`std::hint::cold_path`、
`Option::map_or_default`），因此 `rust-toolchain.toml` 固定到本机已验证的 1.100.0。
不更改全局工具链设置。

默认测试只访问本地模拟目标。真实只读烟雾测试需显式设置：

```sh
ISCSI_TEST_HOST=10.1.10.1 ISCSI_TEST_LUN=1 \
  cargo test -p iscsi-core real_target_smoke_env_gated -- --nocapture
```

还可通过 `ISCSI_TEST_IQN` 指定其他 IQN。测试不会写入远端。

## 打包

```sh
./scripts/package-dmg.sh
```

产物是 `dist/rsiscsi-<版本>-<架构>.dmg`。脚本只构建用户态应用，使用 ad-hoc 签名，
不包含 DriverKit 扩展，也不需要关闭 SIP 或修改系统安全设置。
由于没有开发者签名和公证，`spctl` 会拒绝该 DMG。首次启动需在
「系统设置 → 隐私与安全性」中点击「仍要打开」（macOS 15 起已无右键打开的绕过方式），
或执行：

```sh
xattr -dr com.apple.quarantine /Applications/iSCSI.app
```

DMG 内的「使用说明.txt」写有同样的步骤。该版本不创建 `/dev/disk*`，
Finder 与磁盘工具看不到目标磁盘。

## 本地只读挂载（自研 kext）

上面的用户态版本不会创建 `/dev/disk*`。要让 LUN 真正出现在磁盘工具和 Finder 里，
本仓库自带一个自研 kext（`kext/`，`IOBlockStorageDevice` 子类）和一个 Rust daemon
（`crates/iscsi-blockd`）。内核只搬运块，iSCSI 协议仍由 `iscsi-core` 在用户态执行；
不依赖 iSCSIKit，也不链接 libiscsi。

```sh
./kext/build.sh                                  # 产出 kext/build/ISCSIVirtualDisk.kext
sudo ./scripts/load-kext.sh                      # 首次加载后需在系统设置里批准
sudo cargo run -p iscsi-blockd -- \
  --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1
diskutil list                                    # 另一个终端
sudo ./scripts/unload-kext.sh
```

第一版**只读**：kext 对写请求直接返回 `kIOReturnNotWritable`，daemon 只调用
`Session::read_into`。未签名（ad-hoc）kext 在 Apple silicon 上需要先把启动安全策略
降到 Permissive Security 并在系统设置中批准；脚本不会修改系统安全设置。
完整前提、ABI、共享内存布局与排查步骤见 [`docs/kext-mount.md`](docs/kext-mount.md)。

## 代码签名与分发

本项目目前**全部使用 ad-hoc 签名**，没有 Apple Developer Program 付费账号（99 美元/年）。
这带来两个实际影响：

- **App / DMG**：没有 Developer ID 签名与公证，Gatekeeper 会拒绝首次打开，需要在
  「系统设置 → 隐私与安全性」点「仍要打开」，或手动
  `xattr -dr com.apple.quarantine /Applications/iSCSI.app`。
- **内核扩展**：ad-hoc 签名的 kext 在 Apple silicon 上必须先降低启动安全策略
  （Permissive Security）并在系统设置中批准，系统大版本升级后通常还要重新批准。

计划：当本仓库的下载量、Star 或捐赠累计足以覆盖开发者年费时，会立即开通
Apple Developer Program，用 Developer ID Application 证书签名 App 与 DMG 并提交公证，
消除上面的第一步门槛，并改用正式签名的 kext。

需要说明的是，**开发者账号无法消除 kext 的启动安全策略要求**：Apple silicon 上任何
第三方 kext（无论签名与否）都需要用户批准并降低启动安全策略。彻底解决要把内核部分
迁到 DriverKit System Extension（dext），这同样需要付费账号，是本项目的长期方向。

愿意支持本项目（Star、Issue 反馈或捐赠）请通过仓库主页联系作者；不承诺具体时间表。

## 发布与下载

CI 在推送 `v*` 标签或手动触发时构建 DMG，上传到城通网盘的
「软件集合/rsiscsi/<版本>/」目录，**不在 GitHub Release 附加二进制**。原因有两条：

- **国内下载**：GitHub Release 的下载在国内经常不可达或极慢，网盘链接可以直接访问。
- **支持开发**：下载集中到同一个分享页，便于累积赞助来覆盖 Apple Developer Program
  年费（当前全部为 ad-hoc 签名，见上一节）。

发布默认走城通的 REST API（`scripts/publish-ctfile.sh`），只需要会话令牌，缺失的目录会
自动创建。需要的仓库 Secrets：

| Secret | 说明 |
|---|---|
| `CTFILE_SESSION` | 已登录城通网页端的会话令牌 |
| `CTFILE_FOLDER_ID` | 父目录的数字 id，`0` 表示网盘根目录 |

目标目录用仓库变量 `SOFTWARE_COLLECTION_DIR` 覆盖，默认 `软件集合/rsiscsi`；这里的路径
始终相对于 `CTFILE_FOLDER_ID`，若父目录就是根目录则直接写 `软件集合/rsiscsi`。

没有会话令牌时，CI 会退回到 WebDAV（`scripts/publish-webdav.sh`），此时需要
`WEBDAV_URL`、`WEBDAV_USERNAME`、`WEBDAV_PASSWORD`、`WEBDAV_ROOT_PATH` 四个 Secrets；
这条路径下 `SOFTWARE_COLLECTION_DIR` 若以 `/` 开头则忽略 `WEBDAV_ROOT_PATH`。

CI 只在打标签或手动触发时运行，不消耗每次推送的 macOS 分钟数；本地仍可用
`./scripts/package-dmg.sh` 生成同样的 DMG。

## CLI

```sh
cargo run -p iscsi-cli -- discover --host 10.1.10.1
cargo run -p iscsi-cli -- ls --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836
cargo run -p iscsi-cli -- inq --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 --lun 1
cargo run -p iscsi-cli -- read --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1 --lba 0 --blocks 256 --out target/block.bin
cargo run -p iscsi-cli -- bench --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1 --pattern seq-read --bs 1MiB --qd 1,16,32 --duration 5 --json
```

`--json` 的成功结果写入 stdout，错误 JSON 写入 stderr，Cargo/日志不混入结果。
二进制名称为 `rsiscsi`，Cargo 包名为 `iscsi-cli`。
退出码：0 成功、2 参数、3 连接、4 登录、5 已建立会话的 I/O。

全局开关包括 `--nodelay=false`、`--header-digest`、`--data-digest`。
`--queue-depth` 限制会话在途命令（默认 128），`--qd` 选择基准测试的并发深度，
不得超过会话限制。`--warmup` 默认 1 秒，`--duration` 是正式测试秒数。
块大小支持 B、KiB、MiB、GiB，十进制 KB/MB；必须与逻辑块大小对齐。

CHAP 可用 `--chap-user` / `--chap-secret` 指定；命令行参数可能被同机进程查看，
请勿将凭据写入脚本、日志或提交。协议结构的 Debug 不显示 secret。

### 写入安全

`seq-write` / `rand-write` 会永久覆盖所选区域，包括预热阶段。
CLI 和核心 `benchmark` 同时要求显式破坏性确认及区域：
`--allow-destructive-write --start-lba <起始块> --range-blocks <块数>`。
没有确认时，在连接之前拒绝；核心库也会在任何 SCSI I/O 之前拒绝。
本次未获知可丢弃的真实 LBA 区域，故没有执行真实 NAS 写入测试。
写入协议（全即时数据、即时数据加多轮 R2T、无即时数据、CRC32C、缓存同步）
和两种写入基准均在本地模拟目标上验证。

### 本地缓存

**默认策略：读缓存按自动容量开启，写回关闭。** 与目标 MODE SENSE 页面中的 WCE/RCD 不同：
这是每个会话独有的客户端内存，不会修改目标缓存策略，也不跨进程、重连或目标共享。

- 读缓存：`--cache-size auto|<大小>|off`。`auto`（read 子命令的默认值）取本机内存的
  1/8，限制在 128 MiB–2 GiB，按 64 MiB 向下取整；取不到内存信息时退回 512 MiB。
  `bench` 默认 `off`，避免本地命中掩盖 LUN 真实吞吐；显式开启后吞吐数字含义随之改变。
  固定 64 KiB 段、按字节预算的 LRU；
  仅字节偏移及长度都按段对齐时缓存，连续缺失段合并读取，每次不超过 1 MiB。
  非对齐请求整条透传。小于一个段的容量不保存任何段。
- 默认写穿：`--write-back off`（默认）。写入立即发送到 LUN，并失效重叠的本地读缓存；
  不额外发送 SYNCHRONIZE CACHE，保持原有完成语义。写穿不等于目标掉电持久化。
- **危险的写回**：`--write-back <容量|auto>` 必须同时提供 `--allow-destructive-write`；
  `auto` 取推荐值：自动读容量的 1/4，限制在 64–256 MiB。
  对齐写入可以仅进内存就返回。超过脏容量、显式 `flush`、`logout`，或
  `--flush-interval <毫秒>` 到期时，按 LBA 升序合并写入，再统一发送一次
  SYNCHRONIZE CACHE（group commit）。只有全部成功才清除脏数据。
  非对齐写入先刷写旧脏数据，再以原请求写穿，不做读改写或拆分。

**崩溃或断电会丢失未刷写的写入。** 写回不是持久日志、事务或远端共享缓存；
最后一个 Session 直接被丢弃也不能异步保证刷写。使用方必须显式 `flush` /
`logout` 并处理错误。`logout(self)` 失败需要重试时，调用方必须保留另一个
Session 克隆；GUI 会保留原句柄，刷写失败会记录日志并阻止断开、切换目标和关窗。
CLI 刷写退出失败会返回 5，同时报告未刷写字节数及进程退出造成的内存数据丢失；
CLI 不提供持久化恢复或隐式重试，不应把失败退出视为数据已落盘。
进程被强制终止不能拦截。普通读缓存也不具备多发起端一致性：
其他客户端修改 LUN 不会自动失效此内存缓存，只应在数据稳定或独占访问时开启。

```sh
# 同一会话内读两遍；只读，不启用写回。
cargo run -p iscsi-cli -- read --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1 --lba 0 --blocks 256 --out target/cached-read.bin \
  --cache-size 1MiB --repeat 2 --json
```

`hits` / `misses` 按**字节**计数，命中率为 `hits / (hits + misses)`；
`misses` 是实际成功从 LUN 读到的字节，包含非对齐透传读，
`bytes_served` 是从本地读缓存或脏段提供的字节。关闭时统计全部为零。
`bench --json` 新增 `cache` 字段，原结果字段保持不变。
`read --repeat` 默认 1，两次独立启动 CLI 不会共享缓存。

Rust API 为 `Session::enable_cache`、`read_cached`、`write_cached`、`cache_stats`；
`read_pooled` / `read_buffer` 保持原有池化直读路径。原有写 API 始终写穿；
已有脏数据时先刷写以保持顺序。修改配置仅限空闲、无脏数据时；
`try_enable_cache` 返回错误，兼容的 `enable_cache` 则记录错误并保留原配置。
定时刷写错误可通过 `cache_flush_error()` 观察，成功重试后清除。

GUI 基准页读缓存默认「自动（推荐）」，写回默认关闭（预选「推荐（自动）」）；
容量以下拉预设选择：读缓存 64 MiB–1 GiB，写回 64–512 MiB，自动/推荐值旁注明实际 MiB。
写回需要再次确认「允许写入」，会话页仅在缓存开启时显示容量、命中率与脏数据。

大块顺序读、工作集超过缓存或很少重复访问时通常没有收益，反而多一次内存拷贝。
此前 NAS 后端约 165 MiB/s 的只读对照不能通过低命中率的客户端缓存消除。
测量方法及本轮只读验收见 [`docs/local-cache.md`](docs/local-cache.md)。

### 磁盘可见性

连接只建立 SCSI 会话，不创建 `/dev/disk*`，也不挂载文件系统，所以磁盘工具和 Finder
看不到它。要让 LUN 以块设备出现，需要加载自研 kext 并运行 `iscsi-blockd`，
见上文「本地只读挂载（自研 kext）」。连接成功后，「会话」页的「磁盘」区块会只读扫描 `diskutil`：

- 新出现且无分区表的盘标记「未初始化」，提供「打开磁盘工具」；
  初始化由用户在磁盘工具里确认，应用绝不自动抹掉。
- 已有文件系统并挂载的盘显示卷挂载点，提供「在 Finder 中打开」。
- 扫描只调用 `diskutil list -plist` 并转成 JSON，不写任何磁盘；
  「刷新」可重新扫描，连接断开时清空结果。

## 核心设计

- 每连接一个 Tokio socket owner，mpsc 入队、Semaphore 限流、ITT 槽位和代际编号；
  标签不在连接内重用，命令服从经过序号回绕校验的目标窗口。
- 每批请求、每轮 R2T 的 PDU 编码到可复用发送缓冲区，统一 `write_all`。
- `read_pooled` / `read_buffer` 直接将 Data-In 收到拥有所有权的池化缓冲区，
  不经过中间 payload 缓冲。缓存关闭时基准 worker 循环复用同一缓冲区；
  显式启用缓存后使用 `read_cached` / `write_cached`，包含缓存层内存拷贝。
- 安全 Rust 无法将任意短生命周期 `&mut [u8]` 转移到独立 `'static` socket task；
  因此兼容接口 `read_into` 保留一次最终拷贝，`read` 也会分配返回 Vec。
  oneshot 等任务通信仍有元数据分配，不宣称整个命令路径零分配。
- Digest 默认关闭；启用时由 `crc32c` 实现，校验覆盖 padding，线上的 digest 为小端。
  处理可选 AHS 的 framing/header digest；当前 target response opcodes 不定义 AHS，
  收到 target AHS 时校验后明确拒绝，不将其静默忽略。
- 支持单连接、ERL 0、InitialR2T=Yes、顺序 DataPDU/DataSequence。
  支持目标拒绝 ImmediateData；不支持重连恢复、双向 SCSI、双向 CHAP 或任务管理。
- 协议错帧、digest 错误、超时会终止连接并释放所有等待者；
  未发送的取消请求被丢弃，已发送命令取消后按 ERL 0 终止连接，不重试写入。
  已发送写入超时/取消后的远端完成状态不确定，不提供事务回滚语义。
- `SessionStats` 提供三秒时间常数的 EWMA，原始计数器使用 atomics。
- 基准使用统一测量时钟；预热排除在结果外，在途命令完整回收，
  分母截止到最后一次 I/O 完成，不包括直方图聚合。
  HDR 直方图为三位有效数字；p50/p95/p99 不是无界逐命令样本列表。
  写入结果不含后续 SYNCHRONIZE CACHE 的时间，该时间单独记录。

### 地址解析

连接入口始终使用 `Target { addr, iqn, lun, chap }`，不会按冒号拆 IQN。
`Target::parse_url` 解析结构，`Target::to_url` 输出明确带 port/LUN 的规范形式。
需要原始 URL 字节级往返时使用 `TargetUrl::parse(...).as_str()`；
它保留省略的默认值、大小写、前导零和百分号文本，不在加载时规范化。
历史 `iscsi://iqn...` 形式必须另行提供 fallback portal。
URL 和 `to_url()` 的返回值可能含凭据，不得记录到日志。

## 桌面应用

`cargo run -p iscsi-gui` 打开 900×620 的紧凑窗口（深色单色主题，参考 Volt Native）：
左侧是已保存目标与 SendTargets 发现，右侧通过「目标 / 会话 / 基准」切换。
目标保存在 `~/Library/Application Support/rsiscsi/targets.toml`（目录 0700、文件 0600，
临时文件 + rename 原子写入），IQN 原样保存、不做规范化。
连接、INQUIRY/容量读取和实时速率（1 Hz EWMA）在独立 Tokio 任务中执行，
结果通过 channel 回到 UI 线程，不阻塞渲染。

界面中英双语，启动时按系统语言自动选择（`zh-Hans`/`zh-TW` 等显示中文，其余显示英文）。
需要固定语言时可用 `RSISCSI_LANG=zh` 或 `RSISCSI_LANG=en` 覆盖，不影响系统设置。

写入测试默认不可见：只有选择顺序写/随机写后才会出现
「允许写入（会破坏 LUN 上的数据）」确认项，未确认时运行按钮禁用并显示红色警告。
读取默认限制在 LUN 前 16 GiB 窗口内；连接与信息读取阶段不发送写入或 SYNCHRONIZE CACHE。
未知数据区域不得写入。

## 文件布局

```text
crates/iscsi-core/Cargo.toml          核心包依赖
crates/iscsi-core/src/lib.rs          公共 API 与所有权说明
crates/iscsi-core/src/config.rs       目标结构、无损 URL、连接及协商参数
crates/iscsi-core/src/error.rs        类型化错误
crates/iscsi-core/src/wire.rs         BHS/PDU、CRC32C、文本键、CHAP 编解码与单元测试
crates/iscsi-core/src/login.rs        TCP 调优、分阶段登录、CHAP 和参数/窗口协商
crates/iscsi-core/src/session.rs      会话 API、socket owner、ITT 流水线与 SCSI 传输
crates/iscsi-core/src/cache.rs        可开关的段 LRU、脏段、统计与自动容量
crates/iscsi-gui/src/disks.rs         只读 diskutil 探测：空白盘引导与 Finder 打开
crates/iscsi-core/src/scsi.rs         CDB、LUN、INQUIRY/VPD 和容量解析
crates/iscsi-core/src/pool.rs         有界可复用缓冲区缓存
crates/iscsi-core/src/stats.rs        原子计数与 EWMA 采样
crates/iscsi-core/src/bench.rs        四种模式、预热、QD 扫描、HDR 延迟与吞吐量
crates/iscsi-core/tests/mock_target.rs 本地协议、并发、取消和基准集成测试
crates/iscsi-cli/Cargo.toml           CLI 包和 rsiscsi 二进制定义
crates/iscsi-cli/src/main.rs          中文 clap 命令、JSON、文件输出、退出码
crates/iscsi-gui/Cargo.toml           GUI 包依赖（gpui-kit 0.9.1）
crates/iscsi-gui/src/i18n.rs           中英双语文案与系统语言探测
crates/iscsi-gui/src/main.rs          深色主题、窗口尺寸与 GPUI 启动
crates/iscsi-gui/src/state.rs         目标模型、配置原子读写、基准参数与安全校验
crates/iscsi-gui/src/engine.rs        Tokio 运行时桥、事件通道、取消与断开
crates/iscsi-gui/src/views/mod.rs     窗口骨架：标题栏、侧栏、标签、状态栏
crates/iscsi-gui/src/views/sidebar.rs 目标列表与发现面板
crates/iscsi-gui/src/views/editor.rs  目标编辑表单与 URL 导入
crates/iscsi-gui/src/views/session.rs 会话控制、设备信息、磁盘引导、协商参数与日志
crates/iscsi-gui/src/views/benchmark.rs 基准表单、结果与吞吐量柱状图
crates/iscsi-gui/src/views/updates.rs 后台事件到 UI 状态的归约
packaging/Info.plist                  App 包元数据（版本号由脚本替换）
packaging/使用说明.txt                DMG 内的安装与限制说明
scripts/package-dmg.sh                构建 .app、ad-hoc 签名并生成 DMG
```

真实目标验证数据见 `docs/phase1-verification.md`。

## 许可证

本项目以 GNU Affero General Public License v3.0 或更新版本发布，
完整条款见 [`LICENSE`](LICENSE)。通过网络提供本项目的修改版本时，
同样需要提供对应源码。
