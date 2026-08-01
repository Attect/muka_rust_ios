# 从 go-ios 迁移

## 核心变化

go-ios 中的 `forward`、tunnel agent、syslog、pcap 等命令通常由调用方保留一个长期进程。
rust-ios 把这些资源移动到一个本机 server：

| go-ios 模式 | rust-ios 模式 |
|---|---|
| 每个 forward 一个阻塞 CLI | server 中一个 `ForwardTask` |
| tunnel agent 进程 + HTTP info API | server 中 `TunnelTask` + task IPC |
| syslog/pcap 直接写当前 stdout/file | server `StreamTask` 写 capture 文件 |
| 调用方自行记录 PID | 使用稳定 task UUID |
| 进程退出即丢失资源 | desired state 在 server 重启后恢复 |

因此自动化脚本应从“保持 child 存活”改为“保存 task ID，并在 finally 中 stop”。

## Forward 示例

go-ios：

```powershell
$process = Start-Process ios -ArgumentList 'forward 8100 8100' -PassThru
# ...
Stop-Process $process
```

rust-ios：

```powershell
$response = ios forward 8100 8100 --udid $udid | ConvertFrom-Json
$taskId = $response.value.tasks[0].id
try {
    # 使用 127.0.0.1:8100
}
finally {
    ios task stop $taskId | Out-Null
}
```

client 退出不会停止 listener。端口冲突在创建阶段返回 `conflict`，且不会留下 desired task。

## Tunnel 示例

Windows 首版：

```powershell
$task = (ios tunnel start --userspace --udid $udid | ConvertFrom-Json).value.task
$endpoint = $task.runtime.userspaceEndpoint
ios tunnel ls --pretty
ios tunnel refresh --udid $udid
ios tunnel stop --udid $udid
```

不再需要 `GO_IOS_AGENT_HOST`、tunnel info HTTP port 或 `stopagent` 关闭另一个进程。
`stopagent` 仅保留为兼容别名。需要 go-ios userspace endpoint 的程序仍可使用相同 20-byte 握手。

iOS 17+ 也可显式选择 RemotePairing TCP transport：

```powershell
ios tunnel pair --manual --pair-record-path C:\stable\rust-ios-rp --udid $udid
ios tunnel start --userspace --remote-pairing --pair-record-path C:\stable\rust-ios-rp --udid $udid
```

配对是一次性 server request，不再由 tunnel agent 隐式执行；设备端确认最多等待 120 秒。默认
identity 位于 server home，已有 go-ios camelCase identity 可只读复用，但验证失败时必须换独立路径，
不会原地覆盖 go-ios 文件。当前 Windows 真机缺少 iPhone IPv6/link-local interface，故该 transport
只标记为 device-test-needed。

## 日志与 PCAP

go-ios 的命令默认一直占用 terminal；rust-ios 默认创建任务并立即返回：

```powershell
$task = (ios syslog --parse | ConvertFrom-Json).value.task
$path = $task.spec.configuration.output_path

# 分块读取；next_offset 用作下次 offset
$chunk = (ios task output $task.id --offset 0 | ConvertFrom-Json).value
[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($chunk.bytes_base64))
```

显式 `--output` 使用 client cwd 解析；未指定时在 server home 下生成 UUID 文件名。PCAP restart
继续追加同一文件，不重复 global header。

## Server 地址与 Home

生产通常使用默认值：

```text
RUST_IOS_HOME=%LOCALAPPDATA%\rust-ios
RUST_IOS_SERVER_ADDR=127.0.0.1:60106
```

测试必须隔离二者。不要让 CI、多个 worktree 或开发版二进制共享 home，因为 auth token、lock、
metadata 和 desired tasks 都属于该 home。

## 输出兼容

- `list` 默认保留 `{"deviceList":[...]}`。
- `--nojson`、`--pretty`、`--udid` 可放在 legacy 命令后。
- `mobilegestalt` 保留 go-ios 的 `Status`/`Diagnostics` envelope；`--plist` 原样输出 XML。
- `info display` 不查询 go-ios agent；它复用 rust-ios server 已持有的同设备 userspace tunnel。
- 错误统一为 `{"error": ProtocolError}`，比 go-ios 的日志 + `os.Exit` 更稳定。
- task/server 响应使用 tagged `kind/value`，这是 rust-ios 扩展，不与 go-ios agent HTTP JSON 等同。
- `help` 在 client 本地执行；不会因请求帮助而连接设备。

## Backend 与录制

测试脚本可替换真实设备：

