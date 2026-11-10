# 测试策略与当前证据

## 原则

- 普通测试不得要求设备、Apple 服务、管理员权限或固定端口。
- 每个进程测试必须通过 `--server-home` 指向隔离的绝对目录，并使用独立 loopback port；本轮禁止
  读取或设置任何 HOME 环境变量。
- destructive 真机命令默认不运行；真机测试按单独清单记录。
- mock 测生命周期，replay 测稳定控制面，recording 只采集脱敏协议结果。
- Cargo gate 串行运行，避免 Windows linker、exe lock 和共享 target 干扰结论。

## 测试层次

| 层次 | 覆盖 |
|---|---|
| protocol 单元 | frame 往返、0/超限/截断、stable tags、task kind |
| core 单元 | 设备选择、双向 bridge、dproxy half-close/raw/plist/redaction、mock attach/detach、tunnel、stream、XCTest/AppProcess session、UI HTTP/RPC、artifact 下载、CMS/P12 重签名、replay、录制脱敏 |
| server 单元/集成 | auth、lock、metadata、state 隔离、IPC 复用、registry wait、dproxy fake upstream/capture/restart、常驻/有限 task 生命周期 |
| CLI 单元 | global flag 提升、legacy help 安全、mapping、dproxy/stream/XCTest/AppLaunch request、`.xctestrun` v1/v2、日志文件 |
| 子进程 smoke | 自动启动、数据面、重启、并发、端口冲突、强杀恢复、redaction |
| 只读宿主机检查 | Apple Mobile Device Process `list`；其余真机能力独立审批 |

当前 workspace 有 233 项 Rust 测试：CLI 33、core 125、protocol 12、server 63。受控
`vendor/idevice-0.1.65` 另有 4 项 XCTest archive/configuration/launch 定向测试、1 项 DVT
`killPid:` wire argument 定向测试和 6 项 TSS RestoreRequestRules 规则引擎测试；受控
`vendor/ios-core-0.1.7` 另含 NSKeyedArchiver 解码深度上限测试。

## Cargo Gate

```powershell
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --release --workspace --all-features
```

若固定 `1.96.1` toolchain 的 Clippy component 损坏，先重新安装：

```powershell
rustup component remove clippy --toolchain 1.96.1-x86_64-pc-windows-msvc
rustup component add clippy --toolchain 1.96.1-x86_64-pc-windows-msvc
```

2026-07-16 本机已通过该方式恢复仓库默认 Clippy gate。不要因 component 问题跳过 lint；无法恢复时
才可在比对两个 `rustc -Vv` 完全一致后使用 stable Clippy，并在验证记录中注明。

## Task 进程 Smoke

```powershell
cargo build --workspace
node scripts/smoke-tasks.mjs target/debug/ios.exe
powershell -NoProfile -ExecutionPolicy Bypass `
  -File scripts/smoke-tasks.ps1 -Bin target/debug/ios.exe
