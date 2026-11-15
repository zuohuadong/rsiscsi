# 国内反向代理

城通对 WebDAV 登录做了地区风控，境外 IP 会收到
`Access denied: Login from different location detected`。CI 在 GitHub 托管的运行器上执行，
出口在境外，因此 `WEBDAV_URL` 指向一台国内 NAS 上的反向代理，由它转发到
`dav.ctfile.com`。

## 结构

```
GitHub runner ──https──> zmhome.mycloudnas.com:2087 ──https──> dav.ctfile.com:443
                          (国内 NAS, Apache 反代)
```

仓库只需要一个 Secret：`WEBDAV_URL=https://zmhome.mycloudnas.com:2087`。

## QNAP 配置

控制台 → 应用程序 → 反向代理 → 添加：

| 字段 | 值 |
|---|---|
| 来源协议 / 主机名 / 端口 | HTTPS / `zmhome.mycloudnas.com` / `2087` |
| 目标协议 / 主机名 / 端口 | HTTPS / `dav.ctfile.com` / `443` |
| 证书 | 该域名的 Let's Encrypt 证书 |

路由器把外部 2087 映射到 NAS 的内网 2087。443 已被 QTS 管理界面占用，所以另开端口。

## 必须修正 ProxyPreserveHost

QTS 为每条规则生成 `ProxyPreserveHost on`。对 TLS 后端，Apache 会把**收到的 Host**
当作 SNI 发给上游，于是握手时通告的是 `zmhome.mycloudnas.com`，城通直接断开连接，
表现为：

```
[proxy:error] AH01084: pass request body failed to 124.160.151.82:443 (dav.ctfile.com)
[proxy:error] AH00898: Error during SSL Handshake with remote server
```

处理方式：让生成器只对明文 HTTP 后端保留该指令。

```sh
ssh admin@<NAS>
sudo -i
cp /etc/init.d/reverse_proxy.sh /etc/init.d/reverse_proxy.sh.bak
```

把

```sh
    data+="\tProxyPreserveHost on\n"
```

替换为

```sh
    if [ "$des_protocol" != https ] && [ "$des_protocol" != wss ]; then
        data+="\tProxyPreserveHost on\n"
    fi
```

然后重启反向代理：

```sh
/usr/sbin/reverseproxy -f /etc/reverseproxy/reverseproxy.conf -k restart
```

`/etc/init.d/reverse_proxy.sh restart` 在某些 QTS 版本上会让端口不再监听，此时用上面的
命令直接拉起。

## 验证

```sh
# 需要认证，期望 207
curl -s -o /dev/null -w '%{http_code}\n' -X PROPFIND -H 'Depth: 0' \
  -u 'USER:PASS' https://zmhome.mycloudnas.com:2087/ws35579/

# 8 MiB 上传，期望 201，用于确认 nginx/Apache 没有请求体上限
head -c 8388608 /dev/urandom > /tmp/big.bin
curl -s -o /dev/null -w '%{http_code}\n' -u 'USER:PASS' \
  -T /tmp/big.bin https://zmhome.mycloudnas.com:2087/ws35579/_probe.bin
```

注意：本机若开着 HTTP 代理，`curl` 会走代理出口，可能复现同样的地区风控错误。
排查时加 `--noproxy '*'`。

## 已知限制

- 家宽入站端口依赖运营商策略，部分地区的 80/443 被封，2087 通常可用
- 跨境链路由 GitHub 出口到你家的上行决定，6–8 MB 的 DMG 在十几秒内完成
- QTS 升级可能重置 `/etc/init.d/reverse_proxy.sh`，升级后重新应用补丁
