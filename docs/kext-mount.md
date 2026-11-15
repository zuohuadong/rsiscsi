# 本地只读挂载：自研 kext + iscsi-blockd

本项目不依赖 iSCSIKit 或 libiscsi。macOS 没有用户态块设备接口，所以让 LUN 以
`/dev/disk*` 出现只能靠内核驱动。这里的内核部分是一个自研 kext
（`IOBlockStorageDevice` 子类），协议部分仍是 Rust（`iscsi-core`），两者通过
IOUserClient 的共享内存和请求队列通信。

## 结构

```text
APFS / IOBlockStorageDriver
        │ doAsyncReadWrite / report*
        ▼
kext: ISCSIVirtualDisk            (kext/，C++，只搬运块)
        │ ① IOSharedDataQueue     内核 → 用户：读请求
        │ ② 外部方法 completeIO   用户 → 内核：完成
        │ ③ 共享 arena            数据缓冲区（默认 64 MiB）
        ▼
用户态: iscsi-blockd              (crates/iscsi-blockd，Rust)
        │ iscsi-core::Session
        ▼
   目标 portal（TCP 3260）
```

内核侧不做协议解析，也不访问网络；它只把 `{token, chunk, lba, 长度, arena 偏移}`
交给 daemon，并把 daemon 填好的数据复制回调用方缓冲区。

## 只读保证

- `reportWriteProtection` 恒为 true，`getWriteCacheState` 恒为 false。
- `doAsyncReadWrite` 见到写方向立即以 `kIOReturnNotWritable` 完成，不排队、不下发。
- daemon 只调用 `Session::read_into`，不启用任何缓存、不调用 write/flush。
- 因此第一版只能只读挂载；APFS 无法回放日志，处于 dirty 状态的卷可能拒绝挂载。

## ABI

`kext/ISCSIProtocol.h` 与 `crates/iscsi-blockd/src/abi.rs` 必须保持一致。

| 项 | 值 |
| --- | --- |
| ABI 版本 | 1 |
| 外部方法 | 0 setGeometry、1 completeIO、2 setOnline、3 getConfig |
| 映射类型 | 0 arena、1 requests |
| 单请求上限 | 32 MiB（32 × 1 MiB 分块） |
| 并发请求 | 32 |
| arena 默认大小 | 64 MiB，属性 `ISCSIArenaBytes` 可改 |
| 队列条目 | 1024 |

请求体为 40 字节：`token, chunk, length, block_size, lba, arena_offset, direction, reserved`。

## 前提：启动安全策略

本机没有付费开发者账号，kext 只能 ad-hoc 签名。Apple silicon 上加载这种 kext 需要：

1. 关机后长按电源进入「启动选项」→「启动安全性实用工具」，
   选择「降低安全性」并勾选「允许用户管理来自被认可开发者的内核扩展」。
2. 重新启动，执行加载脚本，按提示到「系统设置 → 隐私与安全性」点击「允许」。
3. 再次执行加载脚本。

脚本不会调用 `csrutil`，也不会修改任何安全设置。SIP 可以保持开启。

## 使用

```sh
# 编译 kext（产物 kext/build/ISCSIVirtualDisk.kext）
./kext/build.sh

# 加载（需要 root，首次加载后要在系统设置里批准）
sudo ./scripts/load-kext.sh

# 运行 daemon：登录后 LUN 才会上线
sudo cargo run -p iscsi-blockd -- \
  --host 10.1.10.1 \
  --iqn iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836 \
  --lun 1

# 另一个终端
diskutil list

# 停止 daemon（Ctrl-C）后卸载
sudo ./scripts/unload-kext.sh
```

daemon 登录成功后才调用 `setGeometry` / `setOnline`，在此之前不创建 IOMedia，
所以 `diskutil` 不会看到空盘。

## 故障排查

```sh
kmutil showloaded | grep iscsi          # kext 是否加载
diskutil list                           # 是否出现新盘
log show --last 5m --predicate 'process == "kernel"' | grep ISCSIVirtualDisk
```

- 加载报 `code signature invalid` 或直接被拒：先降低启动安全策略并批准。
- `daemon` 报找不到服务：kext 未加载，或加载后被拒绝。
- 盘出现但挂载失败：APFS 卷可能处于 dirty 状态，先在 NAS 端正常卸载/修复。
- 读写极慢：当前用 200 µs 轮询队列唤醒 daemon，属于第一版取舍；
  后续可换成 mach 通知端口。

## 已知限制

- 只读。
- 单请求上限 32 MiB，更大的请求返回 `kIOReturnBadArgument`。
- daemon 退出时 kext 会广播媒体离线并失败所有未完成请求，不会悬挂。
- kext 崩溃会影响整机稳定性，所以所有错误路径都返回 IOReturn，不做断言。
