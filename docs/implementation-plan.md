# 首版详细实施计划

## 1. 目标解释

本项目不是把 `go-ios` 逐文件翻译成 Rust，而是在保持用户可见能力和命令语义的前提下，
重组为类似 ADB 的 host 架构：

1. `ios` 默认作为短生命周期 client。
2. client 连接失败时，启动同一个二进制的 `ios server nodaemon` 隐藏模式。
3. server 是本机设备状态、tunnel、forward、日志流、抓包、屏幕流、WebInspector/CDP 等
   长期资源的唯一所有者。
4. 普通一次性命令也通过 server 执行，使设备选择、配对、tunnel 复用和并发策略保持一致。
5. 只允许数据面在必要时建立额外 socket；不再为每个 tunnel/forward 启动一个 CLI 进程。

“首版可用”要求 Windows 上 server/client 与真实 usbmuxd 数据路径可运行；无设备时全部核心
生命周期测试可重复。真机协议功能只有在真机验证后才标记为 `verified`，不能把“能编译”写成
“已验证”。

## 当前进度（2026-07-18）

| 阶段 | 状态 | 当前证据/剩余工作 |
|---|---|---|
| P0 参考审计 | complete | `5804fa9` Usage、iOS 26 DDI 修正与命令族已进入矩阵 |
| P1 workspace/IPC | complete | protocol 1.20、codec/auth/request ID、typed OsTrace options、交互 task input、client-owned lease、WDA event、XCTest unit/filter/shared RSD、Windows kernel tunnel 与 dproxy capability 测试 |
| P2 server 生命周期 | complete on Windows | detached spawn、lock、强杀恢复进程测试 |
| P3 backend/usbmuxd | in progress | mock/replay/recording 完成；Windows list/info 只读通过 |
| P4 task manager | complete for current task kinds | list/show/stop/restart/output、原子 state、退避/恢复；dproxy active connection 取消与超时 supervisor abort 已测 |
| P5 forward | implemented | echo、冲突、并发、强杀恢复；iOS 26 Lockdown 真机转发通过 |
| P6 tunnel | partial | iOS 26 USB userspace lifecycle/RSD/restart 通过；RemotePairing TCP 离线闭环完成但真机 mDNS 受 host interface 阻塞；Windows Wintun lifecycle/packet pump 离线通过，提权真机数据面待测 |
| P7 命令迁移 | in progress | 基础/AFC/诊断/DVT/SpringBoard 已有一批；完整矩阵仍有 planned |
| P8 验证/文档 | in progress | 219 workspace tests、shared XCTest 专项 process smoke、多套隔离 process/compat smoke、Windows/iOS 26 多轮真机记录；ASC 真实账号、RemotePairing/macOS/Linux 矩阵待做 |

`complete` 只描述该阶段列出的当前架构交付，不表示 go-ios 全功能目标完成。整体 goal 在功能矩阵
仍有 planned 项时保持进行中。

## 2. 阶段与验收

### P0：参考实现审计与范围冻结

- 从 `C:\Users/<user>\GolandProjects\go-ios` 提取完整 Usage、顶层命令、复合命令和协议包。
- 记录原 tunnel agent HTTP API、forward 阻塞模型和所有额外长期服务。
- 建立功能迁移矩阵，状态只能是：`implemented`、`offline-tested`、`device-test-needed`、
  `planned`、`intentionally-changed`。

### P1：workspace 与 IPC

- 四 crate：`rust-ios-protocol`、`rust-ios-core`、`rust-ios-server`、`rust-ios-cli`。
- IPC 使用 loopback TCP，默认 `127.0.0.1:60106`，可由 `RUST_IOS_SERVER_ADDR` 覆盖。
- 每帧为 4 字节大端长度 + JSON，最大 16 MiB。
- 首帧必须是带 token 和协议版本的 `hello`；拒绝跨 major 版本连接。
- 每个请求有 UUID；响应包含稳定错误码、可重试标记和结构化 details。