```

脚本创建十五个可恢复/有限 server task：forward、userspace tunnel、debug proxy、WebInspector CDP、finite XCTest、
finite AppProcess、finite GPX LocationSimulation、WDA、syslog、ostrace、pcap、notifications、sysmontap、
screenshot stream、device state；另创建一次不持久化的 `debug <app_path>` client-owned session。验证：

1. client 自动启动 detached server。
2. forward 和 tunnel endpoint 双向 echo。
3. `rsd ls` 复用同一 tunnel，并保持 `{service: {Port}}` 输出。
4. `info lockdown` 保持 Lockdown shape，`info display` 复用同一 tunnel 的 deviceinfo service。
5. `debug <app_path>` 生成含 local/device executable、`remote-ios`、`--stop-at-entry` 的 LLDB source，
   fake LLDB 退出后 task 数量归零；随后 raw debug proxy 经共享 RSD tunnel 双向 echo，stop/restart
   后 listener 可恢复。
6. 五种 capture 可读取；ostrace level/subsystem/message include/exclude 组合后只有匹配 NDJSON，PCAP
   magic 为 `d4c3b2a1`，MJPEG 返回 JPEG frame。
7. notifications/sysmontap/screenshot/devicestate 首次启动和恢复后的 `serviceTransport` 都是
   `shared_rsd`，证明没有退回独立 DVT。
8. `devicestate list`、enable、`--kind device_state` filter 返回稳定形状。
9. Node 通过 format v2 `.xctestrun`、PowerShell 通过 `runtest` 创建 XCTest task；suite/case NDJSON
   可读、状态进入 completed，显式 restart 提高 generation。
10. AppProcess task 复用 shared RSD、捕获 raw stdout/stderr、保持 completed 且不自动重跑。
11. GPX task 解析两点时间序列、输出 NDJSON、显式 restart 提高 generation。
12. WDA task 返回 localhost URL，stop/restart 释放并重建 session handle。
13. `kill-server` 后十二个常驻 task 使用同一 ID 恢复，三个 finite task 保持 completed 且不重跑。
14. forward/debug proxy 继续 echo，capture 文件继续增长；ostrace options 与过滤结果跨重启不变。
15. `task stop all` 后 state task 数量为 0，并释放 condition/WDA handle。

server runtime 单测另外直接 drop owner IPC，验证 `client_owned` debug proxy 不进入 state，并在断连后
自动移除；这覆盖 client 被强制结束而无法发送 `TaskStop` 的路径。

成功标记为 `PROCESS_SMOKE_OK`。

## Resilience Smoke

```powershell
node scripts/smoke-resilience.mjs target/debug/ios.exe
```

覆盖：

- 16 个并发 client 混合执行 `list --details` 和 `server status`。
- host port 被外部 listener 占用时返回 `conflict`，state 不泄漏 task。
- 创建 forward/syslog 后用 OS 强制终止 server PID。
- client 自动启动新 server，恢复两个 task，forward 和 syslog 数据面继续工作。
- recording 中出现 `read_pair_record` + `redacted:true`，且不含 mock HostID。

成功标记为 `RESILIENCE_SMOKE_OK`。

## MCInstall 与 Host Tool Smoke

```powershell
node scripts/smoke-mcinstall.mjs target/debug/ios.exe
```

第一阶段使用 mock + recording 和独立 home/port，依次验证 `prepare printskip/cloudconfig`、Wi-Fi
add/remove、supervised profile add、global proxy add/remove 和 prepare 参数形状。四个测试密码必须
全部从 JSONL 消失，并出现 `<redacted>`。

第二阶段使用不可达的测试 usbmuxd 地址，证明 `prepare create-cert` 不选择设备，并在临时 client
cwd 生成非空 certificate DER/PEM、private key DER/PEM、CSR、PKCS#12 六个文件。单元测试还会把
生成的 P12 重新解析并对 challenge 生成 PKCS#7/CMS SignedData。成功标记：

```text
MCINSTALL_MOCK_OK
SUPERVISION_HOST_OK
MCINSTALL_SMOKE_OK
```

## Replay

fixture：`tests/fixtures/replay-basic.json`。包含设备快照、BUID、脱敏 PairRecord，以及 info、apps、
batterycheck、diskspace、`fsync tree`、ip、`devicestate list`、devmode get、ps、profile list、crash ls、language 和
四种设置 get、go-ios 形状的 `diagnostics list`，以及 `prepare cloudconfig/printskip` 响应。使用：

```powershell
$env:RUST_IOS_BACKEND = 'replay'
$env:RUST_IOS_REPLAY_FILE = (Resolve-Path tests/fixtures/replay-basic.json)
target\debug\ios.exe info --udid replay-phone `
  --server-home C:/Users/<user>\RustroverProjects\rust_ios\target\process-tests\replay-basic `
  --server-addr 127.0.0.1:60201
