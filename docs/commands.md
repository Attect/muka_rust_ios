# 命令手册

## 全局选项

全局选项可放在命令前后；CLI 会在解析 external go-ios 命令前提升它们。

| 选项 | 作用 |
|---|---|
| `--udid <UDID>` | 选择设备；也读取 `GO_IOS_UDID` |
| `--nojson` | 尽量输出人类可读文本 |
| `--pretty` | 缩进 JSON |
| `-v, --verbose` | client/server debug 日志 |
| `-t, --trace` | trace 日志 |
| `--version`, `version` | 输出版本 |
| `--server-home <PATH>` | 隔离实例数据目录，必须是绝对路径；显式值不会先做默认目录发现 |
| `--server-addr <ADDR>` | 覆盖 IPC 地址，主要用于测试 |

`ios <legacy-command> --help` 和 `ios help <legacy-command>` 在本地处理，不会连接设备。未迁移的
命令仍可被 parser 接受，但 server 返回 `not_implemented`，exit code 为 2。

## Server

```powershell
ios start-server
ios server status --pretty
ios kill-server
ios server nodaemon              # 前台调试
```

普通设备命令自动调用 `ensure_server`。`server status` 返回 PID、instance ID、运行时间、home、
设备数量和每类 task 数量。

## Task 控制

```powershell
ios task list [--kind forward|tunnel|syslog|ostrace|pcap|device_state]
ios task show <TASK_ID>
ios task restart <TASK_ID>
ios task stop <TASK_ID>
ios task stop all
ios task stop kind:syslog
ios task stop port:8100
ios task stop tunnel:<UDID>

# capture 增量读取
ios task output <TASK_ID> --offset 0 --limit 65536
ios task output <TASK_ID> --raw > chunk.bin
```

`task output` 的 JSON 包含 `offset`、`next_offset`、`total_bytes`、`eof`、`content_type` 和
`bytes_base64`。单次 `limit` 为 1 到 1,048,576。停止任务会删除 desired state，但不会删除文件。

## 设备发现

```powershell
ios list
ios list --details --pretty
ios listen                       # 初始快照 + attach/detach 快照，Ctrl+C 退出
ios buid
ios readpair --udid <UDID>
```

默认 `list` 兼容 `{"deviceList":["UDID"]}`。`readpair` 可能输出私钥，不应写入公开日志；
`RUST_IOS_RECORD_FILE` 只记录脱敏成功标记。

## 设备信息

```powershell
ios info --udid <UDID>
ios info lockdown --udid <UDID>
ios info display --udid <UDID>
```

`info` 与 `info lockdown` 只读取 Lockdown 全量值。`info display` 复用同设备已运行的 server-owned
userspace tunnel，连接 `com.apple.coredevice.deviceinfo`；没有 tunnel 或 DDI 未发布该 service 时返回
可重试的 `device_unavailable`。connect、XPC handshake 和 feature call 分别有 10 秒门限，不会另建
CoreDevice tunnel。

## 设备激活

```powershell
ios activate --udid <UDID>
```

`activate` 先读取 `ActivationState`；状态不是 `Unactivated` 时只返回 `already_activated`，不会连接
Apple 服务或改写设备。未激活设备依次执行 `CreateTunnel1SessionInfoRequest`、Apple
`drmHandshake`、`CreateActivationInfoRequest`、Apple `deviceActivation` 和
`HandleActivationInfoWithSessionRequest`，每个 device request 使用独立 `mobileactivationd` connection。
状态读取/每次 device exchange 时限为 5/15 秒，HTTP connect/总时限为 5/30 秒，单个请求/响应最多
8 MiB，激活 plist 和 response headers 通过严格解析后才回写设备。该路径需要访问
`albert.apple.com`；最终 `ActivationState` 会短轮询验证，响应已接受但
状态尚未更新时返回 `activation_response_accepted` 和 warning，不伪造已验证状态。

## 设备擦除（高风险）

```powershell
ios erase --force --udid <UDID>
```

`erase` 会不可逆地删除目标设备上的所有 app、设置和用户数据。client/server 模式不读取 stdin；为避免
默认选中错误设备，命令必须同时提供显式 `--udid` 和且仅一个 `--force`，任何缺失、重复或额外参数都在
连接 MCInstall 前返回 `invalid_request`。server 先执行 `Flush` 和 `GetCloudConfiguration` preflight，
再发送 `EraseDevice` + `PreserveDataPlan=1`；收到 `Acknowledged` 或设备因重启明确断开后返回
`erase_requested`。30 秒超时会返回“设备状态未知”，不会伪造成功。该命令只能在有备份的专用设备上
人工执行，普通 smoke 禁止调用真实 backend。

## Forward

```powershell
ios forward 8100 8100 --udid <UDID>
ios forward --port 8100:8100 --port 8200:8200
ios forward 0 8100                    # host port 0 自动分配
ios forward list
ios forward stop <TASK_ID|HOST_PORT>
ios forward clear
```

默认 bind `127.0.0.1`。可用 `--bind`、`--usb`、`--network` 调整地址和设备连接偏好。命令在
listener 建立后立即返回，实际连接由 server 接受。

## Tunnel

