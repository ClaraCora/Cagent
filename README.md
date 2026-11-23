# Cagent

[Cmonitor](https://github.com/ClaraCora/Cmonitor) 的 Linux agent。采集本机指标，经 WebSocket 上报 hub。

静态链接单文件，无运行时依赖，常驻内存数 MB。

## 特性

- 直接读 `/proc` 与 `statvfs`，不依赖 sysinfo
- 内存对齐 `free(1)` 的 used 列，磁盘对齐 `df(1)` 的 Used 列
- 无状态：不写文件，不保存跨重启的数据，流量累加由 hub 负责
- token 走 `Authorization` 头，不进反向代理的 access log
- 默认拒绝非回环地址的明文 `ws://`；即使显式启用 `--insecure`，远程明文连接也不能打开终端
- 每节点最多 4 个终端，断线清理交互 shell，输出拥塞不会阻塞 Agent 的通信循环
- `v1.1.3` 修复 TLS 握手漏洞 RUSTSEC-2026-0285
- Web terminal 由本机 agent 打开 PTY，终端命令以 `root` 权限执行

## 安装

在 hub 的面板添加节点，复制生成的命令在目标主机执行：

```bash
curl -fsSL https://your-hub/install.sh | sh -s -- --server https://your-hub --token <token>
```

安装脚本识别 systemd 与 OpenRC，二进制装到 `/opt/monitor/monitor-agent`，token 写入
`/opt/monitor/agent.env`（0600）——和 hub 同一个目录，那台机器上只有这一处要看。Agent
服务以 `root` 运行，因此登录 Cmonitor 后台的管理员可以通过 Web Terminal 完整管理节点。

## 运行

```bash
monitor-agent --server https://your-hub --token-file /opt/monitor/node.token
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--server` | 必填 | hub 地址，也可用 `MONITOR_SERVER` |
| `--token-file` | 可选 | 单行节点 Token 文件，属主为 root 或运行 Agent 的用户，权限设为 `0600`。常规服务安装继续从环境文件读取 `MONITOR_TOKEN`。旧参数 `--token` 仍可用，值会出现在进程命令行。 |
| `--interval` | 1 | 上报间隔（秒），1–3600 |

Web Terminal 需要 WSS 连接，本机回环的明文连接仅用于上报与已获准的探测。可在服务环境文件设置 `MONITOR_DISABLE_TERMINAL=true` 关闭终端。填写无法识别的值时也会关闭终端，监控上报继续运行。

HTTP 节点默认只上报指标。若需要延迟探测，在节点的 `/opt/monitor/agent.env` 设置 `MONITOR_HTTP_PROBE_ALLOWLIST`，重启 Agent 后生效。列表以逗号分隔，每项指定 IP 和端口，或域名、端口及允许的解析 IP。多个解析 IP 用 `|` 分隔。只有 Hub 已分配且在列表中的目标会被探测。以下为示例地址，请换成实际监测目标。使用引号可兼容 OpenRC。

```ini
MONITOR_HTTP_PROBE_ALLOWLIST="192.0.2.1:443,[2001:db8::1]:80,latency.example:443=192.0.2.2|2001:db8::2"
```

域名解析到其他地址时，Agent 会跳过该地址。名单格式无效时会停止 HTTP 延迟探测，修正并重启后恢复。

## 上报字段

`src/collect.rs` 中的 `Facts` 与 `Metrics` 两个 struct 直接序列化为线上 JSON，是字段的权威定义。

- **`Facts`** 连接时上报一次：主机名、系统、内核、架构、虚拟化类型、CPU 型号与核数、内存与磁盘总量、本机 IPv4 / IPv6
- **`Metrics`** 每 `--interval` 秒上报：CPU、负载、内存、swap、磁盘、网卡收发速率与内核累计计数器、TCP / UDP 连接数、进程数、运行时间

`net_rx_total` / `net_tx_total` 为内核 lifetime 计数器，原样上报；`boot_id` 取自
`/proc/sys/kernel/random/boot_id`，是 hub 判定主机重启的唯一依据，**不要删**。

协议说明见 [hub 仓库](https://github.com/ClaraCora/Cmonitor)。

## 构建

所有构建、测试与发布使用 GitHub Actions 的 Linux runner，需要 Rust stable。以下命令在 runner 执行：

```bash
cargo build --release
cargo test
cargo clippy --all-targets
```

发布产物为 musl 静态链接二进制，推送 `v*` tag 由 `.github/workflows/release.yml` 构建。

## 许可

MIT