验收：帧往返、超限、截断、无效 token、版本不兼容和并发请求测试全部通过。

### P2：server 生命周期

- 数据目录默认 `%LOCALAPPDATA%\rust-ios`，测试可用 `RUST_IOS_HOME` 隔离。
- `server.lock` 使用独占文件锁，保证每个 home 只有一个 server。
- `server.json` 记录 pid、实例 UUID、版本、地址、启动时间和协议版本。
- `auth.token` 为随机 256-bit token；client 和 server 只通过受控文件共享。
- Windows 后台启动使用 `CREATE_NO_WINDOW | DETACHED_PROCESS`，stdout/stderr 写入日志。
- `start-server`、`server status`、`kill-server`、自动启动和崩溃后重启可测试。

验收：真实子进程启动、第二实例拒绝、优雅关闭、陈旧 metadata 自愈。

### P3：设备抽象与 usbmuxd

- `DeviceBackend` 隔离真实 `idevice` 实现与 mock。
- server 维护设备快照，USB 优先，UDID 显式选择优先。
- Windows 默认连接 Apple Mobile Device Process 的 `127.0.0.1:27015`；尊重
  `USBMUXD_SOCKET_ADDRESS`。
- `ios list [--details]` 输出兼容 `go-ios` 的 JSON 形状。
- 配对记录、BUID、provider、Lockdown/RSD 连接都复用固定版本的纯 Rust `idevice`。

验收：mock 空设备返回 `{"deviceList":[]}`；真实 Windows usbmuxd 的只读 list 成功；attach/detach
和设备选择测试通过。当前主机已出现一台设备，因此“本机真实空列表”需要换无设备环境复测。

### P4：持久任务管理器

- 任务统一包含 `id/kind/desired_state/runtime_state/spec/restart_policy/attempt/message/timestamps`。
- desired state 持久化到 `state-v1.json`，写入采用临时文件 + 原子替换。
- 任务 supervisor 使用 cancellation token；崩溃后指数退避，server 重启后恢复。
- `task list/show/stop/restart/output` 是通用控制面；output 支持 offset/limit/raw。

验收：模拟崩溃、取消、并发 start/stop、state 损坏和 server 重启恢复测试。

### P5：server 托管 forward

- 兼容 `ios forward <hostPort> <targetPort>` 和重复 `--port=host:target`。
- 扩展 `ios forward list|stop|clear`，命令立即返回任务信息。
- listener 由 server 绑定；每个入站连接实时选择目标设备并通过 usbmuxd 连接。
- 支持 bind host、USB/network 选择和可选 UDID；默认仅绑定 loopback，避免原实现
  `0.0.0.0` 的意外暴露。
- server 重启后重建 listener；端口冲突进入 `failed/backoff` 并保留诊断信息。

验收：mock echo 设备的双向字节、半关闭、大 payload、并发连接、停止释放端口、重启恢复。

### P6：server 托管 tunnel

- `ios tunnel start|ls|stop|refresh` 只操作 server 任务，不启动独立 agent。
- 首版优先 userspace tunnel：`CoreDeviceProxy` + `CDTunnel` + `jktcp`；server 保持 adapter。
- 为 go-ios 兼容保留本地 userspace endpoint：连接方先写 16-byte IPv6 + 4-byte
  little-endian port，然后进行透明代理。
- Windows kernel TUN 作为独立 backend；权限检查和 Wintun/tun-rs 生命周期不污染协议层。
- 每设备 tunnel 可独立重试，device detach 后停止，attach 后按 desired state 恢复。
- `--pair-record-path` 已进入持久 spec；默认路径由 server home 所有。`--remote-pairing` 已接入
  `_remoted._tcp`、external RSD、RemoteXPC、pair verify/manual consent、TLS-PSK/CDTunnel 和 jktcp；
  manual pairing 为一次性请求，PIN 不持久化。默认 USB 路径仍使用 `CoreDeviceProxy`。