```powershell
ios tunnel start --userspace --udid <UDID>
ios tunnel start --wintun-path C:\Windows\System32\wintun.dll --interface-name rust-ios-phone --udid <UDID>
ios tunnel pair --manual --udid <UDID>
ios tunnel start --userspace --remote-pairing --udid <UDID>
ios tunnel ls
ios tunnel refresh --udid <UDID>
ios tunnel stop --udid <UDID>
ios tunnel stopagent
```

`--userspace` runtime 返回 `userspaceEndpoint`、tunnel address、RSD port 和 MTU。本地 endpoint
的调用方先写 16-byte IPv6 与 4-byte little-endian port。

Windows 默认的 kernel 模式需要管理员权限和 Wintun。`--wintun-path` 会在 client 侧转为绝对路径；
未指定时使用 `C:\Windows\System32\wintun.dll`。`--interface-name` 可固定 adapter 名称。server 完成
USB `CoreDeviceProxy`/CDTunnel handshake 后分配 client IPv6 `/64` 和 negotiated MTU，并在 Wintun 与
设备连接间双向转发 raw IPv6 packet。runtime 含 `kernel=true/interfaceName`，且不创建
`userspaceEndpoint`。kernel route 仍注册到 server tunnel registry，RSD/DVT 消费者可复用它。
`RemotePairingTcp` 当前只支持 userspace mode。

默认 `tunnel start --userspace` 继续通过 USB `CoreDeviceProxy` 建链。`--remote-pairing` 改为从
`_remoted._tcp.local.` 发现与 UDID 匹配的 external RSD，经
`com.apple.internal.dt.coredevice.untrusted.tunnelservice` 建立 RemoteXPC、pair verify、TLS-PSK、
CDTunnel 和 jktcp 数据面。首次使用前必须单独执行 `tunnel pair --manual` 并在设备端确认；这是
一次性 server request，不创建 task，PIN 不进入 IPC/state。

未指定 `--pair-record-path` 时使用 `<server-home>/remote-pairing/selfIdentity.plist`。显式路径表示
目录，CLI 会把相对路径按当前目录转为绝对路径。已有 go-ios camelCase `selfIdentity.plist` 可只读
复用；若设备拒绝该 identity，rust-ios 不会覆盖原文件，manual pairing 必须改用另一个可写目录。
control 操作上限为 10 秒，设备人工确认上限为 120 秒，超时返回结构化 `timeout`。

`stopagent` 为兼容名称，当前等价于停止 tunnel task；没有单独 agent 进程。

```powershell
ios rsd ls --udid <UDID>
```

`rsd ls` 复用 server 当前持有的 userspace 或 kernel route，不会建立第二条 CoreDevice tunnel。输出保持
go-ios 的 `{serviceName: {Port}}` 形状。没有 running tunnel 时返回结构化
`device_unavailable`；同时有多台设备的 tunnel 时必须用 `--udid` 选择。

iOS 17+ 的 `ps/launch/kill/memlimitoff/devicestate` 与 notifications/sysmontap/screenshot DVT
stream 也复用该 tunnel；没有 running tunnel 时保持 `device_unavailable/waiting`，不会静默创建
第二条 CoreDevice tunnel。持续任务 runtime 的 `serviceTransport` 应为 `shared_rsd`。iOS 16 及
以下没有 active tunnel 时仍使用 `lockdown`。

```powershell
ios debug proxy --port 0 --udid <UDID>
ios debug [--stop-at-entry] <APP_PATH> --udid <UDID>
```

`debug proxy` 在 loopback 建立长期 GDB remote TCP listener，每个 client 连接通过同一条
server-owned tunnel 打开 `com.apple.internal.dt.remote.debugproxy`。返回 task 的 runtime 含
`endpoint/host/port/protocol`；server 重启后 listener 和 tunnel 会按 desired state 恢复。

`debug <app_path>` 读取本地 `.app/Info.plist` 的 `CFBundleIdentifier/CFBundleExecutable`，要求本地
executable 存在，再通过 installation_proxy 查找设备 app 的 `Path/CFBundleExecutable`。preflight
通过后创建 `client_owned` debug proxy，生成临时 LLDB source，以前台、继承 stdio 的方式运行 LLDB：
选择 `remote-ios`、加载本地 Mach-O 符号、设置设备 executable、连接 GDB remote 并 `process launch`。
`--stop-at-entry` 会传给 LLDB；可用 `RUST_IOS_LLDB` 指定 LLDB executable。

app debug proxy 不进入 task state；LLDB 正常退出、失败、Ctrl+C 都显式 stop，client 被强制结束时
server 也会在 owner IPC 断开后回收 listener/session。iOS 17+ 当前要求先挂载 DDI 并启动同一设备的
userspace tunnel。

### usbmuxd Capture Proxy

```powershell
ios dproxy --mode=usbmuxd --bind 127.0.0.1 --port 0 `
  --upstream 127.0.0.1:27015 --output C:\captures\dproxy-events.ndjson
