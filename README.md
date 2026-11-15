# iscsi-rs

Rust 用户态 iSCSI 发起端。当前交付为 phase 1：协议引擎和 CLI，GUI 保留原始骨架。
不会创建、挂载块设备，也不会修改 iSCSIKit、安装系统扩展或更改系统设置。

## 构建与测试

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -p iscsi-core -p iscsi-cli
cargo test -p iscsi-core -p iscsi-cli
cargo clippy -p iscsi-core -p iscsi-cli --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p iscsi-cli -- --help
```

本机默认 `stable` 缺少可用的 `rustc`，因此 `rust-toolchain.toml` 固定到本机已经安装、
验证可运行的 Rust 1.97.1。不更改全局工具链设置。

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

## 核心设计

- 每连接一个 Tokio socket owner，mpsc 入队、Semaphore 限流、ITT 槽位和代际编号；
  标签不在连接内重用，命令服从经过序号回绕校验的目标窗口。
- 每批请求、每轮 R2T 的 PDU 编码到可复用发送缓冲区，统一 `write_all`。
- `read_pooled` / `read_buffer` 直接将 Data-In 收到拥有所有权的池化缓冲区，
  不经过中间 payload 缓冲。基准 worker 循环复用同一缓冲区。
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
phase 1 不创建或加载 GUI 配置文件。

## 文件布局

```text
crates/iscsi-core/Cargo.toml          核心包依赖
crates/iscsi-core/src/lib.rs          公共 API 与所有权说明
crates/iscsi-core/src/config.rs       目标结构、无损 URL、连接及协商参数
crates/iscsi-core/src/error.rs        类型化错误
crates/iscsi-core/src/wire.rs         BHS/PDU、CRC32C、文本键、CHAP 编解码与单元测试
crates/iscsi-core/src/login.rs        TCP 调优、分阶段登录、CHAP 和参数/窗口协商
crates/iscsi-core/src/session.rs      会话 API、socket owner、ITT 流水线与 SCSI 传输
crates/iscsi-core/src/scsi.rs         CDB、LUN、INQUIRY/VPD 和容量解析
crates/iscsi-core/src/pool.rs         有界可复用缓冲区缓存
crates/iscsi-core/src/stats.rs        原子计数与 EWMA 采样
crates/iscsi-core/src/bench.rs        四种模式、预热、QD 扫描、HDR 延迟与吞吐量
crates/iscsi-core/tests/mock_target.rs 本地协议、并发、取消和基准集成测试
crates/iscsi-cli/Cargo.toml           CLI 包和 iscsi-rs 二进制定义
crates/iscsi-cli/src/main.rs          中文 clap 命令、JSON、文件输出、退出码
crates/iscsi-gui/Cargo.toml           原有 GUI 依赖，未修改
crates/iscsi-gui/src/main.rs          原有 skeleton，未修改
```

真实目标验证数据见 `docs/phase1-verification.md`。