验收：mock tunnel 状态机、两类 transport、userspace endpoint 双向代理、refresh 原子替换和恢复
测试；真机 CoreDeviceProxy、RemotePairing 和 Wintun 进入单独清单。RemotePairing 当前环境已确认
identity 兼容加载但在 mDNS discovery 阶段受 Windows 缺少 iPhone IPv6/link-local interface 阻塞。

### P7：一次性命令迁移

按依赖层次迁移，而不是按 CLI 文件顺序：

1. usbmuxd/Lockdown：`list`、`listen`、`pair`、`readpair`、`info`、基础设置。
2. 安装与文件：`apps/install/uninstall/file/fsync/crash/profile`。
3. 诊断：`battery*`、`diagnostics`、`mobilegestalt`、`screenshot`、`syslog/ostrace/pcap`。
4. 开发者服务：`image`、`ps/launch/kill`、`instruments/sysmontap`、`runtest/runwda/runxctest`。
5. iOS 17+：RSD、CoreDevice、UI driver、WebInspector/CDP。
6. 主机侧：签名、App Store Connect、HTTP proxy、UI HTTP/RPC 客户端。

所有命令入口先进入 registry；未迁移命令返回稳定 `not_implemented`，并在矩阵中明确，避免
静默成功或输出空 JSON。

当前新增完成：`diskspace/devmode/image list|mount|unmount/ps/launch/kill/memlimitoff`、单张
截图、SpringBoard wallpaper/icon layout、supervised set-wallpaper、profile/crash/fsync/ip、language/locale、基础
Accessibility 开关和 time format；新增持续任务
`syslog/ostrace/pcap/devicestate enable/accessibility_inspector`。

ostrace 细粒度 filters 已补齐：protocol 1.14 保存 typed level/subsystem/message/follow options，CLI 按
go-ios level 名称解析并去重，core 在解析后的 entry 上执行组合过滤。双 process smoke 覆盖 state/restart，
iOS 26 真机组合过滤 108 条全部满足条件；上游固定全量 trace 的设备负载边界已记录。

MCInstall wifi/proxy/prepare、supervised profile escalation 和 host supervision identity 已完成 offline
实现，等待专用设备验证。iOS 17+ DVT 命令/stream 已统一复用 server tunnel，并通过 iOS 26 真机；
下一批优先级：

1. 受控 `idevice 0.1.65` fork 已修正 unit XCTest launch/test filter archive，并完成 shared RSD
   connection injection。unit/filter 真机验证仍是下一批核心工作；Windows kernel tunnel 已完成实现，
   待管理员真机闭环。
2. WebInspector 一次性 `list/launch/eval`、server-owned CDP、交互 `js-shell` 与 UI runner 已完成。iOS 26 已到达
   `com.apple.webinspector.shim.remote`，设备设置关闭时结构化提示；开启后补 page/eval/CDP 真机闭环。
3. `ax audit` Accessibility DTX 已完成 iOS 26 真机闭环，`resetax` 完成 offline 实现；`ax` 持续
   inspector 已归 server task，完成 NDJSON、互斥、restart/restore/stop 和 iOS 26 真机闭环。
   UI artifact 下载、`.app/.ipa` 纯 Rust 重签名、`ui install` installation_proxy 编排和 server-owned
   `ui run` 已完成 offline/真机 WDA 闭环；RemotePairing 已完成离线闭环并等待可用 network interface
   的真机数据面验证。App Store Connect host service 已完成 certificate/P12/profile/reuse 离线闭环，
   下一步需要专用 Apple 账号验证，并在可用 network interface 上完成 RemotePairing 数据面。

iOS 17+ personalized image mount 与本地 `image auto --basedir` 已补齐 offline 路径：复用设备已有
manifest，缺失时请求 Apple TSS；本地候选按 BoardId/ChipID 和 Apple build 选择，并同步 go-ios
`5804fa9` 的 iOS 26 修正。iOS 17 以下 `image auto` 也已迁移：按 go-ios floor version table 从 raw
GitHub 下载 `.dmg/.signature`，兼容两种 cache 目录，并增加 bounded HTTP、UDIF trailer、regular-file、
symlink 与临时文件校验。剩余 image 工作只保留旧系统分版本真机 mount/unmount 验证。