ios task output <DPROXY_TASK_ID> --raw
ios task restart <DPROXY_TASK_ID>
ios task stop kind:dproxy
```

Windows dproxy 不移动、不停止、也不占用 Apple 的 `127.0.0.1:27015`。命令创建一个 server-owned
显式 proxy endpoint，runtime 的 `endpoint/host/port/upstream` 给出连接参数；只有明确连接该 endpoint
的 client 流量会被捕获。server 强制 endpoint 必须是 loopback，拒绝 `0.0.0.0`/外部地址，避免把
usbmuxd 暴露给网络。动态 port 会写回持久 spec，server restore 和显式 restart 使用同一 port。

`--mode=all` 与 `--mode=usbmuxd` 当前都捕获该 endpoint 上的全部 usbmuxd 字节；`all` 不表示已包含
utun。`--mode=utun` 返回结构化 `not_implemented`，其他 mode 使用 `--iface` 返回 `invalid_argument`。
`--upstream` 必须是数值 IP socket address；listener 与 upstream 地址/通配地址重叠时会拒绝，避免
proxy 连接回自身。`--binary` 关闭每方向的 plist NDJSON，但不会关闭 raw dump 或 master event 文件。

`--output` 是 master NDJSON，记录 connection open/closed/failed 与 byte counters；`task output` 读取
该文件。每条连接另写入 `<output>.d/generation-N/connection-N/`：

- `host-to-usbmuxd.bin`、`usbmuxd-to-host.bin`：无损 raw bytes。
- `host-to-usbmuxd.ndjson`、`usbmuxd-to-host.ndjson`：初始 usbmuxd frame/plist；成功 `Connect` 后停止
  协议解析，但 raw dump 继续。

解析后的 plist 会脱敏 PairRecord/private key/password/escrow 等字段；raw dump 无法脱敏，可能包含
完整 PairRecord、TLS material 和 app/service payload。不得把 `<output>.d` 提交 Git、上传 CI artifact
或发送给不受信任的第三方。

### WebDriverAgent

```powershell
ios runwda --bundleid <RUNNER_BUNDLE_ID> --log-output C:\captures\wda.jsonl --udid <UDID>
ios task output <WDA_TASK_ID> --raw
ios task stop kind:wda
```

`runwda` 创建 server-owned WDA task。仅指定 `--bundleid` 时将其同时作为 target 与 runner bundle；
也兼容 `--testrunnerbundleid`、`--xctestconfig`、重复 `--arg` 和 `--env KEY=VALUE`。未指定 bundle
时使用 go-ios 默认的 `com.facebook.WebDriverAgentRunner.xctrunner`。runner ready 后 runtime 返回
`wdaUrl`、`mjpegUrl`、device ports 与初始 `/status`；task 每秒健康检查，stop 时终止 xctrunner 并
释放 localhost bridge。XCTest event 以 NDJSON 写入 `--log-output`；相对路径由 client 转成绝对路径，
省略该 flag 或使用 `--log-output -` 时由 server 在 captures 目录分配文件。runtime 额外返回
`outputPath`、`contentType=application/x-ndjson`、`eventsWritten` 和 `bytesWritten`；首次启动截断文件，
同一 task 的内部重试、显式 restart 和 server restore 继续追加。
同设备已有 running userspace/kernel tunnel 时，server 把该 route 注入 XCTest：DVT 和两条
testmanagerd connection 都按 shared RSD registry 中的服务端口连接，runtime 的
`serviceTransport=shared_rsd`。没有 active tunnel 时才创建 task-owned CoreDevice/RSD fallback，并标为
`dedicated_core_device`。transport 选择会写入 server info log。

### XCTest

```powershell
ios runtest `
  --bundle-id <APP_BUNDLE_ID> `
  --test-runner-bundle-id <RUNNER_BUNDLE_ID> `
  --xctest-config <BUNDLE_NAME.xctest> `
  --log-output C:\captures\xctest.jsonl `
  --udid <UDID>

ios runxctest --xctestrun-file-path C:\build\Demo.xctestrun --udid <UDID>
ios runtest --test-runner-bundle-id com.example.Demo `
  --xctest-config DemoUnitTests.xctest --xctest `
  --test-to-run DemoUnitTests.DemoCase/testExample --udid <UDID>
ios task show <TASK_ID>
ios task output <TASK_ID> --raw
```

两个命令创建 `restartPolicy=never` 的 server-owned 有限任务并立即返回 task。runner 完成协议后状态为
`completed`，runtime 带 `outcome` 和 suite/case 汇总；连接、bootstrap、timeout 或 runner 错误为
`failed`。断言失败是 `completed + outcome=failed`。事件文件使用 NDJSON，包含 target、test plan、
suite、case、failure、stall、activity、metric 和 runner log。`task restart` 才会显式重跑；server 重启
不会自动重复已完成/失败的测试。

`runxctest` 在 client 侧用 plist parser 支持 binary/XML `.xctestrun` format v1/v2，并把多个 target
按文件顺序交给同一 task 串行执行。相对 file/output path 在 client 侧转成绝对路径；
`--log-output -` 因 client 不持续附着，等价于使用 server 默认 capture path，再通过 `task output`
读取。

workspace 通过受控 `idevice 0.1.65` fork 支持 unit XCTest host：`--xctest` 必须同时提供以
`.xctest` 结尾的 `--xctest-config`，runner bundle 本身作为 test host，不启动 UI target app。
`testsToRun/testsToSkip` 同时编码 legacy `NSSet` 与 modern `XCTTestIdentifierSet`，避免 iOS 26 忽略
旧版 `NSArray<String>` archive。`--test-to-run/--test-to-skip` 可重复传入；真实 unit/filter bundle
尚待真机验证。与 WDA 相同，active tunnel 存在时 runtime 为 `shared_rsd`，否则使用
`dedicated_core_device` fallback。

### UI automation client

