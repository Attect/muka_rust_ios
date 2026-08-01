# Windows 运行与运维

## 前置条件

- Windows 10/11 x64。
- Apple Mobile Device Process 正常运行；默认 usbmuxd 地址 `127.0.0.1:27015`。
- MSVC Rust toolchain 与对应 Visual C++ build tools。
- userspace tunnel 不需要管理员；kernel tunnel 需要管理员和 x64 Wintun DLL。

检查 Apple listener：

```powershell
Get-NetTCPConnection -State Listen -LocalPort 27015
$env:USBMUXD_SOCKET_ADDRESS = '127.0.0.1:27015' # 默认，可显式覆盖
```

## 默认文件

`%LOCALAPPDATA%\rust-ios`：

| 文件 | 含义 |
|---|---|
| `server.lock` | 独占实例锁，不应用“文件存在”判断 server 存活 |
| `auth.token` | 256-bit 本机 IPC token |
| `server.json` | PID、instance、版本、地址、executable |
| `server.log` | server 日志，跨重启追加 |
| `state-v1.json` | running desired tasks |
| `captures\*` | 默认 syslog/ostrace/pcap 输出 |

token 和 PairRecord 不应提交 Git。state 不包含 PairRecord 私钥或 live socket。

## 日常命令

```powershell
ios start-server
ios server status --pretty
ios task list --pretty
ios list --details --pretty
ios kill-server

# 前台调试；Ctrl+C 结束
$env:RUST_IOS_LOG = 'debug'
ios server nodaemon
```

server 专用日志过滤变量为 `RUST_IOS_LOG`。环境中的通用 `RUST_LOG` 不覆盖它，避免调用方意外
关闭 server 诊断。

## 后台进程行为

自动启动使用 `CreateProcessW`：

```text
CREATE_NO_WINDOW | DETACHED_PROCESS
bInheritHandles = FALSE
```

child 不继承 PowerShell/Node/CI pipe，因此 client 输出完成后可以正常退出。不要用另一个 wrapper
再把 `ios server nodaemon` 的 stdout pipe 保持打开；前台模式应由服务管理器直接托管。

## 隔离实例

测试、多个 worktree 和不同版本必须使用不同的绝对 server root 与 port：

```powershell
$env:RUST_IOS_BACKEND = 'mock'
$env:RUST_IOS_MOCK_DEVICES = 'mock-phone'
ios list --server-home C:\rust-ios-process-tests\case-60200 --server-addr 127.0.0.1:60200
```

`--server-home` 拒绝相对路径；提供绝对路径时不会先读取默认数据目录，因此精简 CI/子进程环境也能
只访问明确的隔离 root。

不要在测试里设置 port 0 后让 detached client 猜实际端口；测试 harness 应先取得一个动态空闲
port，再把确定值传给 server/client。

## 更新二进制

Windows 无法覆盖正在运行的 executable：

```powershell
ios kill-server
cargo build --workspace --release
```

若 linker 报 `failed to remove ... ios.exe: Access denied`：

1. 读取 `%LOCALAPPDATA%\rust-ios\server.json`。
2. 确认 `executable` 指向当前 workspace 的 target 文件。
3. 使用该版本 `ios kill-server` 优雅退出。
4. 仅在握手不可用且 executable/PID 均确认后使用 `Stop-Process -Id <PID> -Force`。

不要终止未确认归属的用户实例。

## Tunnel

```powershell
ios tunnel start --userspace --udid <UDID>
ios tunnel start --wintun-path C:\Windows\System32\wintun.dll --interface-name rust-ios-phone --udid <UDID>
```

不带 `--userspace` 选择 Windows kernel mode。程序不会自行请求 UAC 提权：必须从已提升的终端启动
server，且 `--wintun-path` 必须是存在的绝对文件路径；默认值为
`C:\Windows\System32\wintun.dll`。成功后 task runtime 提供 `interfaceName/clientAddress/address/mtu/rsdPort`，
不提供 `userspaceEndpoint`。非管理员返回 `permission_denied`；server stop/restart 负责关闭和重建 adapter。

## Dproxy

```powershell
ios dproxy --mode=usbmuxd --upstream 127.0.0.1:27015 `
  --output C:\rust-ios-captures\dproxy-events.ndjson
ios task show <TASK_ID> --pretty
```

dproxy 创建动态 loopback endpoint，不移动或停止 Apple listener。把需要分析的 client 显式指向 task
runtime 的 `endpoint`；非 loopback `--bind` 会被拒绝，未改向的 Apple/ios client 流量不会经过 proxy。
`--mode=utun` 在 Windows 返回
`not_implemented`，`--iface` 不适用于 usbmuxd mode。

master event 可通过 `task output` 读取；每连接 raw dump 位于
`C:\rust-ios-captures\dproxy-events.ndjson.d\generation-*\connection-*`。parsed NDJSON 会脱敏已知字段，
raw `.bin` 仍可能包含完整 pairing/private-key/service payload，必须按凭据材料保护。

## 故障定位

按顺序收集：

```powershell
ios server status --pretty
ios task list --pretty
Get-Content "$env:LOCALAPPDATA\rust-ios\server.log" -Tail 100
Get-NetTCPConnection -LocalPort 60106 -ErrorAction SilentlyContinue
Get-NetTCPConnection -LocalPort 27015 -ErrorAction SilentlyContinue
ios list --details --pretty
```

常见情况：

- `authentication_failed`：home/token 不一致；不要自动启动第二实例。
- `conflict`：forward bind port 已占用；失败创建不会留下 task。
- `waiting`：设备、目标进程或 usbmuxd 暂不可用，server 正在重试。
- `backing_off`：service/文件/端口出现可重试错误，查看 `message` 和 `retryInMs`。
- `state-v1.corrupt-*`：state 解析失败已隔离；保留文件用于分析。
- capture 不增长：检查 task runtime `bytesWritten/chunksWritten` 和输出路径权限。

## 安全

- 不要把 forward bind 到 `0.0.0.0`，除非确实需要局域网访问。
- `readpair` 输出包含敏感配对材料；默认 terminal history/CI artifact 不应保存。
- dproxy 的每方向 raw dump 未脱敏，不得提交 Git、上传公共 artifact 或长期留在共享目录。
- destructive 命令 `reboot/shutdown/devmode enable/image mount` 必须在真机清单中显式授权。
- `prepare`、Wi-Fi、global proxy 和 supervised profile 会改变设备管理状态，只能使用专用设备。
- `prepare create-cert` 生成的 `.key/.p12` 是敏感文件；使用受控目录并在验证后转移或删除。
- 清理测试目录前验证完整路径在 `%TEMP%` 下；仓库 smoke 脚本已经执行这一约束。