```

最长 argv 前缀优先匹配；fixture 不含命令时返回 `not_implemented`，不会回退 mock success。

## CLI Compatibility

```powershell
node scripts/compat-cli.mjs target/release/ios.exe C:/Users/<user>/bin/ios.exe
```

脚本从参考 `go-ios --help` 提取命令路径。当前基线得到 86 条，逐条执行 Rust 版
`<command> --help`，要求 exit 0 且临时 home 中没有 `server.json`。随后验证 version JSON、mock 设备
列表（`RUST_IOS_MOCK_DEVICES=mock-phone`，空列表形状由 mock 单元测试覆盖）、mock MobileGestalt
JSON/go-ios envelope/XML plist、mock `ax audit/resetax`、
`ui status`、幂等 `activate`、mock `erase` 双重确认 gate、mock supervised pair、mock `set-wallpaper`
screen/Where 合约，以及独立 home/port 的 `webinspector js-shell` stdin eval/退出/task cleanup；另用
`runwda --log-output=-` 启动 mock WDA，验证 `application/x-ndjson` event capture、`task output` 与 stop，
最后关闭隔离 server。go-ios v1.2.1 新增的 `pasteboard get/set` 与 `mdm` 四子命令以显式列表验证
（参考二进制可能早于 v1.2.1）：help 不启动 server、mock get/set 形状、`mdm security-info` mock 形状、
缺 `--p12file` 结构化拒绝，标记 `V121_COMMANDS=ok`。

UI focused tests 使用临时 loopback HTTP server 校验 WDA `/status`、显式 session tap、DeviceKit
JSON-RPC envelope、screenshot base64 与 client-relative 文件输出；mock backend 另行覆盖 status、
screenshot/source 文件形状。测试不启动 WDA，也不要求连接设备。

成功标记为 `CLI_COMPAT_OK`。该测试证明入口/help/错误形状存在，不证明 86 条命令都已实现；实现
状态仍以功能矩阵为准。

protocol 1.16 的 release 定向进程 smoke 使用固定 workspace-local 绝对 server root，不读取 HOME
环境变量：mock `runwda` 首次生成 144-byte/1-event NDJSON；关闭并自动重启 server 后同一 task
generation 2 追加为 288-byte/2-event，最后显式 stop 和 kill-server。成功标记为
`WDA_CAPTURE_PROCESS_SMOKE_OK`，测试目录保留用于审计。

protocol 1.17 的 release 定向进程 smoke 使用固定绝对 server root
`target/process-tests/xctest-unit-filters-protocol-117-final` 和独立 port 60217，不读取或设置任何
HOME 环境变量。mock unit XCTest 使用明确 `.xctest` bundle、一个 run filter 和一个 skip filter；
`task output` 捕获 `DemoCase/testSelected`，最终 generation 2 为 8 条 event/1344 bytes。缺少
`.xctest` suffix、空 bundle stem 和 unit `--bundle-id` 请求均被拒绝；server PID 退出且 port 可重绑。
release SHA-256 为 `C9E4AA7750ABE544F893E15B2F4AD27BC3B6735B4371A89FDFE776B8059141E3`，成功标记为
`XCTEST_UNIT_FILTER_PROCESS_SMOKE_OK`，证据目录保留。

protocol 1.18 的 Windows kernel tunnel 定向进程 smoke 使用固定绝对 server root
`target/process-tests/kernel-tunnel-protocol-118-final` 和独立 port 60231。release server 明确报告
protocol 1.18；不存在的绝对 DLL path 生成 `runtime_state=failed` 与 `invalid argument` message，真实
`C:\Windows\System32\wintun.dll` 在非管理员进程中生成 `runtime_state=failed` 与
`permission denied ... os error 5`。两项 task 均可显式 stop，kill-server 后 PID 49960 退出且 port
释放。最终复跑 PID 14308，release SHA-256 为
`5D70FAA6C086D9BDC4D7081214E597EA114B9AEF275FCBCDC3943D2F5D6DE619`。
本测试不证明 adapter 创建或真机数据面成功。

protocol 1.19 的 dproxy 单元/集成测试使用 loopback fake usbmuxd upstream，不读取设备 backend：

- protocol 验证 `dproxy_start`/`dproxy` stable tags、legacy defaults；CLI 验证所有 typed options、重复值、
  `task --kind dproxy` 与 `persistent_dproxy` capability gate。
- core 验证 fragmented usbmuxd plist、PairRecord redaction、成功 `Connect` 后 raw-mode 切换、双向 raw/NDJSON
  capture，以及 client half-close 后 upstream 延迟响应不会被截断。
- server 验证 request routing、master `task output`、连接 counters、manager restore、显式 restart、active
  connection stop、port release、utun/iface/相对 output/malformed upstream/self-proxy/non-loopback 拒绝。
- stop/shutdown supervisor 超时会 abort 并 await `JoinHandle`，不会把持续 runtime 留成 detached task。

release 双进程 smoke：

```powershell
node C:/Users/<user>\RustroverProjects\rust_ios\scripts\smoke-dproxy.mjs `
  C:/Users/<user>\RustroverProjects\rust_ios\target\release\ios.exe `
  C:/Users/<user>\RustroverProjects\rust_ios\target\process-tests\dproxy-protocol-119-final-4 `
  60247
```