```powershell
$wda = 'http://127.0.0.1:<runtime-port>'
ios ui status --driver=wda --wda-url $wda
ios ui raw --driver=wda --method GET --http-path /status --wda-url $wda
ios ui tap --driver=wda --x 120 --y 300 --wda-url $wda
ios ui screenshot --driver=wda --output capture.png --wda-url $wda
ios ui source --driver=wda --output hierarchy.json --wda-url $wda
ios ui stream mjpeg --driver=wda --wda-url $wda --fps 30 --quality 80 --output capture.mjpeg
ios task output <UI_STREAM_TASK_ID> --raw
ios task stop <UI_STREAM_TASK_ID>

ios ui status --driver=devicekit --devicekit-url http://127.0.0.1:12004
ios ui api --driver=devicekit --rpc-method device.info --params '{}' --devicekit-url http://127.0.0.1:12004
```

默认 driver 是 DeviceKit；`--driver=auto` 先探测 DeviceKit `/health`，再探测 WDA `/status`。
DeviceKit 请求使用 JSON-RPC 2.0 `/rpc`，WDA action 在需要时先 `POST /session` 并复用返回的
`sessionId`；也可通过 `--session-id` 直接复用已有 session。`--body-file/--params-file` 和
`--output` 的相对路径均按 client cwd 解析，不会落到 server cwd。

当前 IPC 只传结构化 JSON，`ui screenshot` 必须指定普通文件路径；省略 `--output` 或使用
`--output=-` 会明确返回 `not_implemented`，不会把 binary PNG 混入 JSON stdout。`ui source
--output=-` 等价于直接返回结构化响应。HTTP 非 2xx、无效 JSON/base64、参数冲突和不可达 backend
均返回结构化错误。

`ui stream mjpeg|h264` 立即返回 server-owned `ui_stream` task。server 直接持有 backend HTTP response，
将 bytes 追加到指定文件或 `<server-home>/captures/ui-stream-<id>.<format>`；runtime 提供
`requestedDriver/resolvedDriver/wdaTaskId/streamUrl/contentType/bytesWritten/chunksWritten`。当 URL 对应
本 server 的 WDA task 时，spec 记录其 task ID，重启后从最新 runtime 重新解析动态 URL。`--driver=auto` 先探测
DeviceKit，再在 MJPEG 模式回退 WDA；关联 task 使用独立 `mjpegUrl` 根路径，外部 WDA URL 使用
`/mjpeg`。WDA 不支持 H264。`--output=-` 因 client 不持续附着，等价于
使用 server 默认 capture，再通过 `task output --raw` 读取。stop 取消 HTTP response，server restart
按同一 task ID、增加后的 generation 重新连接并继续追加。

`ui run wda|devicekit` 立即返回 server-owned `ui_runner` task。默认 WDA bundle 为
`com.deviceboxhq.goios.WebDriverAgentRunner.xctrunner`、host/device port 8100、health `/status`；默认
DeviceKit bundle 为 `com.deviceboxhq.goios.devicekit.runner`、port 12004、health `/health`。可用
`--bundleid/--test-runner-bundleid/--xctest-config/--host-port/--log-output` 覆盖安装产物和 host 端口；
`--host-port=0` 选择动态端口。实际 URL、设备、transport、health、日志字节数写入 task runtime。
`--log-output=-` 或省略时使用 server capture，`task output <id> --raw` 读取 NDJSON XCTest event。
runner、forward、health loop 和连接全部由 task manager 回收，server restart 以同一 task ID/host port 恢复。
WDA target 的 DVT/testmanagerd transport 与 `runwda` 相同：优先复用同设备 active tunnel，并在 runtime
报告 `shared_rsd`；DeviceKit runner 也通过同一 XCTest connection injection 路径启动。

`ui download [wda|devicekit|all] --output <dir>` 是 host-only 命令，不选择设备。默认源与 go-ios
`5804fa9` 一致；测试或镜像环境可用 `RUST_IOS_WDA_ARTIFACT_URL`、
`RUST_IOS_DEVICEKIT_ARTIFACT_URL` 覆盖。下载采用临时 `.part` 文件和同目录 rename；WDA ZIP 会安全
解压并返回其中 `.app` 路径，拒绝 zip slip。

`ui install (wda|devicekit) --p12file <p12> --profile <mobileprovision>` 支持 `--path` 指定本地
`.app/.ipa/.zip`，否则下载默认 artifact。server 结构化解析 CMS profile，校验到期时间、P12 证书、
目标 UDID 和 `application-identifier`，使用纯 Rust `apple-codesign` 重签名后再交给 installation_proxy。
`--output/--bundleid/--p12password` 与 go-ios 对齐；密码也可由 `P12_PASSWORD` 提供。独立
`sign app` 使用同一流程，可选 `--install`。

### App Store Connect 签名资产

```powershell
ios sign certificate appstoreconnect `
  --asc-key-id <KEY_ID> --asc-issuer-id <ISSUER_ID> --asc-private-key .\AuthKey.p8 `
  --p12-output .\identity.p12 --p12password <PASSWORD>

ios sign provision appstoreconnect --bundleid com.example.Runner `
  --asc-key-id <KEY_ID> --asc-issuer-id <ISSUER_ID> --asc-private-key .\AuthKey.p8 `
  --certificate-id <EXISTING_CERTIFICATE_RESOURCE_ID> `
  --profile-output .\Runner.mobileprovision --udid <UDID>
