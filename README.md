# Muka Rust iOS

一个 Rust 实现的 iOS 设备操作工具，是 [go-ios](https://github.com/danielpaulus/go-ios) 的 Rust 重构。

## 项目信息

| 项目 | 内容 |
| --- | --- |
| 名称 | Muka Rust iOS |
| 作者 | Attect |
| 网站 | https://muka.cool/rust_ios |
| 仓库 | https://github.com/Attect/muka_rust_ios |
| 许可证 | MIT |
| 版本 | 0.1.0（首版开发快照） |
| 工具链 | Rust 1.96+，edition 2024 |

## 架构概述

项目只产出一个 `ios` 可执行文件，包含两种角色：

- 普通调用是短生命周期的 client，负责解析参数并连接本机 server。
- 同一文件的隐藏 `server nodaemon` 模式是本机常驻 server，持有所有持续资源。

forward、userspace/kernel tunnel、dproxy、syslog、ostrace 和 pcap 均由 server 持有，并在 server
重启后按 desired state 自动恢复；用户不再需要为每项能力保留一个 CLI 进程。

Workspace 分为四个 crate，依赖方向严格单向：

- `rust-ios-protocol`：IPC 协议定义，不依赖其他 workspace crate 或设备库。
- `rust-ios-core`：全部设备协议与 backend（`idevice` / mock / replay / recording）。
- `rust-ios-server`：常驻 server、task manager、desired-state 恢复。
- `rust-ios-cli`：命令行入口，不直接调用 `idevice`。

当前 Windows 是已执行完整进程测试的首要平台；IPC、状态模型、设备 backend 和 userspace 数据面
不绑定 Windows API，macOS/Linux 的结构边界已建立但尚未完成宿主机实测。参考基线为 go-ios
`main-Attect-dev@5804fa9`。

## 已可用能力

- ADB 风格自动启动、`start-server`、`kill-server`、`server status` 和前台调试模式。
- token 认证、protocol `1.20` 握手、16 MiB 帧限制、UUID request ID 和结构化错误。
- 原子 desired-state、独占 server lock、损坏 state 隔离、优雅关闭和强杀后恢复。
- server 托管 forward、userspace CoreDevice tunnel、Windows Wintun kernel tunnel、RSD registry、syslog、ostrace、pcap、DVT
  notifications/sysmontap/device condition、Accessibility inspector、MJPEG screenshot stream、
  WebInspector CDP、WDA session、
  WDA/DeviceKit UI runner 与 UI HTTP stream capture 文件；iOS 17+ DVT/WebInspector 命令和 stream 复用同设备
  server-owned tunnel。
- server 托管 USB CoreDevice 与 RemotePairing TCP 两类 userspace tunnel；RemotePairing identity
  由 server home 管理，manual consent 是一次性请求，不创建长期子进程或持久化 PIN。
- server 托管 dproxy 显式 usbmuxd endpoint：双向 raw dump、初始 usbmuxd plist NDJSON、连接统计、
  stop/restart/恢复和 `task output` 已离线测试；Windows 不移动 Apple listener，utun/DTX decode 尚未实现。
- server-owned WDA/XCTest runner、NDJSON event/result capture 与 `task output`；`.xctestrun` v1/v2
  parser、unit XCTest/test filters 和有限测试任务的 completed/failed 恢复语义。unit/filter 真实设备
  路径已接入受控 `idevice 0.1.65` fork；iOS 17+ 在同设备已有 server-owned tunnel 时，WDA、
  XCTest 和 UI runner 的 DVT/testmanagerd sockets 全部复用 shared RSD，不再创建第二套
  CoreDeviceProxy。普通 unit/filter bundle 仍需真机验证。
- WDA/DeviceKit 一次性 UI HTTP/RPC client：status、raw API、tap/swipe/longpress/type/button、
  screenshot/source/size/orientation 和 app control；WDA session 按需创建，也可显式复用。
- host-only UI artifact 下载与纯 Rust `.app`/`.ipa` 重签名；`ui install` 在签名后通过真实 backend
  调用 installation_proxy，支持本地 artifact 或 WDA/DeviceKit 默认下载源。
- host-only App Store Connect ES256 JWT/JSON:API、iOS Development certificate/P12 和 per-device
  provisioning profile workflow；profile reuse 模式不创建或 revoke certificate。
- `list/listen/readpair/buid/pair/info/lockdown/apps/fsync/install/uninstall` 等基础设备命令；`file`
  同时支持 AFC/HouseArrest 和 iOS 17+ shared-RSD FileService app/app-group/temp/crash domain。
- AFC diskspace、AMFI devmode、image mounter、DVT process control、诊断、定位、截图和
  SpringBoard 读取/布局命令；iOS 17+ personalized image 可从本地 Xcode DDI cache 按设备 identity
  自动选择，包含 iOS 26 build 选择修正。精确到 flag 的状态见[功能矩阵](docs/feature-matrix.md)。
- WebInspector page list、Remote Automation launch、JavaScript eval 和 server-owned CDP endpoint；CDP
  提供 Chrome `/json` discovery 与 page WebSocket，支持 stop/server restart。iOS 17+ 复用同设备
  shared RSD tunnel，设备关闭 Web Inspector/Remote Automation 时返回可操作的结构化错误。
- Lockdown language/locale、AssistiveTouch、VoiceOver、Zoom 和 12/24 小时时制读写；写操作
  必须在真机清单中单独授权。
- Accessibility `ax audit` 支持 API v20/v21+ selector、issue/rect/platform element/label 解析；
  `resetax` 仅完成 offline 验证，因会重置用户设置而不进入普通真机 smoke。
- MCInstall profile、Wi-Fi、global HTTP proxy、prepare/cloud config，以及纯 Rust supervision
  identity 生成和 PKCS#12/CMS silent install；真实设备写操作尚待专用设备验证。
- `idevice`、mock、replay、recording 四类 backend；PairRecord 录制强制脱敏。
- 无设备单元/集成测试，以及隔离 home/随机端口的 Windows 子进程、并发和恢复测试。

未完成入口一律返回 `not_implemented` 结构化错误，不会伪造成功。

## 构建与快速开始

```powershell
cargo build --workspace --release
target\release\ios.exe list
target\release\ios.exe server status --pretty

# server 持有任务，命令立即返回 task ID
target\release\ios.exe forward 8100 8100 --udid <UDID>
target\release\ios.exe tunnel start --userspace --udid <UDID>
# Windows 管理员进程；默认加载 C:\Windows\System32\wintun.dll
target\release\ios.exe tunnel start --wintun-path C:\Windows\System32\wintun.dll --udid <UDID>
# iOS 17+ 网络 RemotePairing；首次使用需在设备端人工确认
target\release\ios.exe tunnel pair --manual --udid <UDID>
target\release\ios.exe tunnel start --userspace --remote-pairing --udid <UDID>
target\release\ios.exe syslog --udid <UDID>
target\release\ios.exe pcap --process MyApp --udid <UDID>
target\release\ios.exe devicestate enable <CONDITION_ID> <PROFILE_ID> --udid <UDID>

# 显式连接返回的 endpoint 才会被抓取；不会占用 Apple 的 127.0.0.1:27015
target\release\ios.exe dproxy --mode=usbmuxd --upstream 127.0.0.1:27015 `
  --output C:\captures\dproxy-events.ndjson

# 使用 runwda task runtime 返回的动态 wdaUrl
target\release\ios.exe runwda --bundleid <RUNNER_BUNDLE_ID> --log-output - --udid <UDID>
target\release\ios.exe ui status --driver=wda --wda-url <WDA_URL>
target\release\ios.exe ui source --driver=wda --wda-url <WDA_URL> --output hierarchy.json

# 启动 server-owned runner，并在固定 host port 提供 UI backend
target\release\ios.exe ui run wda --host-port 8100 --udid <UDID>
target\release\ios.exe ui run devicekit --host-port 12004 --udid <UDID>

# 有限 XCTest task；unit 模式要求明确的 .xctest bundle
target\release\ios.exe runtest --test-runner-bundle-id <TEST_HOST_BUNDLE_ID> `
  --xctest-config UnitTests.xctest --xctest --test-to-run UnitTests.TestCase/testExample `
  --udid <UDID>

# host-only，不需要连接设备
target\release\ios.exe prepare create-cert --p12password <PASSWORD>
target\release\ios.exe ui download all --output .\ui-artifacts
target\release\ios.exe sign app --path Runner.app --p12file identity.p12 `
  --profile Runner.mobileprovision --output Runner-signed.app
target\release\ios.exe sign certificate appstoreconnect --asc-key-id <KEY_ID> `
  --asc-issuer-id <ISSUER_ID> --asc-private-key .\AuthKey.p8 --p12-output identity.p12

target\release\ios.exe task list --pretty
target\release\ios.exe task output <TASK_ID> --raw
target\release\ios.exe task stop all
```

相对 `--output` 路径在 client 侧解析成绝对路径。未指定输出时，capture 保存在
`%LOCALAPPDATA%\rust-ios\captures`。普通命令连接失败会自动启动 server，无需先运行
`start-server`。

## 文档

- [命令手册](docs/commands.md)
- [架构设计](docs/architecture.md)
- [IPC protocol 1.20](docs/ipc-protocol.md)
- [go-ios 迁移差异](docs/migration.md)
- [完整功能矩阵](docs/feature-matrix.md)
- [测试与证据](docs/testing.md)
- [真机验证清单](docs/device-validation.md)
- [Windows 运维](docs/windows.md)
- [开发与扩展](docs/development.md)
- [首版实施计划](docs/implementation-plan.md)

## 质量门禁

Cargo gate 必须串行执行：

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
# 固定工具链组件不可用时，可使用同版本 stable 的 clippy
rustup run stable cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo build --workspace --release
```

进程测试隔离 `RUST_IOS_HOME` 和 server port，不会操作用户实例；普通测试不需要真实设备。

## 已知限制

项目尚未达到 go-ios 全功能等价：

- dproxy 的显式 usbmuxd endpoint 已实现并完成离线生命周期测试；系统级 socket 替换、utun 与
  DTX decode 仍未实现。
- Windows kernel tunnel 已完成 Wintun 数据泵、server 生命周期和离线测试，但真实
  adapter/CoreDevice 数据面仍需提权真机复验。
- `debug <app_path>`、WebInspector js-shell、supervised pairing、unit XCTest 与 test filters 已接入，
  其中 app debug 与 unit/filter 完整真机运行仍需设备测试；MCInstall supervision escalation 已接入。

## 实现说明

本项目由 Attect 主导，使用 AI 辅助实现。