screenshot MJPEG、notifications、sysmontap 已由 server task manager 持有并通过 stop/restart/process
smoke；userspace tunnel 注册共享 RSD service map，`rsd ls`、DVT stream、device condition 和 process
control 均复用该 registry。iOS 26 的 `ps/devicestate list/notifications/sysmontap/screenshot` 已真机
闭环，sysmontap 也验证了非空 sample。AppService/OpenStdio 已用于 server-owned `launch --wait`，
真机验证了 PID、raw output、EOF completion 与显式 restart。raw `debug proxy` 同样复用 registry；
自动 LLDB 已完成 typed app/installation_proxy preflight、前台 source 执行和 client-owned IPC lease，
两套 process smoke 与 Windows `remote-ios`/真实 arm64 Mach-O target 已通过；完整真机 launch 仍待设备
重新连接。usbmux dproxy 已实现 Windows 显式 endpoint、双向 raw、初始 plist/redaction 和完整 task
lifecycle；系统级 socket 替换、utun 与 DTX decode 保留为后续能力。

`setlocationgpx` 已迁为 `restartPolicy=never` 的 server-owned finite task。client 对 GPX XML、文件大小、
点数、坐标和 RFC3339 time 做前置校验，wire 只传标准化 point/delay；task manager 提供可取消调度、
NDJSON progress、失败/取消 clear、完成后保留最终点、显式 restart 和 server interruption at-most-once
语义。mock/session/process smoke 已覆盖；iOS 17+ shared-RSD DVT 与旧系统 classic service 尚需分版本
真机验证。

`activate` 已接入完整 one-shot mobileactivation 流程：非 `Unactivated` 状态只读返回；未激活设备通过
三条独立 `mobileactivationd` connection，完成 Apple DRM handshake、activation-info form request 和
activation response 回写。HTTP 使用固定 Apple endpoint、5/30 秒 connect/总时限、8 MiB body 与
64 KiB header 上限，并在回写前解析 plist。mock backend、本地 HTTP server、非 2xx/无效 plist 和
请求字段测试已覆盖；当前已激活 iOS 26 只验证幂等 fast path，完整激活仍需专用未激活设备。

`erase` 已接入 MCInstall one-shot 路径，但安全语义有意严于 go-ios：不在 server 读取 stdin，必须显式
`--udid` 加单个 `--force`。执行顺序为 Flush -> GetCloudConfiguration preflight -> EraseDevice，固定
`PreserveDataPlan=1`；只有 Acknowledged 或设备明确断开才返回 accepted，30 秒超时明确标记设备状态
未知。参数 gate、request plist 和 mock process 路径已覆盖；不可逆真机操作不在普通验证中执行。

`set-wallpaper` 已对齐 go-ios `5804fa9` 的 supervised MCInstall 路径：唯一 JPEG/PNG、唯一
`--p12file`、`--screen=lock|home|both`（Where 1/2/3）和可选 password 先由共享 parser 校验。real
backend 在设备选择前完成 client-relative 文件解析、10 MiB image/4 MiB identity 上限、图片 header/
dimension 与 PKCS#12 本地验签；随后复用 supervision escalation，发送 Settings/Wallpaper，并校验外层
和 item Acknowledged。30 秒超时不伪造结果；mock、request/response/parser/image 与 compat 路径已覆盖。
该命令会修改用户设置，普通真机 smoke 禁止执行。

`pair --p12file` 已接入 supervised Lockdown pairing：共享 parser 将标准 `--name` pairing 与 P12 模式
严格分离，P12/client-relative path/4 MiB 上限和 identity 在设备连接前验证。实现使用纯 Rust 生成独立
root/host key 与 root/host/device certificate，首个 Pair 只发送 public cert/ID 和
SupervisorCertificate，第二个 Pair 发送 CMS ChallengeResponse；private key 只进入设备成功返回
EscrowBag 后保存到 usbmuxd 的 host PairRecord。duplex 设备模拟验证了完整四阶段流、payload key 隔离和
PairingFile round-trip；真实 supervised pairing 会替换 record，普通真机 smoke 禁止执行。