```

两条命令都经 host command router 执行，不选择 `DeviceBackend`、不创建 task。credentials 也可由
`GO_IOS_ASC_KEY_ID/GO_IOS_ASC_ISSUER_ID/GO_IOS_ASC_PRIVATE_KEY` 提供；P12 password 可由
`P12_PASSWORD` 提供。相对 key/output path 按 client cwd 解析，P12/profile 使用同目录临时文件原子
替换；private key、JWT 和 password 不进入 command result、desired state 或 recording。

`certificate` 生成 RSA key/CSR，创建 `IOS_DEVELOPMENT` certificate 并写 P12。Apple 账号通常只允许
一个当前 development certificate；只有显式 `--revoke-existing` 才会先列出并删除账号下全部该类型
certificate，此操作只适用于专用 CI 账号。

`provision` 使用显式 `--udid` ensure device、Bundle ID 和 `IOS_APP_DEVELOPMENT` profile。推荐通过
`--certificate-id` 复用已有 identity：此模式不会创建/revoke certificate，也不会写 P12；为避免误导，
它拒绝 `--revoke-existing` 和 `--p12-output`。省略 `--certificate-id` 时会同时创建 certificate/P12，
输出名默认是 `<bundleid>.p12` 与 `<bundleid>.mobileprovision`。当前仅完成本地 JSON:API 离线验证，
真实 Apple 账号调用仍需专用凭据测试。

## 日志与抓包

```powershell
ios syslog --udid <UDID>
ios syslog --parse --output C:\captures\phone.ndjson

ios ostrace --pid 123 --udid <UDID>
ios ostrace --process MyApp --follow --nojson
ios ostrace --level error,fault --subsystem com.example --match timeout --exclude ignored

ios pcap --udid <UDID>
ios pcap --process MyApp --output C:\captures\app.pcap
```

三个命令都立即返回 task。未指定 `--output` 时写入 `<server-home>/captures`。`syslog --parse`
写 JSONL；ostrace 默认 JSONL，配合 `--nojson` 写 plain text；pcap 写 tcpdump PCAP。

`sysmontap` 先通过 DVT DeviceInfo 读取 process/system attributes，再按 go-ios 配置接收嵌套 sample
batch；task 进入 running 后仍应通过 `task output` 检查 `total_bytes`，不能只检查连接状态。

ostrace 的 `--level` 接受大小写不敏感的逗号列表：`default,info,debug,error,fault`，会去重并在非法值时
于 client 连接 server 前报错。`--subsystem` 对 label subsystem 做包含匹配，`--match/--exclude` 对 message
做大小写敏感的包含/排除；所有条件按 AND 组合，options 会随 task state 和 server restart 保留。
当前 `idevice 0.1.65` 的 `start_trace` 固定请求全量 trace，因此这些过滤只减少 capture 输出，不减少设备
采集或 USB 流量；os_trace 对设备 CPU 有明显负载，应缩短采集时间。server 对暂时不存在的设备/进程进入
`waiting` 并重试。

## Device Condition

```powershell
ios devicestate list --udid <UDID>
ios devicestate enable <CONDITION_ID> <PROFILE_ID> --udid <UDID>
ios task list --kind device_state
ios task stop <TASK_ID>
```

`list` 是一次性 DVT 查询；`enable` 立即返回 task，DVT session 由 server 持有。停止 task 或优雅
关闭 server 时会调用 `disable_condition`；desired state 保留时，新 server 会重新启用同一 profile。
同一设备选择器只允许一个 active `device_state` task。

## Accessibility

```powershell
ios ax [--font 17] --udid <UDID>
ios ax audit [--timeout 60] --udid <UDID>
ios resetax [--timeout 60] --udid <UDID>
ios task output <AX_TASK_ID> --raw
ios task restart <AX_TASK_ID>
ios task stop <AX_TASK_ID>
```

裸 `ax` 启动 server-owned Accessibility inspector。task 先执行与 go-ios 相同的
`SwitchToDevice`/selection mode 握手，再写入一条 `inspector_started` 和三条初始 focus event；后续
`hostInspectorCurrentElementChanged:`、App state 与 inspector notification 继续追加为 NDJSON。同一
设备只允许一个 inspector，server restart 会按 desired state 重建 session。stop 会关闭 monitored
event、selection visuals 和 app monitoring；输出通过 `task output --raw` 读取。

`--font` 对齐 go-ios 的 `DYNAMIC_TYPE` 设置，会改变设备 Accessibility 设置；普通只读 smoke 不传。

`ax audit` 对当前前台 App 执行设备支持的全部 Accessibility audit，返回 `apiVersion/auditTypes/issues`
以及每个 issue 可用的 `issueType/label/elementRect/platformElementValue`。AX daemon 在新旧系统上均从
Lockdown/usbmuxd 打开，并执行 TLS handshake-only 后切回明文 DTX。`--timeout` 允许 1 到 600 秒。

`resetax` 会调用 `deviceResetToDefaultAccessibilitySettings`，直接重置设备的 Accessibility 用户设置；
它不是只读命令，不应放进普通 smoke。

## 已接入设备命令

下列命令均通过 server 和 `DeviceBackend::execute`，真实协议状态见功能矩阵：

```text
pair [--name <host>]
pair --p12file <organization.p12> [--password value]
info
lockdown get [key] [--domain domain]
devicename
date
ip [--timeout seconds]
lang [--setlocale locale] [--setlang language]
assistivetouch enable|disable|toggle|get [--force]
voiceover enable|disable|toggle|get [--force]
zoom enable|disable|toggle|get [--force]
timeformat 24h|12h|toggle|get [--force]
ax audit [--timeout seconds]
ax [--font size]
resetax [--timeout seconds]