脚本只把明确的 Windows system/path/temp 字段传给 child，显式 `start-server`，不依赖默认数据目录；
`--server-home` 直接从绝对路径构造生产配置，relative path 会在启动前拒绝。fake upstream 验证
generation 1/2/3、同一 dproxy port 52498、三个 parsed connection 与一个 binary-only connection；第一代
master output 为 636 bytes。active connection stop 后 dproxy port 与 IPC 60247 均可重绑，PID 38228 退出，
`state-v1.json` tasks 为空且 `server.json` 已移除。成功标记为 `DPROXY_PROCESS_SMOKE_OK`，证据目录保留。
最终 release 为 28,642,304 bytes，SHA-256
`881CD81FE88B3768FE17E726D48E0406B0B2B0BBFB9686EE528FB7AD2AD6B4A2`。

## Shared XCTest RSD 进程 Smoke

protocol 1.20 的专项脚本要求调用方传入不存在的绝对 evidence 目录，不读取或设置任何 HOME 环境变量，
也不删除成功或失败 evidence：

```powershell
node C:/Users/<user>\RustroverProjects\rust_ios\scripts\smoke-shared-xctest.mjs `
  C:/Users/<user>\RustroverProjects\rust_ios\target\debug\ios.exe `
  C:/Users/<user>\RustroverProjects\rust_ios\target\process-tests\shared-xctest-protocol-120-smoke-2
```

脚本先建立 mock userspace tunnel，再串行验证 WDA、UI runner 和 finite XCTest，避免同设备 XCTest
owner 互斥冲突。WDA/UI runner 分别在 server restart 后以 generation 2 恢复且保持
`serviceTransport=shared_rsd`；XCTest 在 server restart 后保持 completed/generation 1，不会自动重跑，
显式 `task restart` 后 generation 2 仍为 `shared_rsd`。tunnel 跨三个阶段恢复到 generation 4，
XCTest 首次 output 为 1,514 bytes。最终 task/state 为空、IPC/UI runner port 释放，成功标记为
`SHARED_XCTEST_PROCESS_SMOKE_OK`。成功 evidence 保留；首次并发启动触发预期 `conflict` 的失败 evidence
保留在相邻 `shared-xctest-protocol-120-smoke-1`，没有覆盖或删除。

一次性 screenshot 的 shared-RSD 路由另由两条定向测试覆盖：core mock 写出可解析 PNG 并标记
`reused server-owned shared RSD tunnel`；server task manager 测试证明 active tunnel 会注入普通
`screenshot` 命令，而不只注入持续 screenshot stream：

```powershell
cargo test -p rust-ios-core one_shot_screenshot
cargo test -p rust-ios-server shared_rsd_commands_and_streams_reuse_active_tunnel
```

## UI Stream 进程 Smoke

```powershell
node scripts/smoke-ui-stream.mjs target/debug/ios.exe
```

脚本启动本地持续 MJPEG HTTP 源和独立 home/动态 IPC port，通过真实 `ios.exe` 创建 `ui_stream` task，
验证 query、multipart content type、binary `task output` marker、显式 stop。中途关闭 server 后，同一
task 以 generation 2 恢复并继续追加 capture；最终 task 0、server/home/process 全部清理。成功标记为
`UI_STREAM_PROCESS_SMOKE_OK`。

server 单元测试另用 DeviceKit 风格 `/health` + `/h264` 源验证 auto driver、bitrate query、
`video/h264` content type 和 NAL start code；真机仅覆盖 WDA MJPEG，未安装 DeviceKit runner。

## UI Artifact 与签名 Smoke

```powershell
node scripts/smoke-ui-artifacts.mjs target/debug/ios.exe
```