```powershell
$env:RUST_IOS_BACKEND = 'mock'
$env:RUST_IOS_MOCK_DEVICES = 'phone-a,phone-b'

$env:RUST_IOS_BACKEND = 'replay'
$env:RUST_IOS_REPLAY_FILE = 'tests\fixtures\replay-basic.json'

$env:RUST_IOS_RECORD_FILE = "$env:TEMP\ios-recording.jsonl"
```

recording 包装任意 backend。数据面 payload 不录制；PairRecord 内容永不录制；password 和 env value
会替换成 `<redacted>`。fixture 应只包含可公开、最小化的设备信息。

## MCInstall 与 Supervision

`wifi`、`httpproxy`、`prepare` 和 `profile add --p12file` 保留 go-ios 命令形状。差异：

- `prepare create-cert` 额外直接生成 `supervision-identity.p12`，不要求外部 OpenSSL 命令。
- P12/CMS 是纯 Rust host 实现，private key 不进入设备 backend 之外的 crate，也不进入 recording。
- 未监督 Wi-Fi/profile install 返回成功时同时输出 warning，明确区分“已提交”和“已在设备生效”。
- 完整 prepare 的 `--skip` 会拒绝未知值；用 `prepare printskip` 获取稳定列表。
- 常驻 server 只能看到启动时环境变量；自动化应显式传 `--password`，不要依赖稍后设置的
  `P12_PASSWORD/PROXY_PASSWORD`。

## App Store Connect

`sign certificate/provision appstoreconnect` 保留 go-ios 的 credential flag 与环境变量。rust-ios 在选择
设备 backend 前处理它们；`provision` 只把显式 `--udid` 作为 Apple device registration identifier，
因此可在没有 usbmuxd/真机连接的 CI host 上运行。所有相对 key/P12/profile path 按 client cwd 解析。

existing certificate reuse 是推荐的并行 CI 模式：传 `--certificate-id` 只创建 profile，不创建 P12，
也不允许同时传 `--revoke-existing` 或 `--p12-output`。创建新 certificate 时，
`--revoke-existing` 仍表示删除账号下全部 `IOS_DEVELOPMENT` certificate；不会因为 Apple 返回 409
而自动 revoke。API private key/JWT/password 不持久化到 task state 或 recording。

## 不兼容或尚未完成

- Windows kernel TUN 已使用 Wintun 实现；它需要管理员权限，默认 DLL 为
  `C:\Windows\System32\wintun.dll`，不会像 go-ios agent 那样创建独立长期子进程。
- RemotePairing TCP 已接入但尚未完成真机数据面验证；默认 USB CoreDevice transport 不变。
- `launch --wait`、screenshot MJPEG、DVT notifications/sysmontap 已迁移为 server-owned task，
  并在 iOS 17+ 复用 server tunnel。`launch --wait` 返回 finite `app_process` task，输出需通过
  `task output` 读取。
- supervised pairing 已实现并保持显式 P12 模式；unit XCTest 已通过受控 `idevice` fork 接入。WebInspector
  `js-shell` 已迁移为 `restartPolicy=never` 的 server-owned 交互 task；持续 Accessibility inspector 已迁移为
  server-owned `accessibility_inspector` task，输出用 `task output` 读取；一次性 UI HTTP/RPC client、
  server-owned UI stream、WDA 和 XCUITest/runxctest 已迁移。XCTest filters 同时编码 `NSSet` 与
  `XCTTestIdentifierSet`；unit/filter 仍需真机测试。MCInstall supervision escalation、wifi/proxy/prepare、普通 profile、crash report
  与 Lockdown Accessibility 开关已接入但待真机验证。`ax audit/resetax` 已迁移到 AX DTX；前者已
  完成 iOS 26 只读验证，后者会修改用户设置，只做 offline 验证。`ax --font` 同样会改变设置，真机
  smoke 不执行。
- `debug <app_path>` 已迁移为 installation_proxy preflight、前台 LLDB 和 client-owned debug proxy；
  owner IPC 断开会自动回收且不会写入 state。
- `dproxy` 在 Windows 改为 server-owned 显式 TCP endpoint，不移动 Apple usbmuxd socket；支持 raw dump、
  初始 usbmuxd plist/redaction、task output、stop/restart/restore。系统级拦截、utun 和 DTX decode 尚未迁移。
- `rsd ls`、process control、AppService/OpenStdio、device condition、DVT streams、WDA、UI runner 和
  XCTest 已统一复用 server tunnel registry。WDA/XCTest 在没有同设备 active tunnel 时仍保留
  `dedicated_core_device` fallback；脚本可从 task runtime 的 `serviceTransport` 判断实际路径。

迁移脚本时先查 [功能矩阵](feature-matrix.md)，再用 mock/replay 执行错误路径；依赖 planned flag 的
脚本应在调用前显式拒绝，而不是捕获空输出。