iOS 17+ FileService 已接入同一 active tunnel：`--app/--app-group/--crash/--temp` 通过 shared RSD
control/data service 执行 ls/pull/push；没有 tunnel 时仅 `--app` 保留 HouseArrest compatibility，其他
domain 明确拒绝。实现包含 client-relative local path、原子 pull、500-byte inline 与 streaming push、
1 GiB 上限、控制面超时及 RSD 有界重试。mock/replay/task-manager/process smoke 覆盖四个 domain 和
shared tunnel 复用；iOS 26 `--crash` 列表与 15,990-byte pull 已和 go-ios `5804fa9` 同文件同哈希验证。

WebInspector 使用 `ios-core 0.1.7` 已验证的 plist framing、page state、Inspector target bridge 和
Remote Automation session；iOS 17+ 命令由 task manager 注入 active shared RSD tunnel，旧系统保留
Lockdown service。mock/replay/recording、两套 Windows process smoke 和参数/输出契约测试已覆盖。
server-owned CDP task 在同一设备 connection 上提供 Chrome `/json` discovery 和 page WebSocket，
由 task manager 管理 stop、端口释放和 server restart；单活动 WebSocket、mock command/response 与
两套隔离 process smoke 已覆盖。`js-shell` 以 `restartPolicy=never` task 持有 Inspector session，IPC
逐条传入 JavaScript，result/error 追加 NDJSON；stop close、异常后继续和 server interruption 不重放
均已通过 mock/socket/process 测试。真机 iOS 26 的 RSD registry 确认 service 存在，但设备未开启 Web
Inspector，因此 page list/eval/CDP 仍需在开启设置后补真机闭环。

Accessibility 使用独立 Lockdown AX DTX service，不复用 Instruments service name。该 service 的 TLS
只完成握手，随后 DTX 回到明文；保持 TLS 或直接打开未 check-in 的 RSD shim 都会被设备关闭。
`ax audit` 根据 `deviceApiVersion`
选择 API v21+ 的 `deviceAllSupportedAuditTypes/deviceBeginAuditTypes:` 或旧版 case ID selector，解析
`AXAuditIssue_v1`、classification、element rect、PlatformElement 并尽力查询 label。`resetax` 已接入但
会重置用户 Accessibility 设置，普通真机验证不得执行。持续 `ax` inspector 已使用独立
`AccessibilityInspectorSpec` 进入 task manager：启动时执行 SwitchToDevice/selection mode，输出 AX event
NDJSON，stop 时关闭 monitored event、visuals 和 app monitoring；`--font` 写入 `DYNAMIC_TYPE`，只做
offline 验证。
iOS 26.5.2 真机返回 AX API v26、7 类 audit type，并连续两次完成 audit；验证中发现并修正了该 service
特有的 TLS handshake-only 以及设备 `hostApiVersion` incoming callback ACK。inspector 两代 session 各
读取 3 个真实 SpringBoard focus element，stop 后 audit 可立即重连。