脚本使用本地 HTTP server、隔离 home/动态 IPC port 和 mock phone，验证 host-only DeviceKit IPA
streaming download 不选择真实设备、client-relative output、随后 `ui install devicekit` 进入 mock backend
而不产生长期 task。core 单元测试另生成 RSA P12、CMS mobileprovision 和 AArch64 Mach-O app，完成
`.app` 与 `.ipa` 的 Bundle ID/profile/CodeResources/Unix executable mode 重签名闭环。成功标记为
`UI_ARTIFACT_PROCESS_SMOKE_OK`；默认外网 artifact URL 不属于普通 gate。

## UI Runner 进程 Smoke

```powershell
node scripts/smoke-ui-runner.mjs target/debug/ios.exe
```

脚本使用 mock phone、独立 home 和动态 IPC/WDA/DeviceKit port，通过真实 `ios.exe` 创建 WDA runner，
验证 `/status`、非 HTTP 双向 echo、NDJSON `task output`、stop 释放端口；`kill-server` 后同一 task 以
generation 2 和同一 host port 恢复。随后创建 DeviceKit runner，验证固定 device port 12004 与
`/health`，最终 task、server、端口和临时 home 全部清理。成功标记为
`UI_RUNNER_PROCESS_SMOKE_OK`。

## App Store Connect 进程 Smoke

```powershell
node scripts/smoke-app-store-connect.mjs target/debug/ios.exe
```

脚本生成临时 P-256 `.p8`，启动 loopback Apple JSON:API、独立 server home 和动态 IPC port。它验证
provision 缺 UDID、existing certificate 与 revoke 冲突均在发请求前失败；成功路径完成 Bundle ID/device
ensure 与 profile relationship/content 写入，输出相对 client cwd 解析，不创建 P12/task。随后 certificate
路径生成真实 RSA CSR，mock API 返回 409，CLI 保留 Apple title/detail 且不留下 P12。六个 API request
均携带三段 ES256 JWT，private key 内容不进入 state/server log；最终 server/port/home 清理。成功标记为
`APP_STORE_CONNECT_PROCESS_SMOKE_OK`。测试只使用 loopback override，不访问真实 Apple 账号。

## RemotePairing 进程 Smoke

```powershell
node scripts/smoke-remote-pairing.mjs target/debug/ios.exe
```

脚本使用 mock phone、独立 home 和动态 IPC/userspace port。它先验证缺 identity 返回明确失败、
`tunnel pair` 缺 `--manual` 被拒绝，再执行一次性 pairing，检查 identity 不进入 `state-v1.json` 且
不创建长期 task。随后以 `remote_pairing_tcp` 启动 tunnel，完成 20-byte endpoint echo；`kill-server`
后同一 task 以 generation 2 恢复并再次 echo。最后停止 task、关闭 server、确认 IPC port 可重绑并
删除临时 home。成功标记为 `REMOTE_PAIRING_PROCESS_SMOKE_OK`。

## FileService 进程 Smoke

```powershell
node scripts/smoke-fileservice.mjs target/debug/ios.exe
node scripts/smoke-fileservice.mjs target/release/ios.exe
```

脚本使用 mock phone、独立 server home、动态 IPC/userspace port 和 client cwd。它先确认
`--app-group/--crash/--temp` 缺 tunnel 时明确失败，而 legacy `--app` fallback 不变；随后启动 shared
tunnel，验证 app/app-group/crash/temp 四个 domain 的 ls，覆盖歧义 selector、small inline push、large
streaming push、原子 pull 和 partial file 清理。最后停止 tunnel、关闭 server、确认 task/port/home 无
残留。debug/release 成功标记均为 `FILESERVICE_PROCESS_SMOKE_OK`。

## Accessibility inspector 进程 Smoke

```powershell
node scripts/smoke-accessibility-inspector.mjs target/debug/ios.exe
node scripts/smoke-accessibility-inspector.mjs target/release/ios.exe
```

脚本使用 mock phone、独立 server home 和动态 IPC port。它验证裸 `ax` 创建 server-owned task、NDJSON
started/focus event、同设备第二实例 conflict、`kill-server` 后 generation 2 restore、显式 restart 到
generation 3、task kind filter 与 stop 清理；不会传 `--font`。成功标记为
`ACCESSIBILITY_INSPECTOR_SMOKE_OK`。

