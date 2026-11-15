# Phase 1 验证记录

日期：2026-11-15，macOS arm64，Rust 1.97.1，Cargo dev 构建（非 release）。
范围只有 `iscsi-core` / `iscsi-cli`。没有改动 GUI、系统应用或系统扩展，没有挂载 LUN。

## 确定性检查

- `cargo build -p iscsi-core -p iscsi-cli`：通过。
- `cargo test -p iscsi-core -p iscsi-cli`：15 项核心单元测试、11 项本地模拟目标测试、
  3 项 CLI 测试全部通过。默认网络测试在未设置环境变量时直接跳过。
- `cargo clippy -p iscsi-core -p iscsi-cli --all-targets -- -D warnings`：通过。
- `cargo fmt --all` 已执行。
- `cargo run -p iscsi-cli -- --help`：通过，包名和二进制名不同是有意设计。
- 显式设置 `ISCSI_TEST_HOST=10.1.10.1 ISCSI_TEST_LUN=1` 后，真实烟雾测试通过：
  登录、INQUIRY/VPD、容量、读 1 块、退出。

本地模拟器覆盖流水线先发多命令后收结果、乱序完成、NOP、完整即时数据写入、
多轮 R2T/Data-Out、ImmediateData=No、CRC32C、CHAP 二进制挑战、分轮参数协商、
非法偏移、超时关闭、取消排队写入、关闭窗口仍可退出，以及四种基准模式。
窗口单元测试覆盖陈旧/非法广告、关闭窗口和 u32 序号回绕。

## 真实目标

```text
地址       10.1.10.1:3260
IQN        iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836
LUN        1
认证       无 CHAP
厂商       QNAP
产品       iSCSI Storage
版本       300
容量       8589934592 × 512 = 4398046511104 字节（4 TiB）
```

已通过 CLI 执行 discovery、REPORT LUNS（返回 `[1]`）、标准及 VPD INQUIRY、
READ CAPACITY(10)/(16)、READ(16)、Logout。
额外读取 LBA 0、256 块，得到 131072 字节，保存在未跟踪的 `target/qnap-read.bin`，
不将远端原始数据加入提交。

实际协商参数：

| 参数 | 实测值 |
| --- | --- |
| HeaderDigest / DataDigest | None / None |
| 发起端 / 目标 MaxRecvDataSegmentLength | 262144 / 262144 |
| MaxBurstLength / FirstBurstLength | 262144 / 262144 |
| InitialR2T / ImmediateData | Yes / Yes |
| DefaultTime2Wait / DefaultTime2Retain | 0 / 0 |
| ErrorRecoveryLevel | 0 |
| MaxConnections / MaxOutstandingR2T | 1 / 1 |

启用双 CRC32C 的真实请求被拒绝：目标未接受请求的 digest，客户端报登录错误并退出 4，
没有静默降级。CRC32C 的 encode/verify 数据路径由本地带 digest 的目标实测覆盖。

## 性能

最终代码测量：1 MiB 顺序读，每个 QD 预热 1 秒、正式运行 5 秒，完整排空在途请求。
延迟含客户端排队与完整 I/O 时间，单位毫秒。

| QD | 命令数 | 实际秒数 | MiB/s | IOPS | p50 ms | p95 ms | p99 ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 576 | 5.001660625 | 115.16 | 115.16 | 4.125 | 15.368 | 192.807 |
| 16 | 1014 | 5.055389167 | 200.58 | 200.58 | 56.820 | 181.666 | 287.834 |
| 32 | 1112 | 5.111605875 | 217.54 | 217.54 | 113.508 | 308.281 | 430.703 |

完整精度的 CLI JSON 结果保存在 `phase1-seq-read.json`。
QD16 超过要求的 50 MiB/s。1 MiB 请求下 IOPS 数值恰好等于 MiB/s。
另以 `--nodelay=false --qd 16 --duration 1` 实跑人类可读表格输出，
得到 99.25 MiB/s、99.25 IOPS，确认该 socket 开关和表格输出可用；
这次一秒短测不并入上面的五秒矩阵。
此前两轮 QD1/16/32 分别测得 146.85/159.59/151.49 和 41.88/141.75/197.53 MiB/s，
说明短测受 NAS/缓存/网络波动影响；
这里不将最好成绩当作最终唯一结果。

读取区域从 LBA 0 开始，未控制远端缓存，也未判断稀疏/未分配区域。
这些数字是用户态协议端到端的短时读吞吐，不代表物理磁盘持续带宽。

交叉检查：

```sh
iscsi-ls -s iscsi://10.1.10.1:3260
iscsi-inq iscsi://10.1.10.1:3260/iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836/1
iscsi-readcapacity16 iscsi://10.1.10.1:3260/iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836/1
iscsi-perf -m 16 -b 2048 -t 5 -l iscsi://10.1.10.1:3260/iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836/1
```

libiscsi 的目标名、LUN、厂商/产品和 READ CAPACITY(16) 完全一致。
`iscsi-ls -s` 的简略容量显示为 `1T`，与其自身 READ CAPACITY(16) 的 4 TiB 不一致；
以完整 READ CAPACITY(16) 结果为准，不掩盖这项工具显示差异。
`iscsi-perf` 成功退出，平均约 80 IOPS、80 MiB/s。该工具打印 `MB/s`，
源码实际使用右移 20 位换算，因此这里记为 MiB/s。它没有使用本客户端的预热，
不作为受控、等条件的性能排名。