apps [--system|--all] [--list] [--filesharing]
install --path <ipa-or-app>
uninstall <bundle-id>
file ls [--app bundle|--app-group group|--crash|--temp] [--path remote]
file pull [--app bundle|--app-group group|--crash|--temp] --remote path --local path
file push [--app bundle|--app-group group|--crash|--temp] --local path --remote path
fsync tree --path remote [--app bundle]
fsync mkdir --path remote [--app bundle]
fsync rm [--r] --path remote [--app bundle]
fsync pull --srcPath remote --dstPath local-path [--app bundle]
fsync push --srcPath local-file --dstPath remote [--app bundle]
diskspace
crash ls [pattern]
crash cp <pattern> <local-directory>
crash rm <remote-directory> <pattern>
profile list
profile add <mobileconfig>
profile remove <identifier>

diagnostics list
batterycheck
batteryregistry
mobilegestalt <key>... [--plist]
devmode get|reveal|enable [--enable-post-restart]
reboot
shutdown

setlocation --lat value --lon value
setlocationgpx --gpxfilepath route.gpx
resetlocation
screenshot [--output file]

image list
image mount --path DeveloperDiskImage.dmg                  # iOS < 17
image mount --path devimages/ddi-23F84/Restore             # iOS 17+
image auto --basedir devimages                             # iOS <17 下载/cache；iOS 17+ 本地选择
image unmount
ps [--apps]
launch <bundle-id> [--arg value]... [--env KEY=VALUE]... [--kill-existing] [--wait]
kill <bundle-id>|--pid value|--process name
memlimitoff --process name