## 真实 Windows 只读证据

2026-07-16 当前主机的 `127.0.0.1:27015` 有 Apple Mobile Device Process listener。使用独立
临时 home/port 和真实 `IdeviceBackend` 检测到一台 `测试 iPhone / iOS 26.5.2 / 23F84` USB 设备，
`list/info/apps --all/apps --list` 均成功。从 `D:\iOSDeviceSupport\iOSDeviceSupport` 选择并挂载
`ddi-17F113` 后，userspace tunnel 与 74 个 RSD service 正常，`ps --apps` 返回 10 个 app process。

同一隔离实例还完成以下可逆真机验证：`launch --wait` 通过 AppService 启动专用 WDA runner，
OpenStdio 捕获 238 bytes；`jktcp` 的 remote FIN 经 AppService 确认 PID 已退出后进入 `completed`，
显式 restart 提高到 generation 2。WDA `/status` 为 `ready=true`；syslog 捕获超过 250 KiB；真实
forward 到 device port 8100 可访问 WDA，stop 后 host port 可立即重新 bind。最终 task 0、image list
空、隔离 home 删除、server port 和 `ios/go-ios` 进程均为 0。未执行 pair、安装、文件写入、定位、
Developer Mode、reboot 或 shutdown。

iOS 17 以下 DDI 自动下载合入后，release build 再次在同一设备执行 `image list/auto/list/unmount/list`：
仍从 `D:\iOSDeviceSupport\iOSDeviceSupport` 选择 `ddi-17F113`，挂载信息的
`ProductBuildVersion=17F113`，卸载后 image/task 均为空；独立 home、动态 IPC port 和进程全部清理。

protocol 1.14 的 ostrace filters 使用同一 iOS 26 设备完成只读真机验证。未过滤 4 秒样本为 48,667 条、
25,459,321 bytes；随后用 `debug + com.apple.locationd.Motion + Accelerometer - Skipping` 组合采集 2 秒，
108 条/40,218 bytes 全部满足 level/subsystem/include/exclude。task stop 后动态 IPC port、临时 home 和
`ios/go-ios` 进程均为 0；release binary 再以相同条件复测 1 秒，63 条/23,616 bytes 仍逐条通过。
该对比也证明 host filter 不降低设备侧全量采集负载。

新增 UI client 后再次使用隔离实例启动同一 WDA runner；`ui status` 和 `ui raw GET /status` 均返回
`ready=true` 与 iOS 26.5.2。同期复测 user app 24 项、syslog 分页输出和 `forward 0 8100`，最终同样
确认 task/image 为 0、4 个动态端口释放、无 `ios/go-ios` 进程且临时 home 已删除。未执行 tap、type、
app control、orientation set 或其他 UI 写操作。

server-owned UI stream 随后完成真机验证：关联 WDA task 后使用独立 `mjpegUrl` 根路径，capture 返回
`multipart/x-mixed-replace` 和真实 JPEG SOI。关闭 server 后 WDA/UI stream 均 generation 2，WDA
HTTP/MJPEG 动态端口发生变化，UI stream 经过短暂 waiting 自动跟随新 URL 并继续追加。最终 task/image
为 0，旧/新 WDA/MJPEG 与 IPC 共 5 个端口释放，进程/home 清理完成。

protocol 1.9 的 `ui run wda` 随后使用同一已安装 runner 完成真机验证：server-owned task 在 host
50842 转发 device 8100，`/status` 与一次性 `ui status` 均返回 ready=true/iOS 26.5.2；启动捕获
188 条、54014 bytes XCTest NDJSON。`kill-server` 后 task 以 generation 2 和同一 host port 恢复，
capture 追加到 108026 bytes。stop 后 host port 立即可重绑，task/image 为 0，IPC/WDA 端口、临时
home 和 `ios/go-ios` 进程全部清理。设备未安装 DeviceKit runner，其启动仅由 mock/process smoke 覆盖。