## 验收边界

- 真实 NAS 写入吞吐：未测。没有用户指定的可丢弃写入区域，不冒险覆盖既有 LUN。
  写入、flush 和写基准的本地协议验证已完成，但不等于 NAS 写入性能验收。
- 真实 CHAP：NAS 无 CHAP，只有固定向量与本地挑战握手证据。
- 真实 CRC32C：NAS 未接受双 digest，不能宣称已通过该目标的 digest 性能验收。
- GUI：按 phase 1 要求保留骨架；未构建完整 workspace、未启动或进行界面验收。
- 不提供 DriverKit、块设备、挂载、重连恢复、任务管理功能。
- 借用的 `read_into` 接口有一次最终拷贝，池化拥有型 API 无中间 payload 拷贝；
  不宣称整个热路径零分配。

## 审查

同一只读 verifier 的两轮审查聚焦协议、取消与基准真实性。第一轮发现的取消写入、
退出窗口、窗口前进、分轮 burst 参数、测量时钟和 ERL 0 A 位问题均已修复并补测试。
第二轮确认其中五项，另外指出测量窗口尾部可能被截短；随后保证测量分母不小于
指定测试时长，增加确定性尾部/排空测试并重跑上述构建、测试、lint 和真实性能测试。

clean-code-guard 检查结果：

- `crates/iscsi-core/Cargo.toml` / `crates/iscsi-cli/Cargo.toml`：删除未使用的直接依赖。
- `crates/iscsi-core/src/session.rs`：退出失败保留类型化错误，不丢失在任务退出中。
- `crates/iscsi-cli/src/main.rs`：区分已建立会话的 I/O 错误和配置错误退出码。
- `crates/iscsi-core/src/bench.rs`：测量时钟统一，分母覆盖完整时间窗口和在途请求。
- 保留的维护性事项：登录、PDU 分发和 CLI 分发函数仍偏长；本轮未为行数目标
  对已通过协议测试的状态机进行大规模重构。

clean-code-guard: 4 fixed, 1 flagged for author

## NFS 对照（2026-11-15）

同一台 NAS、同一台 Mac（10GbE，RTT 0.44 ms），全部只读。

目标端只注册了 NFSv4（`rpcinfo` 显示 `program 100003 version 3 is not available`，
`low version = 4, high version = 4`），因此 macOS 只能用 `vers=4.1` 挂载；
默认参数会先试 v3，直接报 `Program version wrong`。

```sh
mount_nfs -o ro,vers=4.1 10.1.10.1:/Data /tmp/nfs-cached
```

NFS 侧测量脚本 `/tmp/nfs_bench.py`：`fcntl(F_NOCACHE)` 绕过本机页缓存，
N 个线程各自 `pread` 1 秒预热后计时；iSCSI 侧用 `iscsi-rs bench`，同样 1 秒预热、6 秒正式。

| 场景 | iSCSI | NFS |
| --- | --- | --- |
| 顺序 128 KiB QD8 | 168.7 MiB/s，1350 IOPS，p50 3.55 ms，p95 12.75 ms | 163.2 MiB/s，1306 IOPS，p50 3.66 ms，p95 15.51 ms |
| 顺序 1 MiB QD8 | 152.8 MiB/s，p50 28.5 ms，p95 146 ms | 冷读 118.5 MiB/s（p50 62 ms）；重复读 252–291 MiB/s（p50 28 ms） |
| 随机 4 KiB QD1 | 211 IOPS，p50 0.41 ms，p95 32.7 ms | 2484 IOPS，p50 0.35 ms，p95 0.79 ms |
| 随机 4 KiB QD16 | 1685 IOPS，p50 2.40 ms，p95 51.0 ms | 2830 IOPS，p50 2.26 ms，p95 23.2 ms |
| 本机页缓存命中（参考） | 无此层 | 4 KiB QD16 约 12.9k IOPS（受 Python 线程开销限制） |

解读时需要注意三点，否则容易得出错误排名：

1. 128 KiB 下两者几乎相同（169 对 163 MiB/s），说明 10GbE + NAS 后端的上限就在
   165 MiB/s 附近，协议本身不是瓶颈。
2. NFS 在 1 MiB 上的高值是 NAS 页缓存命中的结果：同一 2 GiB 文件被反复读后从
   118 升到 252–291 MiB/s，而 iSCSI 读的是 LUN 裸块，不存在这层命中。
3. 4 KiB 随机不可直接对比：NFS 读的是已被缓存的文件，iSCSI 在 4 TiB 裸盘上随机，
   尾部延迟（p95 33–51 ms）来自机械盘寻道，不代表协议开销。

结论：日常文件访问用 NFS，客户端自带页缓存、文件语义和成熟的错误恢复；
iSCSI 用于顺序大块搬运和自有客户端验证，在 128 KiB 粒度上与 NFS 持平。
在用户态给自有客户端加块缓存，最多只能追平 NFS 客户端已免费提供的那层能力。

