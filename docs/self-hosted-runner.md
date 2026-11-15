# 自托管 runner 发布（备选）

> 当前发布走国内反向代理，见 [`reverse-proxy.md`](reverse-proxy.md)。本文保留给没有可用
> 反代的场景。

城通对 WebDAV 登录做了地区风控，境外 IP 会收到
`Access denied: Login from different location detected`。因此发布 job
（`.github/workflows/release.yml` 里的 `publish`）必须在国内网络的机器上执行。

## 机器要求

- 一台国内网络可达的 Mac，能 7×24 开机（开发机也可以，睡眠会导致任务排队）
- 只需要 `curl`、`python3`、`shasum`，**不需要** Xcode、Rust 或本仓库的构建产物
- 构建仍在 GitHub 托管的 `macos-latest` 上完成，产物通过 artifact 传给这台机器

## 安装 runner

在仓库 Settings → Actions → Runners → New self-hosted runner 选择 macOS，
按页面给出的命令执行，注意在 `config.sh` 时加上标签：

```sh
./config.sh --url https://github.com/zuohuadong/rsiscsi --token <页面上的 token> \
  --labels rsiscsi-publish
```

默认标签已包含 `self-hosted` 与 `macOS`，工作流要求三者齐全：

```yaml
runs-on: [self-hosted, macOS, rsiscsi-publish]
```

安装为开机自启服务：

```sh
./svc.sh install
./svc.sh start
./svc.sh status
```

## 触发发布

只有 tag push 会进入 `publish` job（`if: github.ref` 以 `refs/tags/v` 开头），
手动 `workflow_dispatch` 只构建、不发布。

```sh
git tag -a v0.1.1 -m "rsiscsi 0.1.1"
git push origin v0.1.1
```

成功后可核对远端目录：

```sh
curl -s -X PROPFIND --user "$WEBDAV_USERNAME:$WEBDAV_PASSWORD" \
  -H 'Depth: 1' "$WEBDAV_URL/$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))' 软件集合/rsiscsi/0.1.1)/"
```

## 没有 runner 时

本地执行同等流程，凭据放在 `.env.release`（见 `.env.release.example`）：

```sh
./scripts/release-local.sh
```

## 安全注意

- 仓库是公开的，自托管 runner **不要**允许来自 fork 的 pull request 运行：
  Settings → Actions → General → 确认 fork PR 的工作流需要批准。
- `publish` job 只读取仓库 Secrets，不向仓库写任何内容，权限保持 `contents: read`。
- runner 所在机器要能访问 `dav.ctfile.com`，公司网络或代理可能触发同一风控。

## 故障排查

| 现象 | 原因 |
|---|---|
| `403 Access denied: Login from different location detected` | 出口 IP 不在国内，或使用了境外代理 |
| `401 Unauthorized` | `WEBDAV_USERNAME` / `WEBDAV_PASSWORD` 错误 |
| `MKCOL ... 403` 但本地正常 | runner 走的是代理出口，检查系统代理设置 |
| job 一直 queued | runner 离线或标签不匹配，检查 `./svc.sh status` 与 Settings → Runners |