protocol 1.10 的 RemotePairing 阶段再次使用刚重启的同一设备回归，测试前确认 `ios/go-ios` 进程
为 0。release client/server 完成 1.10 协商；`list/info` 正常，App filter 为 24/237/261/8；本地
`ddi-*` 扫描继续选择 `ddi-17F113`。USB CoreDevice tunnel、WDA `/status` 与 `ui status`、syslog
120916 bytes/首 64 KiB 356 条 NDJSON、forward 到 WDA 8100 均通过。RemotePairing 只读加载现有
go-ios camelCase identity，SHA-256 前后不变；当前 Windows 缺少 iPhone IPv6/link-local interface，
在 5 秒 `_remoted._tcp.local.` mDNS discovery 门限处进入 waiting，未执行 manual pairing。最终
task/image 0、DDI 卸载，IPC/WDA/MJPEG/tunnel/forward ports 全部释放，临时 home 删除且
`ios/go-ios` 进程为 0。

FileService 阶段使用 go-ios `5804fa9` 与本仓库 debug build 对照同一 iOS 26 crash report。两者 pull
均为 15,990 bytes，SHA-256 均为
`8FAC24EC2BC6210C657A66B787EDAE4FD0AF2E73D3A9FC47F6840C140E51520B`；Rust 输出经临时文件原子替换，
失败探测没有留下 partial file。不存在的 app container 返回 `invalid_request`。同轮复测刚重启设备的
24 个 user app、WDA ready、13,968-byte/81-line syslog 与 `forward 0 8100`，最终 task/image/process 均为
0，动态端口可重绑，临时 home 已删除。

MobileGestalt 阶段在同一设备重启后使用安全 key `ProductVersion` 对照 go-ios `5804fa9`。Rust JSON 与
XML plist 均恢复参考实现的外层 `Status=Success` 和 `Diagnostics` envelope；iOS 26 内层返回
`MobileGestaltDeprecated`。mock XML 通过 Rust plist parser 往返，compat smoke 还验证 `--pretty`
不会把 XML 包装成 JSON string。所有命令使用隔离 home，结束后 `ios/go-ios` 进程为 0。

DeviceInfo 阶段先以 mock 验证 `info display` 缺 tunnel 的结构化失败与 shared-RSD 注入，再在 iOS 26
挂载 `ddi-17F113` 后对照 go-ios `5804fa9`。RSD 发布 74 services；两端均返回
`backlightState/current/displays/orientation`、6 个 display、`activeOn` 与相同 portrait/faceUp orientation。
结束后 tunnel、DDI、IPC/home 全部清理，`image list=[]` 且 `ios/go-ios` 进程为 0。

Activation 阶段先在独立 home/port 读取同一 iOS 26 设备的 `ActivationState=Activated`，随后 release
`activate` 仅走幂等 fast path，返回 `already_activated/verified=true`；未访问 Apple HTTP，也未发送
activation response 回写。临时 home 删除且 `ios/go-ios` 进程为 0。Unactivated 完整流程只完成本地
HTTP/device request 合约测试，仍需专用未激活设备。

protocol 1.20 只读核心阶段在 iTunes 重启后继续使用绝对隔离目录与 `127.0.0.1:49323`。`date` 从未配对
Lockdown 查询修正为显式加载 pairing file/start session，真机 JSON 与 `--nojson` 均兼容 go-ios。
同轮还验证 battery/diagnostics/diskspace/profile/cloudconfig/Accessibility get，重新从
`D:\iOSDeviceSupport\iOSDeviceSupport` 选择 `ddi-17F113`，并以 shared RSD 返回 571/14 个总/应用进程。
一次性 screenshot 从旧 screenshotr 改为 Instruments DVT，输出 828x1792 PNG；最终 tunnel、DDI、IPC
和 server 全部清理，隔离 evidence 保留且 rust-ios/go-ios process 均为 0。最终串行 Cargo gate 为
format/check/clippy `-D warnings`/222 tests/release 全通过。

随后扩展只读阶段在独立 `127.0.0.1:49324` 验证 `lang`、wallpaper、icon layout、AFC root 与
`fsync tree`。`diagnostics list` 首次对照发现 Rust 丢失 go-ios `Diagnostics/Status` envelope，并缺少 Go
typed struct 会补出的 `WiFi.Active` 与 13 个零值 `IORegistry` 字段；修正后双方 23 个 leaf 的路径和值
差异为 0。real/mock/replay 分别覆盖 subcommand gate、固定 schema 与 fixture。`ip` 在 Rust 15 秒门限和
go-ios 15 秒 bounded probe 中均未等到完整 IPv4/IPv6，记录为设备流量条件阻塞；参考进程按 PID 清理。
最终串行 Cargo gate 为 format/check/clippy `-D warnings`/223 tests/release 全通过。