`runwda` 已迁移为 server-owned WDA task：使用 `idevice` XCTest orchestration 启动 runner，等待
WDA `/status`，建立动态 localhost HTTP/MJPEG bridge，并由 health loop、stop 和 server restore 管理；
iOS 26 custom runner 的 ready、MJPEG、server restore 和 cleanup 真机闭环已通过。WDA listener 现复用
XCTest event mapper，server 将 NDJSON 写入默认或显式 `--log-output`，通过 `task output` 分页读取；
mock 单元/进程路径已覆盖首次截断和 restore append；2026-07-18 真机复测 generation 1/2 均为
`shared_rsd`，第二代累计 126,231 bytes XCTest NDJSON。
受控 `idevice 0.1.65` fork 已增加 `XCTestRsdProvider` connection injection；当同设备 active tunnel
存在时，WDA、UI runner 与 XCTest 的 DVT/testmanagerd connections 全部复用该 route，runtime 为
`shared_rsd`。没有 tunnel 时保留 `dedicated_core_device` fallback。protocol 1.20 process smoke 与
iOS 26 真机 transport log 均证明 shared 分支命中且没有创建第二套 CoreDeviceProxy。
一次性 `ui` client 已迁移到 core 的 host-only 路径，在选择真实设备前执行：支持 WDA HTTP 与
DeviceKit JSON-RPC 2.0、auto probe、WDA lazy session、action API、client-relative body/params/output
文件和结构化 HTTP/参数错误。`ui screenshot` 的 binary stdout 尚未进入 IPC，因此必须指定文件；
持续 `ui stream` 已作为 server-owned HTTP capture task 实现：支持 MJPEG/H264、WDA/DeviceKit/auto、
query 参数、默认/显式 output、分页读取、stop 和 server restart 追加恢复，不从普通命令生成长期子进程。
真机确认 WDA API port 的 `/mjpeg` 可能返回 404；关联 `runwda` task 时必须跟随其独立 `mjpegUrl`
根路径。iOS 26 验证已覆盖真实 JPEG、task output 和 WDA/UI 两个 task generation 2 动态端口恢复。
`ui download` 现以 host command 在设备选择前 streaming 获取默认 WDA/DeviceKit artifact，WDA ZIP
安全解压；真实默认源已验证 1,161,764-byte WDA ZIP 和 8,920,427-byte DeviceKit IPA。`sign app` 与
`ui install` 结构化解析 CMS mobileprovision，校验 P12/profile/device/bundle，使用 `apple-codesign`
完成 `.app/.ipa` 重签名；generated Mach-O/profile/P12、IPA executable mode、mock process smoke 与
release build 已闭环。因未提供真实开发签名资产，installation_proxy 最终写入仍为
device-test-needed。
`ui run wda|devicekit` 现创建 `ui_runner` 常驻 task：core 持有 XCUITest runner，server 持有 host
listener、每连接 device port bridge、target-specific health loop 与 NDJSON XCTest event output。WDA 固定
8100/`/status`，DeviceKit 固定 12004/`/health`；协议中的 transport 字段由 server 再校验，不能被 client
改写。stop/restart/server restore、端口释放、双向 echo 与 capability gate 已通过隔离进程 smoke。
iOS 26 真机进一步验证 WDA `/status`、一次性 UI client、真实 XCTest event output、generation 2 同端口
恢复与完整 cleanup；设备未安装 DeviceKit runner，因此 DeviceKit 真机启动仍保留为后续验证项。
`runtest/runxctest` 已迁移为 `restartPolicy=never` 的 server-owned finite task：支持 XCUITest
suite/case/failure/activity/log NDJSON、completed/failed 汇总、显式 restart、server interruption
at-most-once 恢复，以及 binary/XML `.xctestrun` v1/v2 多 target parser。iOS 26 已验证 WDA runner 的
真实 event capture 与 stop 回收，但设备没有会自然结束的普通 XCUITest/unit bundle，完整 completion
仍是 device-test-needed。受控 `idevice 0.1.65` fork 已为 unit host 注入
`libXCTestBundleInject.dylib`，关闭 UI activate/initialize，并将 filters 编码为 legacy `NSSet` 与
modern `XCTTestIdentifierSet`；定向 archive、launch、mock task 测试已通过，真机结果尚未填写。

### P8：验证与文档

顺序执行：

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo build --workspace --release
```

然后运行 `smoke-tasks.mjs`、`smoke-tasks.ps1`、`smoke-resilience.mjs`、`compat-cli.mjs`。真机
验证文档必须记录设备/iOS/连接方式/预期/结果；destructive 命令不得进入默认 gate。
