# iscsi-rs

Rust 用户态 iSCSI 发起端：协议引擎 + CLI + 原生桌面应用（GPUI / gpui-kit）。
不会创建、挂载块设备，也不会修改 iSCSIKit、安装系统扩展或更改系统设置。

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
二进制名称为 `iscsi-rs`，Cargo 包名为 `iscsi-cli`。
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

**默认全部关闭**，与目标 MODE SENSE 页面中的 WCE/RCD 不同：这是每个会话独有的
客户端内存，不会修改目标缓存策略，也不跨进程、重连或目标共享。

- 读缓存：`--cache-size 64MiB`。固定 64 KiB 段、按字节预算的 LRU；
  仅字节偏移及长度都按段对齐时缓存，连续缺失段合并读取，每次不超过 1 MiB。
  非对齐请求整条透传。小于一个段的容量不保存任何段。
- 默认写穿：`--write-back 0`。写入立即发送到 LUN，并失效重叠的本地读缓存；
  不额外发送 SYNCHRONIZE CACHE，保持原有完成语义。写穿不等于目标掉电持久化。
- **危险的写回**：`--write-back <容量>` 必须同时提供 `--allow-destructive-write`。
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

GUI 基准页提供独立开关和 MiB 容量，默认关闭、0；运行时应用配置。
写回需要再次确认「允许写入」，会话页仅在缓存开启时显示命中率与脏数据。

大块顺序读、工作集超过缓存或很少重复访问时通常没有收益，反而多一次内存拷贝。
此前 NAS 后端约 165 MiB/s 的只读对照不能通过低命中率的客户端缓存消除。
测量方法及本轮只读验收见 [`docs/local-cache.md`](docs/local-cache.md)。

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
目标保存在 `~/Library/Application Support/iscsi-rs/targets.toml`（目录 0700、文件 0600，
临时文件 + rename 原子写入），IQN 原样保存、不做规范化。
连接、INQUIRY/容量读取和实时速率（1 Hz EWMA）在独立 Tokio 任务中执行，
结果通过 channel 回到 UI 线程，不阻塞渲染。

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
crates/iscsi-core/src/cache.rs        默认关闭的段 LRU、脏段及统计
crates/iscsi-core/src/scsi.rs         CDB、LUN、INQUIRY/VPD 和容量解析
crates/iscsi-core/src/pool.rs         有界可复用缓冲区缓存
crates/iscsi-core/src/stats.rs        原子计数与 EWMA 采样
crates/iscsi-core/src/bench.rs        四种模式、预热、QD 扫描、HDR 延迟与吞吐量
crates/iscsi-core/tests/mock_target.rs 本地协议、并发、取消和基准集成测试
crates/iscsi-cli/Cargo.toml           CLI 包和 iscsi-rs 二进制定义
crates/iscsi-cli/src/main.rs          中文 clap 命令、JSON、文件输出、退出码
crates/iscsi-gui/Cargo.toml           GUI 包依赖（gpui-kit 0.9.1）
crates/iscsi-gui/src/main.rs          深色主题、窗口尺寸与 GPUI 启动
crates/iscsi-gui/src/state.rs         目标模型、配置原子读写、基准参数与安全校验
crates/iscsi-gui/src/engine.rs        Tokio 运行时桥、事件通道、取消与断开
crates/iscsi-gui/src/views/mod.rs     窗口骨架：标题栏、侧栏、标签、状态栏
crates/iscsi-gui/src/views/sidebar.rs 目标列表与发现面板
crates/iscsi-gui/src/views/editor.rs  目标编辑表单与 URL 导入
crates/iscsi-gui/src/views/session.rs 会话控制、设备信息、协商参数与日志
crates/iscsi-gui/src/views/benchmark.rs 基准表单、结果与吞吐量柱状图
crates/iscsi-gui/src/views/updates.rs 后台事件到 UI 状态的归约
```

真实目标验证数据见 `docs/phase1-verification.md`。