2026-07-18 写操作阶段新增 `userspace_tunnel_restarts_after_data_plane_failure`：mock tunnel 首次 running 后
打开 `connect_failure`，等待 2 秒 RSD 数据面探针把 task 置为 `backing_off`，再恢复 backend；测试要求
attempts 和 `backend.tunnel_count()` 至少为 2，并通过新 endpoint 的 20-byte header + 双向 echo 证明
数据面实际恢复。`rust-ios-server` 63/63 单元测试通过。真机随后两次执行 `reboot`，修正后的 task 在设备
离线阶段进入 waiting/重试，重新枚举后从 RSD 62736/endpoint 54861 自动换到 RSD 64722/endpoint 52664，
恢复后的 `setlocation/resetlocation` 再次成功。完整写入矩阵和恢复状态见
[device-validation.md](device-validation.md)。

本阶段最终串行 gate 全部通过：format、workspace/all-targets/all-features check、Clippy `-D warnings`、
workspace/all-features 227/227 tests、workspace/all-features release build。最终 `ios.exe` 为 28,705,280
bytes，SHA-256 `CA7FD3F5CE53D94403AB581E52D372C4D428581DE69C636E0CE6C71F4122828B`。

最初“无设备应输出空列表”的环境前提当前不成立；空列表形状由 mock/in-process 测试覆盖，真机
证据必须按 [device-validation.md](device-validation.md) 更新。

## 必测错误路径

- auth token 错、protocol major 错、response ID 错。
- server metadata 陈旧、第二实例、state JSON 损坏。
- 无设备、指定 UDID 不存在、usbmuxd 不可达。
- forward 端口冲突、停止释放、并发连接、server 强杀。
- tunnel/device/UI stream 等待、退避、取消、重启追加；userspace tunnel 必须用 RSD port 的真实 connect
  探测数据面，缓存的 `rsd ls` 不能作为 adapter 存活证据。
- task output offset 越界、limit 0/超限、无 output task；stream/UI stream/WDA/XCTest/AppProcess output content type。
- finite XCTest completed/failed、server interruption 不自动重跑、显式 restart、unit/filter 完成路径，
  以及 unit 缺明确 `.xctest` bundle 的结构化拒绝。
- finite AppProcess completed、raw output、server restore 不自动重跑、stop 关闭 owned session。
- finite GPX LocationSimulation completed/取消 clear、NDJSON progress、server restore 不自动重跑和显式 restart。
- activate 多余参数、mobileactivation request 字段、Apple HTTP headers/form、非 2xx、无效/超限 plist response。
- erase 必须显式 `--udid` + 单个 `--force`；MCInstall request 只含 EraseDevice/PreserveDataPlan，mock 不接触真机。
- set-wallpaper 唯一参数、screen/Where 映射、JPEG/PNG header/dimension、image/identity 上限、supervision escalation 复用，以及 MCInstall Settings 外层/item 双 ack。
- supervised pair 标准/P12 mode 互斥、纯 Rust cert chain、SupervisorCertificate/ChallengeResponse 两阶段 Lockdown request、device payload 不含 private key、EscrowBag PairRecord round-trip。
- WebInspector JS shell page/bundle 选择、启动超时、串行 eval、单条 error 后继续、NDJSON capture、stop close、IPC process 输入和 server interruption 不重放。
- iOS 17 以下 DDI floor version matching、build-label URL 编码、本地 HTTP image/signature 下载、UDIF `koly`/size 校验、无 partial artifact 失败与关闭 server 后 cache 复用。
- 每个未支持 flag 返回 `not_implemented`；特别是 help 不得执行命令。

## 代理

需要下载 crate 或测试 host download 时可临时设置：

```powershell
$env:HTTP_PROXY = 'http://127.0.0.1:10800'
$env:HTTPS_PROXY = 'http://127.0.0.1:10800'
$env:ALL_PROXY = 'socks5://127.0.0.1:10800'
```

普通 unit/process smoke 不依赖外网。