get-wallpaper [--output wallpaper.png]
set-wallpaper <wallpaper.jpg|png> --p12file <identity.p12> [--password value] [--screen lock|home|both]
get-app-icon <bundleID> [--output icon.png]
get-icon-layout [--output layout.json]
set-icon-layout <layout.json>
```

`mobilegestalt` 默认保留 go-ios 的外层 `Status` 与 `Diagnostics` envelope 并输出 JSON；`--plist`
将同一响应直接写成 XML plist，不受 `--pretty` 或 `--nojson` 影响。iOS 26 会在请求成功的 envelope
内返回 `Diagnostics.MobileGestalt.Status=MobileGestaltDeprecated`，这是设备协议结果，不是 CLI 失败。

`setlocationgpx` 在 client 侧解析 GPX `trk/trkseg/trkpt`，文件上限 16 MiB、最多 10,000 点；坐标必须
在有效范围，`time` 存在时必须为 RFC3339。命令创建 server-owned finite task，按相邻时间戳间隔
调度并把 `simulation_started/location_set/simulation_completed` 写入 NDJSON。iOS 17+ 要求同设备已有
userspace tunnel 并复用 DVT LocationSimulation，旧系统使用 classic service。正常完成后保留最后一个
模拟点，使用 `resetlocation` 清除；`task stop` 或执行失败会主动 clear 半程位置。server 重启不会自动
重放，只有 `task restart <id>` 会从第一个点开始新一代。

普通 `launch` 是一次性 DVT 调用。`launch --wait` 要求同设备已有 server-owned userspace tunnel，
通过 `com.apple.coredevice.openstdiosocket` 取得 UUID，再由 AppService 启动 app；返回的
`app_process` task 捕获合并 stdout/stderr。EOF 后 task 进入 `completed`，`task stop` 或 server
shutdown 只终止该 task 本次启动且仍存在的 PID。输出用 `task output <id> --raw` 读取；该有限任务
不会在 server 重启后自动重跑，只有显式 `task restart <id>` 才会再次启动 app。

相对文件路径基于发起 client 的 current working directory，不基于 server cwd。
`pair --p12file` 对齐 go-ios 的 supervised pairing：P12 最大 4 MiB，密码可由 `--password` 或 server
启动时继承的 `P12_PASSWORD` 提供。server 在连接设备前解析并验证 identity，然后经 raw Lockdown
依次读取 `DevicePublicKey`/`WiFiAddress`，发送 SupervisorCertificate Pair request，签名设备返回的
`MCChallengeRequired.PairingChallenge`，再发送 ChallengeResponse。只有设备返回 EscrowBag 后才把
包含 host/root private key 的完整 PairRecord 保存到 usbmuxd；发往设备的 PairRecord 不含 private key。
`--name` 仅用于标准 trust-dialog pairing，不能与 `--p12file` 组合。45 秒超时会明确说明设备 pairing
状态未知且 host record 未保存。真实 supervised pair 会替换 pairing record，只能在有 identity 备份和
恢复方案的专用 supervised 设备执行。

`set-wallpaper` 只接受单个不超过 10 MiB 的 JPEG/PNG，默认 `--screen=home`；`lock/home/both`
分别映射 MDM Wallpaper `Where=1/2/3`。命令要求匹配设备 supervision identity 的 PKCS#12，密码可由
`--password` 或 server 启动时继承的 `P12_PASSWORD` 提供。server 在连接设备前校验路径、格式、尺寸、
identity 和 4 MiB identity 上限，再执行 MCInstall `Escalate -> EscalateResponse ->
ProceedWithKeybagMigration -> Settings`，且同时要求外层和 `Settings[0]` 返回 `Acknowledged`。30 秒
超时会明确报告壁纸状态未知。iOS 16+ 可能忽略 `Where` 并把图片应用到 lock/home 两个 screen；该命令
属于高风险写操作，日常真机 smoke 禁止执行。

`get-app-icon` 是明确的 Rust 扩展命令；当前 go-ios CLI 没有对应入口。它通过 classic
`com.apple.springboardservices` 的 `getIconPNGData` 读取一次性 PNG，默认输出
`<bundleID>.png`。响应不是 PNG、bundle ID/参数无效或 `--output=-` 时返回结构化错误；文件使用
同目录临时文件原子替换，不会在失败时留下 partial output。iOS 26 的 SpringBoard 对语法有效但未安装的
bundle ID 可能返回 placeholder PNG；命令保留设备响应，调用者若要区分 App 是否安装，应先查询
`apps --all`。

`file` 不带 domain selector 时保持普通 AFC；`--app` 在没有 shared tunnel 时保持 HouseArrest 兼容路径。
存在同设备 server-owned userspace tunnel 时，`--app` 与 `--app-group/--crash/--temp` 使用 iOS 17+
CoreDevice FileService，并复用 `com.apple.coredevice.fileservice.control/data`。后三种 selector 不会回退到
AFC，缺 tunnel 时返回可操作的 `device_unavailable`。同一命令只允许一个 selector；app/app group
identifier 不能为空。

FileService remote path 相对所选 domain 根目录。`pull` 先写同目录临时文件并原子替换目标，失败时删除
partial file；`push` 对不超过 500 bytes 的文件使用 inline XPC，较大文件走 data service，最大 1 GiB。
control service connect、XPC handshake、session/list 与 data connect 均有 10 秒超时；RSD 首次读取最多
重试 3 次，避免设备大 service map 分片异常导致 client 永久等待。不存在的 app/group container 返回
不可重试的 `invalid_request`，不会伪装为内部错误。

`image auto` 在 iOS 17+ 扫描 `--basedir`（默认 `./devimages`）的直接子目录
`ddi-*/Restore/BuildManifest.plist`，使用设备 `BoardId/ChipID` 过滤 identity，并按 Apple build
版本选择最新的完整 DDI。该选择规则同步 go-ios `5804fa9`，因此不会再把固定的
`ddi-17E5179g` 用于 iOS 26/A19 等不兼容设备。若没有匹配项会返回结构化错误；iOS 17+ 不会联网
下载或回退到固定 personalized DDI。`image mount` 在 iOS 17+ 也会先校验显式 Restore 目录是否包含当前设备 identity，
再优先复用设备已有 personalization manifest，必要时请求 Apple TSS。

iOS 17 以下的 `image auto` 按 go-ios 版本表选择不高于设备 ProductVersion 的最新 DDI，从
`mspvirajpatel/Xcode_Developer_Disk_Images` 的 raw GitHub 路径下载 image/signature，并缓存到
`<basedir>/<image-version>/`。兼容读取 go-ios 的短版本目录和带 build label 目录。HTTP connect/总时限
为 15/120 秒，image/signature 上限分别为 2 GiB/64 KiB；只有 regular file、UDIF `koly` trailer 和
非空 signature 校验通过后才从临时文件提升到 cache，再调用 MobileImageMounter。HTTP 非 2xx、空体、
超限、symlink cache 或不完整文件都返回结构化错误，不会当作已下载。
其中 iOS 15.8/15.8.x 使用已实机验证的 Xcode 15.5 legacy DDI 兼容覆盖，不再选择 15.7；其他
版本继续使用不高于设备版本的最高 floor DDI。

`lang` 不带设置 flag 时返回当前 language/locale 和支持列表。改变 language 后 SpringBoard 会重启，
server 最多轮询 5 分钟，只有读回请求值才返回成功。三个 Accessibility 开关和 `timeformat` 默认
拒绝 iOS 11 以下设备；`--force` 只绕过版本 gate，不能绕过非法 action，写后仍执行读回校验。

`ip` 从 Lockdown 读取静态 `WiFiAddress`，再从 pcapd 流量中寻找源 MAC 匹配的 IPv4 和 IPv6。
设备必须产生网络流量；启用 Private Wi-Fi Address 时静态 MAC 可能无法匹配。默认等待 60 秒，
`--timeout` 可设为 1 到 3600 秒。

## WebInspector

```powershell
ios tunnel start --userspace --udid <UDID>                 # iOS 17+
ios webinspector list [--timeout 5] --udid <UDID>
ios webinspector eval <PAGE_KEY> 'document.title' [--console-enable] --udid <UDID>
ios webinspector launch https://example.com/ [--bundle-id com.apple.mobilesafari] --udid <UDID>
ios webinspector cdp [--host 127.0.0.1] [--port 9222] [--timeout 15] --udid <UDID>
ios webinspector js-shell [https://example.com/] [--bundle-id com.apple.mobilesafari] `
  [--open-safari] [--timeout 5] --udid <UDID>
```

`list` 输出 go-ios 兼容的 `application/page` camelCase 字段，`eval` 使用 listing 中的 `page.key`
选择 `WIRTypeWeb/WIRTypeWebPage/WIRTypeJavaScript` 页面。`launch` 通过 Remote Automation 创建 browsing
context，导航后返回 `bundleID/url/title`。iOS 17+ 必须先启动同设备 server-owned userspace tunnel；
旧系统使用 Lockdown `com.apple.webinspector`。设备还必须开启“设置 > Safari > 高级 > Web 检查器”，
`launch` 另需开启“远程自动化”。连接被设备关闭时返回 `device_unavailable` 和上述操作提示。

`webinspector cdp` 创建 server-owned task，提供 `/json`、`/json/list`、`/json/version` 与
`/devtools/page/<PAGE_KEY>`。HTTP discovery 会刷新或读取缓存 page list；一个 task 同时只允许一个
活动 page WebSocket，第二个连接返回 HTTP 409。设备 WebInspector connection 在 task 内复用，
`task stop <ID>` 会关闭 session 并释放端口，server restart 会按原 task ID 恢复。`--port 0` 可请求
动态端口，实际 `jsonUrl` 和 endpoint 位于 task runtime。

`webinspector js-shell` 的设备 connection 与 InspectorSession 由 server-owned finite task 持有，CLI
只从 stdin 逐行发送 expression；空行忽略，`.exit`、`exit`、`quit`、EOF 或 Ctrl+C 会 stop task。`--open-safari`
先请求启动 Safari，URL 会在 attach 后通过 `window.location` 导航；`--bundle-id` 用于筛选 page。
每条 result 输出到 CLI，并与 JavaScript error 一起追加到 task 的 NDJSON capture。单条 error 不关闭
session；server interruption 将 task 标为 failed 且不重放 expression。`--timeout` 同时限制启动和单条
evaluation，范围 1 到 120 秒。

`fsync pull` 递归复制目录并流式复制文件；与 go-ios 一致，`--dstPath` 本身就是文件或目录的最终
local path，不会再追加 remote basename。`fsync push` 当前与参考实现一致，只接受单个 local file；
若 remote 目标已是目录，会追加 local basename。`fsync rm --path /` 即使带 `--r` 也会被拒绝。

## Profile、Wi-Fi 与 Prepare

```powershell
# 普通 profile 可能要求在设备上确认
ios profile add profile.mobileconfig --udid <UDID>

# supervision identity silent install
ios profile add profile.mobileconfig `
  --p12file supervision-identity.p12 --password <P12_PASSWORD> --udid <UDID>

ios wifi --ssid <SSID> --password <WIFI_PASSWORD> [--enc-type WPA2] --udid <UDID>
ios wifi --ssid <SSID> --remove --udid <UDID>

ios httpproxy <HOST> <PORT> [USER] [PROXY_PASSWORD] `
  --p12file supervision-identity.p12 --password <P12_PASSWORD> --udid <UDID>
ios httpproxy remove --udid <UDID>

ios prepare printskip
ios prepare cloudconfig --udid <UDID>
ios prepare create-cert --p12password <PASSWORD>
ios prepare --skip-all [--locale en_US] [--lang en] --udid <UDID>
ios prepare --skip WiFi --skip AppleID --udid <UDID>
ios prepare --certfile supervision-cert.der --orgname <NAME> --udid <UDID>
```

`wifi` 先检查 activation state。未监督设备使用 `InstallProfile`，成功只表示 profile 已提交，CLI
会明确提示需要在设备设置中确认。`httpproxy` 与 supervised `profile add` 使用 PKCS#12 private key
对 MCInstall challenge 生成 CMS SignedData，再执行 `InstallProfileSilent`；`remove` 只删除 rust-ios/
go-ios 共用的固定 global proxy profile identifier。

`prepare create-cert` 不选择设备，在 client cwd 写入 certificate DER/PEM、private key DER/PEM、CSR
和 `supervision-identity.p12`。未提供 `--p12password` 时 P12 密码为空；private key 文件和密码不得
进入仓库、CI artifact 或 recording。`P12_PASSWORD`、`PROXY_PASSWORD` 可作为兼容 fallback，但常驻
server 不会自动获取启动之后才修改的 client 环境变量，自动化应显式传 flag/位置参数。

完整 `prepare` 会修改 cloud config、可选 supervision、language/locale、time/timezone、restriction
profile 和 AFC `SkipSetup` 文件，只能在专用设备按真机验证清单执行。`--skip` 值会严格校验；不传时
与 `--skip-all` 一样使用完整列表。

## 明确不支持的组合

- `dproxy --mode=utun`、系统级 usbmuxd socket 替换和 DTX decode 尚未实现；显式 usbmuxd endpoint
  不会伪装成这些能力。
- kernel tunnel 的缺 DLL、相对 DLL path、非管理员和不支持 transport 会返回明确的 request error，
  或创建带具体 message 的 `failed` task；不会报告 running。

## 输出与 Exit Code

成功响应通常是 compact JSON；`--pretty` 只改变 JSON 格式。命令返回纯文本时直接写 stdout，
不会额外 JSON quote；目前 `mobilegestalt --plist` 使用该路径。错误固定写 stderr：

```json
{
  "error": {
    "code": "not_implemented",
    "message": "operation is not implemented: ...",
    "retryable": false
  }
}
```

| exit code | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 本地 CLI 参数/序列化错误 |
| 2 | `not_implemented` |
| 3 | server 返回其他 protocol error |
| 4 | IPC、启动或超时错误 |
