
### 2026-11-10 go-ios 65bbc9f 同步真机验证（iOS 27.2.1 测试 iPhone，批次 A–D）

设备：`<udid>`（测试 iPhone / iOS 27.2.1，USB）——正是 go-ios
`6910f3b`/`65bbc9f` 修复的复现机型。隔离 `--server-home`（temp 目录）+ 动态
userspace endpoint；对照 go-ios 提交信息中的复现条件。

| 检查 | 实际结果 | 结论 |
|---|---|---|
| B2 `rsd ls`（新 announce：先解析后回显） | 85 services 枚举成功；连续多次调用正常，端口不迁移 | passed |
| B2 `info display`（announce 后的真实服务连接） | deviceinfo 服务返回 6 displays、displayId 1 主屏 828x1792 | passed |
| A1 `image auto`（ensure-mounted 幂等） | personalized 签名 == dmg SHA-384 → `already-mounted` 幂等返回（两次） | passed |
| C2 cryptexd 服务发现 | iOS 27.2.1 RSD 广告 `com.apple.security.cryptexd.remote`（连同 screencaptureservice/displayservice/hid.indigo） | passed |
| C2 cryptex 挂载路径 | cryptexd.remote 命中 → cryptex 路径执行；本地 `ddi-27A5194q` 为不完整 cryptex 布局（缺 `CryptexInfoPlist`/volume 文件）→ 结构化报错 → 回退 personalized → already-mounted；warnings 记录回退原因 | passed（回退语义；完整 cryptex install 需完整 cryptex DDI，待完整资产） |
| D2 sysmontap JSON v1 帧 | 首行 attributes 帧（procAttrs 8 项/sysAttrs 37 项）+ system 帧（cpuTotalLoad 241.67、systemAttributes 按 sysAttrs 对齐）；processes 帧存在（368+ 进程） | passed |
| D3 `screenshot --display=1` | `coredevice-screencapture` 模式 4,108,592 bytes，PNG 828x1792；默认 DVT 路径 3,144,412 bytes 同尺寸 | passed |
| D4 `hid tap 414 300` | hid.indigo 连接、digitizer Start/End 事件发送成功；警告明示无活跃 display 媒体流时事件被设备丢弃（go-ios 同款门槛） | passed（结构级） |
| D5 `kill --watch com.apple.Preferences` | launch → pid 976 Running 通知命中 → 同连接 processControl `killed:true`；首次真机暴露 bundle-id 匹配缺陷（execName 为 .app 路径），已修（目标解析 CFBundleExecutable + 路径末段/stem 比对 + 单测 2 项） | passed（修复后闭环） |
| A4 syslog vis 解码 | 2062 行捕获，0 行残留 `\M-` 转义；14 行真实 CJK 文本（CoreData/WDA 中文日志） | passed |
| 共享 RSD 白名单 | 真机发现 `image`/`hid` 未列入四处白名单导致 tunnel 不传递，已补齐（real/task/replay/mock 四处） | fixed |
| 清理 | watch/sysmontap/syslog 任务 stopped；隔离 server 进程停止 | passed |

验证中修复的三个缺陷（见提交 2）：共享 RSD 白名单补 `image`/`hid`、cryptex 失败回退
personalized（go-ios `NewImageMounter` 顺序试语义）、WatchKill 目标解析与路径形态匹配。

**遗留待做**：完整 cryptex DDI 资产到位后的 cryptex install 全链路；iOS 27.2 端口迁移
（二次 announce 攻击路径）专门复现；`display start` 媒体流 server 任务；userspace tunnel
拨号超时需死路由场景专门触发。

### 2026-09-08 go-ios ec90af1 同步（TSS HTTPS + 叶子 sanity check，真机回归待做）

同步 go-ios `main-Attect-dev@ec90af1`：TSS 请求由明文 `http://gs.apple.com` 改为
`https://` + rustls 自定义 verifier（跳过链验证，叶子证书校验主机名 `gs.apple.com` 与
有效期；对齐 go-ios `verifyTssServerCert`）。实现落在 vendor `idevice-0.1.65/src/tss.rs`
受控补丁（`tss` feature 拉起 `rustls/ring`、`rustls-webpki`、`reqwest/rustls-no-provider`）。

本轮为离线与网络层验证，未接设备：

| 检查 | 实际结果 | 结论 |
|---|---|---|
| vendored 定向单测（`--features tss,ring tss::`） | 10 passed / 0 failed（DER 遍历、UTCTime/GeneralizedTime 解析、civil 日期换算、oversized length 拒绝、RestoreRequestRules 既有 6 项） | passed |
| 真实端点握手（`tss::tests::tls_handshake_with_sanity_check_reaches_real_endpoint --ignored`） | 对 `https://gs.apple.com/TSS/controller?action=2` 实测握手通过叶子 sanity check（私有 CA 场景，Windows 信任库无该根证书）；首次实现外层 SEQUENCE 少剥一层导致 `BadEncoding`，修正后通过 | passed |
| 冷 DDI 挂载 `image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport` | 未执行（本轮未接设备）；该路径是唯一真实走通 TSS HTTPS 新链路的验证 | **待真机回归** |

注意：TSS 请求现走 HTTPS；如挂载出现证书类错误，先用上述 ignored 测试定位是
verifier 拒绝（叶子主机名/有效期）还是网络层问题。
verified=true` | passed |
| 2026-07-16 | cleanup | 无 server/process/temp home | 独立 home 删除，`ios/go-ios` 进程 0 | passed |

本次不证明 Unactivated -> Activated 的网络与设备回写路径；该路径仍需专用未激活设备。

## iOS 26 SetWallpaper 安全 gate / 核心回归记录

2026-07-17 使用 release `ios.exe`（SHA-256 `E0EBE747...4190F`）、独立 `%TEMP%` server home、
动态 IPC port 和精确显式 UDID。设备刚重启，测试前没有 `ios/go-ios` 进程或已挂载 image；从
`D:\iOSDeviceSupport\iOSDeviceSupport` 临时挂载匹配 DDI。没有提供真实图片或 supervision identity，
没有发送 MCInstall Settings/Wallpaper 写请求。

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| `list --details`、`info lockdown` | 重启后 USB/Lockdown 可用 | 1 台 USB；测试 iPhone / iOS 26.5.2 / 23F84 / arm64e | passed |
| `apps --list/--all --list` | App filter 与 WDA runner 可见 | user 24、all 261；runner `com.nktest.WebDriverAgentRunner.40.xctrunner` | passed |
| `image auto --basedir ...` | 使用同步 go-ios iOS 26 修正后的本地 DDI 选择 | 选择并挂载 personalized `17F113` | passed |
| `runwda` + direct `/status` | 新 MCInstall 分支不影响 server-owned XCTest/WDA | `serviceTransport=dedicated_core_device`，动态 URL ready=true、OS=iOS | passed |
| `syslog --parse` + `task output` | 日志 task 非空且 content type 正确 | 2 秒 22,749 bytes，`application/x-ndjson` | passed |
| `forward 0 8100` + `/status` | 透明转发到 WDA 且 stop 释放 host port | 动态 host port 50835 ready=true；stop 后立即重绑成功 | passed |
| `set-wallpaper missing.png --screen=display --p12file=missing.p12` | parser 在读取文件和连接 MCInstall 前拒绝非法 screen | exit 3，结构化 `invalid_request`；未执行写操作 | passed, safety gate only |
| cleanup | 无 task/image/listener/process/temp home | task/image 0；IPC/forward ports 释放；`ios/go-ios` 0；home 删除 | passed |

该记录不证明 supervision escalation 或真实 wallpaper mutation；这两项仍需有备份的专用 supervised
设备、匹配 identity、原壁纸恢复方案和人工授权。

## iOS 26 Supervised Pairing 安全 gate 记录

2026-07-17 使用 release `ios.exe`（SHA-256 `3DF321ED...5878A`）、独立 `%TEMP%` server home、动态
IPC port 与显式脱敏设备。测试仅在内存中读取当前 usbmuxd PairRecord 并比较前后 SHA-256；没有输出
record 或 digest，没有提供有效 organization identity，也没有发送 Lockdown Pair request。

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| `pair --name=lab-host --p12file=missing.p12` | 标准与 supervised mode 在文件/设备访问前互斥 | exit 3，结构化 `invalid_request` | passed, safety gate only |
| `pair --p12file <不存在的 client path>` | client-relative P12 在设备连接前校验 | exit 3，结构化 `invalid_request` | passed, safety gate only |
| 前后 `readpair` 内存 digest | 两条失败路径不得改写 usbmuxd record | digest 相同；未持久化 digest | passed |
| cleanup | 无 server/listener/process/temp home | IPC port 释放；`ios/go-ios` 0；home 删除 | passed |

完整 SupervisorCertificate/ChallengeResponse/PairRecord save 已由 duplex device protocol 测试覆盖；真实
supervised pair 仍需专用 supervised 设备、匹配 P12、原 PairRecord 备份和恢复演练。

## iOS 26 SpringBoard App Icon 记录

设备重启后使用 debug `ios.exe`、独立 `%TEMP%` server home 和动态 loopback port；测试只经 classic
Lockdown SpringBoard service 读取图标并写 host 文件，不挂载 DDI、不创建 tunnel/task，也不修改设备。

| 日期 | 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|---|
| 2026-07-16 | `info lockdown`、`apps --list`、`image list` | 重启后基础服务可用且 DDI 未挂载 | 测试 iPhone / iOS 26.5.2 / 23F84；user app 24；image 0 | passed |
| 2026-07-16 | `get-app-icon com.apple.Preferences --output <file>` | 返回完整 PNG 并按 client path 原子写入 | 27,355 bytes；PNG signature 正确；System.Drawing 完整解码为 128x128；SHA-256 `002CF02E...5FA4` | passed |
| 2026-07-16 | 语法有效但未安装的 bundle ID | 保留 SpringBoard 实际响应 | iOS 26 返回 7,155-byte placeholder PNG，而不是 service error；需结合 `apps --all` 判断安装状态 | passed, compatibility finding |
| 2026-07-16 | cleanup | 无 task/image/process/temp artifact | task envelope `tasks=[]`、image 0、`ios/go-ios` 0、临时 home 删除 | passed |

## iOS 26 personalized DDI / RSD 记录

设备：`测试 iPhone`、iOS `26.5.2`、build `23F84`、UDID `<udid>`。Host 为
Windows x64，USB 连接；测试使用独立 server home 和动态 loopback port。早期探测发现 go-ios
仍在运行，因此其结果不作为证据；结束 go-ios 后完整重跑 tunnel、RSD 和 debug proxy 路径。

| 日期 | 命令/检查 | 预期 | 实际 | 状态变化 | 结果 |
|---|---|---|---|---|---|
| 2026-07-16 | `list --details`、`devmode get`、`image list` | 重启后 USB/Lockdown 可用 | 检测到 1 台 USB 设备；Developer Mode enabled；初始 image 为空 | 无 | passed |
| 2026-07-16 | `image auto --basedir <local-ddi-cache>` | 按 BoardID/ChipID 过滤并选择最新兼容 DDI | 从 `15F31d/17E5179g/17F113` 中选择 `17F113`，挂载到 `/System/Developer` | 挂载只读 personalized DDI | passed |
| 2026-07-16 | `image list` | 返回真实挂载信息 | `PersonalizedImageType=DeveloperDiskImage`、`ProductBuildVersion=17F113`、`IsMounted=true` | 无 | passed |
| 2026-07-16 | `tunnel start --userspace`、`tunnel refresh` | CoreDevice tunnel 与 RSD 可建立 | tunnel running；挂载后刷新得到 74 个 service，包含 DVT、AppService 和 debug proxy | 创建 host task | passed |
| 2026-07-16 | `debug proxy` + GDB remote `qSupported` | 透明 bridge 到 RSD debug proxy | 收到 180-byte capability packet，含 `PacketSize` 和 AArch64 watchpoint capability | 创建 host task | passed |
| 2026-07-16 | `kill-server` 后恢复 | tunnel/debug task 使用原 ID 恢复 | generation 增长，RSD 仍为 74 个 service，`qSupported` 再次通过 | server 重启 | passed |
| 2026-07-16 | DDI 未挂载时启动 `debug proxy` | service 未发布时不得报告 running | task 保持 `waiting`，返回明确的 RSD service not advertised 错误 | 创建后清理 host task | passed |
| 2026-07-16 | cleanup | 无 task/listener，恢复初始 image 状态 | `task stop all`、DDI unmount、`image list=[]`、server/home/port 清理完成 | 卸载 DDI | passed |

## iOS 26 app / log / forward / WDA 记录

本组沿用同一脱敏设备信息，并使用新的独立 server home 和动态 loopback port。未挂载 DDI，未启动
go-ios，未安装、卸载或启动任何 app。

| 日期 | 命令/检查 | 预期 | 实际 | 状态变化 | 结果 |
|---|---|---|---|---|---|
| 2026-07-16 | `info` | 返回稳定设备详情 | `ProductType/ProductVersion/BuildVersion/CPUArchitecture/HardwareModel` 完整，`TrustedHostAttached=true` | 无 | passed |
| 2026-07-16 | `apps --list/--system/--all` | installation_proxy 按 application type 过滤 | user 24、system 237、all 261；字段一致；检测到已安装 WDA runner | 无 | passed |
| 2026-07-16 | `apps --filesharing --list` | 只保留 file sharing app | 返回 8 项，字段形状与 `--all --list` 一致 | 无 | passed |
| 2026-07-16 | `syslog --parse`、`task output` | server 写 NDJSON，output 可分页读取 | capture 增长到 885016 bytes；前 64 KiB 有 364 个完整 JSON record，content type 正确 | 创建后停止 syslog task | passed |
| 2026-07-16 | `forward 0 62078` | 动态 host port 透明转发 Lockdown | `QueryType` 得到 297-byte 响应，包含 `com.apple.mobile.lockdown`；stop 后 host port 可立即重新 bind | 创建后停止 forward task | passed |
| 2026-07-16 | `runwda --bundleid <installed-runner>` | 已实现时创建 XCTest/WDA task，否则明确拒绝 | runner 已安装，但返回结构化 `not_implemented`；XCTestSessionTask 尚未迁移 | 无 | blocked |
| 2026-07-16 | cleanup | 无 task、listener 或 capture artifact | task list 为空，server/动态端口/temp home 已清理 | 无 | passed |

实现 server-owned WDA task 后，使用同一 runner 重新验证：

| 日期 | 命令/检查 | 预期 | 实际 | 状态变化 | 结果 |
|---|---|---|---|---|---|
| 2026-07-16 | `runwda --bundleid <installed-runner>` | xctrunner 启动并等待 WDA ready | 约 2 秒进入 running；`/status` 返回 `ready=true` 与 iOS `26.5.2` | 启动 WDA runner | passed |
| 2026-07-16 | runtime `wdaUrl` | localhost HTTP bridge 可用 | client GET `/status` 返回 WDA ready 与正确 runner bundle | 创建 host listener | passed |
| 2026-07-16 | runtime `mjpegUrl` | localhost MJPEG bridge 可用 | HTTP 200、`multipart/x-mixed-replace`，前 4096 bytes 含 JPEG SOI | 创建 host listener | passed |
| 2026-07-16 | `kill-server` 后恢复 | 同 task ID 重新启动 runner/bridge | generation `2`、ready=true，新动态 URL 可访问 | server/xctrunner 重启 | passed |
| 2026-07-16 | `task stop`、cleanup | runner 与 listener 由 task 回收 | HTTP/MJPEG ports 可立即重新 bind；DDI unmount、task/server/home 清理完成 | 停止 WDA 并卸载 DDI | passed |

## iOS 26 UI HTTP/RPC client 记录

2026-07-17 使用 release `ios.exe`、独立 `%TEMP%` server home 和动态 IPC port。设备刚重启后先只读
确认 app/image/task 状态，再从 `D:\iOSDeviceSupport\iOSDeviceSupport` 临时挂载匹配 DDI。仅验证
WDA status/raw HTTP，没有执行 tap、type、app control、orientation set、Accessibility reset 或安装。

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| `list --details`、`info`、`apps --list` | 重启后设备与 runner 可见 | 测试 iPhone / iOS 26.5.2 / 23F84 / arm64e；user app 24，runner 为 `com.nktest.WebDriverAgentRunner.40.xctrunner` | passed |
| `image auto --basedir ...` | 使用 iOS 26 DDI 修正选择匹配 image | 选择并挂载 `ddi-17F113` personalized image | passed |
| `runwda --bundleid <installed-runner>` | server-owned task 进入 running | generation 1、`serviceTransport=dedicated_core_device`，动态 WDA/MJPEG bridge ready | passed |
| `ui status --driver=wda --wda-url=<runtime-url>` | 一次性 client 请求 WDA `/status` | `ready=true`，OS iOS 26.5.2，runner build 可读 | passed |
| `ui raw --driver=wda --method=GET --http-path=/status` | raw path 保留 WDA JSON | `value.ready=true`，与 status 结果一致 | passed |
| `syslog` + `task output` | UI 改动不影响日志任务 | 短时捕获 1369 bytes，分页 output content type 为 `text/plain; charset=utf-8` | passed |
| `forward 0 8100` | 动态 forward 可访问同一 WDA | host port 52961 的 `/status` ready；stop 后可立即重新 bind | passed |
| cleanup | 无 task/image/listener/process/temp home | task/image 0；IPC/WDA/MJPEG/forward 4 个端口释放；`ios/go-ios` 0；home 删除 | passed |

同日继续验证 protocol 1.8 的 server-owned UI stream，使用另一组独立 home/动态端口：

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| 首次 `ui stream mjpeg --wda-url=<wdaUrl>` | WDA API port 提供 MJPEG 或返回明确错误 | `/mjpeg?fps=5&quality=50` 返回 HTTP 404 `Unhandled endpoint`；确认该 runner 使用独立 MJPEG port，清理后修正实现 | passed, compatibility finding |
| WDA task URL 关联 | 自动识别动态 `wdaUrl` 对应 task | UI spec/runtime 的 `wdaTaskId` 与 WDA task ID 一致，实际 stream URL 使用 `mjpegUrl/?fps=5&quality=50` | passed |
| `task output <ui-stream-id>` | 返回真实 MJPEG bytes | 首轮 477523 bytes；content type `multipart/x-mixed-replace; boundary=--BoundaryString`；前 64 KiB 含 JPEG SOI | passed |
| `kill-server` 后恢复 | WDA 与 UI stream 同步恢复并跟随动态端口 | 两个 task 均 generation 2；WDA/MJPEG 从 50845/50846 变为 53682/53683；UI stream 4 次尝试后跟随 53683，capture 增长到 26617139 bytes | passed |
| cleanup | 无 task/image/listener/process/temp home | task/image 0；旧/新 WDA/MJPEG 与 IPC 共 5 个端口释放；`ios/go-ios` 0；home 删除 | passed |

同日继续验证 protocol 1.9 的 server-owned UI runner，使用新的独立 home、动态 IPC port 和 host
port 50842；设备未安装 DeviceKit runner，因此只把 WDA 记为真机 passed：

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| `image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport` | 继续使用同步 go-ios 修正后的 iOS 26 DDI 选择 | 选择并挂载 `ddi-17F113` | passed |
| `ui run wda --bundleid <installed-runner> --host-port 50842` | 创建 `ui_runner` task 并同时启动 XCUITest/forward/health | 约 1.4 秒进入 running；device port 8100、`/status` ready=true、iOS 26.5.2、transport `dedicated_core_device` | passed |
| direct `/status` 与 `ui status --wda-url=http://127.0.0.1:50842` | host endpoint 和一次性 UI client 均可用 | 两条路径均返回 ready=true 与正确 runner build/OS | passed |
| `--log-output` 与 `task output` | 捕获真实 XCTest event NDJSON | generation 1 捕获 188 条、54014 bytes，content type `application/x-ndjson` | passed |
| `kill-server` 后恢复 | 同 task ID、同 host port 恢复并继续追加日志 | generation 2、50842 不变、ready=true，capture 增至 108026 bytes | passed |
| stop/cleanup | runner/listener/image/server/home 全部回收 | host 50842 与 IPC port 可重绑；task/image 0；DDI unmount；home 删除；`ios/go-ios` 0 | passed |
| `ui run devicekit` | 只在已安装并签名的 runner 上验证 | 当前设备未安装 DeviceKit；mock/process smoke 已覆盖 12004/`/health` | not-run |

## iOS 26 protocol 1.10 / RemotePairing 与基础回归记录

设备刚重启，Host 为 Windows x64，USB 连接；使用 release `ios.exe`、独立 `%TEMP%` server home 和
动态 IPC port。测试开始前 `ios/go-ios` 进程数为 0，故 go-ios 不参与 USB、DDI、WDA、syslog 或
forward 结果。RemotePairing 只复用已有 go-ios identity 做验证尝试，没有执行 `tunnel pair --manual`。

| 命令/检查 | 预期 | 实际 | 状态变化 | 结果 |
|---|---|---|---|---|
| `server status`、`list --details`、`info` | protocol 1.10 与重启后 USB/Lockdown 正常 | protocol `1.10`；测试 iPhone / iOS 26.5.2 / 23F84，完整字段可读 | 无 | passed |
| `apps` 四种 filter | user/system/all/filesharing 关系稳定 | 分别 24 / 237 / 261 / 8；原始 JSON 的大小写不同键可保留 | 无 | passed |
| `image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport` | 同步 go-ios `5804fa9` 后按 identity/build 选择最新 DDI | 从三组 `ddi-*` 选择并挂载 `ddi-17F113` personalized image | 挂载只读 DDI | passed |
| `tunnel start --userspace` | 默认 transport 不受 RemotePairing 改动影响 | `transport=usb_core_device`，CoreDevice tunnel 与 userspace endpoint running | 创建 tunnel task | passed |
| `runwda --bundleid <installed-runner>`、`ui status` | XCTest/WDA 与 host bridge ready | `ready=true`、iOS 26.5.2；直接 `/status` 与 UI client 均成功 | 启动 WDA runner | passed |
| `syslog --parse`、`task output` | 短时 NDJSON capture 可分页 | 2 秒累计 120916 bytes；首个 64 KiB 有 356 条有效 JSON line | 创建后停止 syslog task | passed |
| `forward 0 8100` | 动态 host port 可访问 WDA，stop 释放 | 经 forward `/status` 为 ready；stop 后 host port 可立即重绑 | 创建后停止 forward task | passed |
| `tunnel start --remote-pairing --pair-record-path <go-ios-dir>` | 只读加载 legacy identity；环境不满足时明确失败且不改文件 | identity SHA-256 前后相同；反复得到 `mDNS discovery found no _remoted._tcp.local. service within 5 seconds`，未进入 pair verify/manual consent | 创建后停止 remote tunnel task | blocked by host network interface |
| cleanup | 无 task/image/listener/process/temp home | task/image 0；DDI unmount；IPC/WDA/MJPEG/tunnel/forward ports 全部可重绑；`ios/go-ios` 0；home 删除 | 卸载 DDI | passed |

当前 Windows interface 列表没有与 iPhone 对应的 IPv6/link-local interface。RemotePairing 真机状态
仍是 blocked，不能用 identity 成功解析或 mock 数据面替代；下一次应先让 `_remoted._tcp.local.` 能
被 mDNS 发现，再继续 external RSD、pair verify、`createListener(tcp)` 与 TLS-PSK/CDTunnel 验证。

## iOS 26 finite XCTest event 记录

同一设备、Windows x64/USB；使用独立 `%TEMP%` server home 和 `127.0.0.1:61347`。设备只安装了
`com.nktest.WebDriverAgentRunner.40.xctrunner`，没有会自然结束的普通 XCUITest bundle，因此只验证
真实 testmanagerd/event/cancel 路径，不能据此把 finite completion 标为真机通过。

| 日期 | 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|---|
| 2026-07-16 | `image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport` | 选择匹配 iOS 26 DDI | 选择并挂载 `ddi-17F113` | passed |
| 2026-07-16 | `runtest <installed-runner> --log-output ...` | 创建 finite XCTest task 并捕获事件 | 进入 running，`serviceTransport=dedicated_core_device`；capture 收到约 190 条 target/log/activity NDJSON | passed |
| 2026-07-16 | `--test-to-run MissingSuite/testDoesNotExist` | 不执行 WDA 常驻测试 | runner 输出 `identifiersToRun:(null)` 并进入 `testRunner`；确认 `idevice 0.1.65` filter archive 被 iOS 26 忽略 | failed, implementation now rejects filters |
| 2026-07-16 | `task stop <id>` | 中止 session 并回收 runner | task stop 返回，随后 shared RSD `ps --apps` 无 WebDriverAgentRunner | passed |
| 2026-07-16 | cleanup | 无 task、mounted image、server/process/home | task count `0`、`image list=[]`、无 `ios/go-ios`，独立 home 删除 | passed |

完整 `completed + summary` 已由 mock/session/进程测试覆盖；真机仍需要安装一个会自然结束且包含
pass/fail/skip case 的普通 XCUITest bundle。unit XCTest 与 filters 在依赖修正前必须保持
`not_implemented`，禁止把本次 WDA 日志路径扩写为完整 XCTest 真机通过。

## iOS 26 FileService 与重启后综合回归记录

2026-07-17 使用同一脱敏设备、Windows x64/USB、独立 server home 和动态端口。go-ios 对照使用本地
`main-Attect-dev@5804fa9` 临时 build；旧的 `C:\Users/<user>\bin\ios.exe` 会固定选择不兼容的
`ddi-17E5179g` 并得到 Apple TSS status 94，因此不作为修正后基线。两边均从
`D:\iOSDeviceSupport\iOSDeviceSupport` 选择 `ddi-17F113`。用户结束先前 go-ios 后，设备仍可枚举，
没有 tunnel/listener 残留；仓库根目录遗留的 `selfIdentity.plist` 已删除并加入 ignore。

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| go-ios `file ls --crash` | FileService control 可列目录 | 返回 8 项；包含 3 个 CoreRoutine report、Jetsam、Wi-Fi、mstreamd、DiagnosticLogs 和 dtfileserviced report | passed |
| go-ios `file pull --crash` | data service 可下载选定 report | 15,990 bytes，SHA-256 `8FAC24EC...E51520B` | passed |
| Rust `file ls --temp/--crash` | shared RSD 且 server 保持存活 | temp 0 项；crash 后续为 9 项，`serviceTransport=shared_rsd` | passed |
| Rust `file pull --crash` | 与 go-ios 内容一致且原子落盘 | 15,990 bytes，完整 SHA-256 与 go-ios 相同；无 `.part` 残留 | passed |
| `file ls --app=com.apple.Preferences` | 不存在 container 不应报 internal | `invalid_request`、`retryable=false`，保留 `ContainerLookupErrorDomain` | passed |
| RSD/FileService 超时回归 | 分片异常不得永久挂起 | RSD 最多 3 次、单次 5 秒；FileService control/session/list/data connect 为 10 秒 | passed |
| `list --details`、`info`、`apps --list` | 重启后基础信息完整 | 测试 iPhone / iOS 26.5.2 / 23F84 / arm64e；user app 24，WDA runner 可见 | passed |
| `runwda`、`syslog --parse`、`forward 0 8100` | WDA/log/forward 不受 FileService 改动影响 | WDA ready；2 秒 syslog 13,968 bytes/81 条 JSON；forward `/status` ready 且 stop 后端口可重绑 | passed |
| cleanup | 无 task/image/listener/process/temp artifact | DDI unmount；server/task/process 0；所有动态端口可重绑；临时目录删除 | passed |

早期 debug build 的 FileService async state 曾在 Windows 约 1 MiB thread stack 上触发 `0xc00000fd`；
控制入口与 pull/download 的大 future 均改为 boxed boundary，回归测试确认公开 `execute` future 为 16
bytes。后续真机 ls/pull、WDA 和 server cleanup 均未再发生进程崩溃。

## iOS 26 shared RSD / DVT 记录

同一设备、Windows x64/USB；使用独立 `%TEMP%` server home、动态 loopback port，并从
`D:\iOSDeviceSupport\iOSDeviceSupport` 临时挂载 `ddi-17F113`。所有 task、DDI、server 和临时目录
均在结束后清理，最终 `image list=[]`、task count `0`。

| 日期 | 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|---|
| 2026-07-16 | `ps --apps` | 经 active tunnel 打开 DVT | 返回 10 个 application process，stderr 明确 shared RSD | passed |
| 2026-07-16 | `devicestate list` | 只读 ConditionInducer 查询 | 返回 3 组 condition，明确 shared RSD；未 enable | passed |
| 2026-07-16 | `instruments notifications` | JSONL capture 非空 | `serviceTransport=shared_rsd`，首轮 1585 bytes | passed |
| 2026-07-16 | `sysmontap` | attributes/config 后持续 sample | `serviceTransport=shared_rsd`，15 秒门限内 37517 bytes | passed |
| 2026-07-16 | protocol 1.14 `ostrace --level debug --subsystem ... --match Accelerometer --exclude Skipping` | typed filters 组合且只输出匹配 entry | iOS 26 Lockdown relay，debug 2 秒 108 条/40,218 bytes、release 1 秒 63 条/23,616 bytes；四项条件逐条通过 | passed |
| 2026-07-16 | `screenshot --stream --port 0` | DVT frame 经 MJPEG 输出 | multipart header + JPEG SOI，读取 4389 bytes | passed |
| 2026-07-16 | `kill-server` 后恢复 | tunnel 先恢复，DVT task 仍复用 | tunnel generation `2`，notifications/sysmontap 均保持 `shared_rsd` | passed |
| 2026-07-16 | `webinspector list --timeout=15` | 经 active tunnel 打开 WebInspector shim | 74 个 RSD services 中确认 `com.apple.webinspector.shim.remote`；设备随后关闭连接，CLI 返回 `device_unavailable` 并提示开启 Web Inspector | blocked by device setting |

WebInspector 本轮使用独立 `%TEMP%` server home 和 port `60261`。要完成 page list/eval，先在设备开启
“设置 > Safari > 高级 > Web 检查器”并重新连接；要验证 `webinspector launch`，还需开启“远程自动化”。
设置未开启前不得把 service advertisement 写成命令真机通过。

## iOS 26 protocol 1.7 / server-owned CDP 记录

使用 release `ios.exe`、独立 `%TEMP%` server home 和动态 loopback IPC port；从
`D:\iOSDeviceSupport\iOSDeviceSupport` 选择并临时挂载 `ddi-17F113`。本轮同时回归基础设备/App、
WDA、syslog 和 forward，确认 CDP task manager 改动没有影响既有路径。

| 日期 | 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|---|
| 2026-07-16 | `list --details`、`info`、`apps` | 重启后设备详情与 App filter 稳定 | 测试 iPhone / iOS 26.5.2；user 24、system 237、all 261、filesharing 8 | passed |
| 2026-07-16 | `image auto --basedir ...`、tunnel、`rsd ls` | 使用 iOS 26 DDI 并发布 WebInspector service | 选择 `ddi-17F113`；RSD 74 services，包含 WebInspector/DVT/AppService/debug proxy | passed |
| 2026-07-16 | `webinspector cdp --port 0 --timeout 15` | 开启设置后进入 running；关闭时明确 waiting | task 经 shared RSD 到达 service；设备关闭连接，8 次尝试后保持 `waiting` 并给出开启 Web Inspector 提示 | blocked by device setting |
| 2026-07-16 | protocol 1.13 `webinspector js-shell --timeout 10`，stdin `2+2/.exit` | server-owned shell attach page、eval 后退出并清理 | DDI/tunnel/RSD 正常；启动到达真实 service 后返回 `task_failed`，message 明确要求开启 Web Inspector；失败 shell task 自动移除，仅 tunnel 保留 | blocked by device setting |
| 2026-07-16 | `runwda` + `/status` | CDP 新分支不影响 WDA | `ready=true`、OS 26.5.2，HTTP/MJPEG 动态端口可访问 | passed |
| 2026-07-16 | `syslog --parse` | 短时 NDJSON capture 非空 | 2 秒 12,801 bytes，80 条有效 JSON record | passed |
| 2026-07-16 | `forward 0 62078` + Lockdown `QueryType` | 透明转发且 stop 释放 port | 297-byte `com.apple.mobile.lockdown` 响应；host port 可立即重绑 | passed |
| 2026-07-16 | cleanup | 无 task/image/listener/process/temp home | task/image 均 0，WDA/CDP/js-shell/IPC ports 释放，`ios/go-ios` 进程 0 | passed |

要把 CDP 标为真机通过，仍需在设备开启 Web Inspector 后验证 `/json` 至少一个 target，并通过
page WebSocket 完成 `Runtime.enable`/`Runtime.evaluate` command-response；不能用 RSD service 存在代替。

## iOS 26 Accessibility audit 记录

同一设备、Windows x64/USB，使用独立 `%TEMP%` server home 和动态 IPC port。为确认 DDI/RSD 状态，
从 `D:\iOSDeviceSupport\iOSDeviceSupport` 临时挂载 `ddi-17F113` 并启动 tunnel；AX audit 自身最终确认
使用 Lockdown `com.apple.accessibility.axAuditDaemon.remoteserver`，不是 RSD shim。

| 日期 | 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|---|
| 2026-07-16 | `ax audit --timeout 90` | 完成 AX audit 并返回结构化结果 | API v26、7 类 audit type、completion 正常、当前前台界面 0 issue、`serviceTransport=lockdown` | passed |
| 2026-07-16 | 第二次 `ax audit --timeout 30` | 关闭前次 DTX 后可重新连接 | 同样完成并返回稳定字段 | passed |
| 2026-07-16 | protocol trace | DTX callback 不阻塞 reply | TLS handshake-only 后 ACK `hostApiVersion`，再关联 `deviceApiVersion` reply | passed |
| 2026-07-16 | cleanup | 不遗留 task/image/server/process/temp home | tunnel stopped、image list `[]`、IPC port free、`ios/go-ios` 0、temp home 删除 | passed |

本轮没有执行 `resetax`；该命令会重置用户 Accessibility 设置，只保留 offline 测试。

## iOS 26 Accessibility inspector 记录

2026-07-17 使用 release `ios.exe`、独立 workspace-local server home、IPC port 56931 和同一 USB
设备。全程未传 `--font`，未执行 `resetax`，只读取 SpringBoard focus element。

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| `ax --udid ...` | 创建 server-owned inspector | generation 1 进入 running，`serviceTransport=lockdown`，started + 3 个 focus event 共 20151 bytes | passed |
| `task output --raw` | NDJSON 可解析 | 3 个 `hostInspectorCurrentElementChanged:`；FaceTime/日历/照片 caption 与 spoken description 可读，platform ID 长 40 hex 字符 | passed |
| 第二个 `ax` | 同设备互斥 | exit 3，结构化 `conflict` 指向已有 task | passed |
| `task restart` | 关闭旧 AX 状态并建立下一代 | generation 2 running；capture 累计 2 个 started、6 个 focus event | passed |
| `task stop` | 关闭 selection/monitoring 并移除 task | 69 ms 返回，`task list --kind accessibility_inspector` 为 0 | passed |
| stop 后 `ax audit --timeout 30` | AX service 可重新连接 | API v26、7 类 audit、0 issue、Lockdown completion 正常 | passed |
| cleanup | 无 listener/process/temp home | IPC port free，`ios/go-ios` 0，两个隔离 home 删除 | passed |

`--font` 会写入 Accessibility `DYNAMIC_TYPE`，本轮只验证了 mock/protocol 路径，没有在真机执行。

## protocol 1.16 / WDA event capture 记录

2026-07-17 使用 release `ios.exe`（SHA-256
`49C661670E54151DCDB31FCABA44930281193FC3A5C19EC250AB17691F2E26FF`）、workspace-local 固定绝对
server root 和独立 IPC port。没有读取或设置任何 HOME 环境变量，也没有删除测试目录。

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| protocol/capability | 新 client 只与支持 WDA capture 的 server 执行 `runwda` | protocol `1.16`，server 发布 `wda_event_capture` | passed |
| mock `runwda --log-output <absolute-path>` | listener event 写入 NDJSON，runtime 暴露 counters | generation 1，1 条 `test_plan_started`，144 bytes，`application/x-ndjson` | passed |
| `task output` + server restart | 同 task ID 恢复并追加，不覆盖已有 capture | generation 2，2 条 event，288 bytes；stop/kill-server 成功 | passed |
| iOS 26 DDI 同步复核 | 与 go-ios `5804fa9` 的本地 `ddi-*` 选择修正一致 | BoardID/ChipID 过滤、Apple build 排序和 `17F113` 选择测试通过；本轮未挂载 | passed, offline only |
| Windows USB/PnP | 设备与 Apple usbmuxd 可见 | PnP 显示 Apple iPhone/USB Device 正常；`AppleMobileDeviceProcess.exe` 监听 27015 | passed, host only |
| rust-ios 与 go-ios `list` | usbmuxd 发布重启后的设备 | 两者均返回 `deviceList=[]`；没有 `ios/go-ios/lldb` 残留 | blocked by Apple device publication |
| `info/apps/image/runwda/syslog/forward` | 仅在获得明确 UDID 后执行 | 因枚举为空未执行，不能沿用历史结果宣称本轮通过 | not-run |

真机后续条件是解锁设备并确认“信任此电脑”，使 Apple usbmuxd 再次发布 UDID。恢复后需使用新的固定
绝对 server root，依次执行 `list --details`、`info`、`apps --all --list`、
`image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport`、`runwda --log-output -`、
`task output`、`syslog --parse` 和 `forward 0 8100`；不得在 `list` 为空时继续设备命令。

## protocol 1.17 / unit XCTest 与 test filters 记录

2026-07-17 使用 release `ios.exe`（SHA-256
`C9E4AA7750ABE544F893E15B2F4AD27BC3B6735B4371A89FDFE776B8059141E3`）、固定绝对 server root 和
独立 IPC port。没有读取或设置任何 HOME 环境变量，也没有删除证据目录。

| 命令/检查 | 预期 | 实际 | 结果 |
|---|---|---|---|
| protocol/capability | unit/filter client 只连接支持新语义的 server | protocol `1.17`，server 发布 `xctest_unit_filters` | passed |
| fork archive/configuration/launch | unit module/bundle/injection 正确，filters 不再使用 `NSArray<String>` | 4 项定向测试通过；`NSSet` + `XCTTestIdentifierSet`，`initializeForUITesting=false` | passed, offline only |
| release mock `runtest --xctest --test-to-run/--test-to-skip` | finite task 完成且输出所选 case | generation 2，`DemoCase/testSelected`，8 条 event/1344 bytes | passed, offline only |
| unit bundle validation | 缺失、非 `.xctest`、空 stem 或 target app 明确失败 | 3 条 release CLI gate 与 server `invalid_argument` 测试通过 | passed, offline only |
| 当前 rust-ios/go-ios `list` | usbmuxd 发布刚重启设备 | 两者仍为 `deviceList=[]`，对照 go-ios exit 0 后 `ios/go-ios/lldb` 进程数为 0 | blocked by Apple device publication |
| unit/filter 真机运行 | 使用会自然结束的普通 unit/XCUITest bundle | 无 UDID 且当前设备未提供普通测试 bundle，因此未启动 runner | not-run |

证据目录为 `target/process-tests/xctest-unit-filters-protocol-117-final` 和
`target/process-tests/device-recheck-protocol-117-final`。结束 go-ios 没有破坏 rust-ios server 或
protocol；当前阻塞仍发生在 Apple usbmuxd 的设备发布层。

## 执行前检查

1. 明确设备所有者允许的操作范围。
2. 备份重要数据；destructive 组使用专用测试设备。
3. 记录设备 iOS major/minor、USB/network、是否已配对、Developer Mode 状态。
4. 使用 `--server-home <workspace 内的绝对路径>` 和独立 server port，避免影响用户实例；禁止依赖
   HOME 环境变量。
5. 先运行 `list --details`，确认目标 `--udid`，禁止依赖“第一台设备”。
6. 禁止把 `readpair`、`info` 全量输出直接附加到 issue/CI artifact。
7. 测试后 `task stop all`、`kill-server`，核对没有 listener/capture 遗留。

## A. 只读基础组

| 命令 | 预期 | 状态 |
|---|---|---|
| `list`, `list --details` | USB/network 形状与 go-ios 对比 | passed（Windows USB / iOS 26） |
| `listen` | attach/detach 各产生一个新 snapshot，Ctrl+C exit 0 | not-run |
| `buid` | 与参考实现一致 | passed（iTunes 重启后 Windows usbmuxd 返回有效值） |
| `readpair --udid` | JSON 可解析；只在安全 terminal 查看 | not-run |
| `info`, `info lockdown` | ProductVersion/DeviceName/UDID 等关键字段一致 | passed（iOS 26 Lockdown） |
| `info display` | shared RSD；四顶层字段、display 数量和值与 go-ios 一致 | passed（RSD 74 services，6 displays，`activeOn`/portrait/faceUp） |
| `lockdown get` | key/domain/全部查询 | passed（iOS 26 `ProductVersion=26.5.2`） |
| `devicename`, `date` | 与设备显示/参考实现一致 | passed（paired Lockdown session；JSON numeric + `formatedDate`，`--nojson` RFC850 UTC） |
| `lang` | language/locale 和支持列表与参考实现一致 | passed（paired Lockdown；58 languages / 369 locales，未执行 set） |
| `assistivetouch/voiceover/zoom/timeformat get` | 字段类型和值与设备设置一致 | passed（三项 Accessibility=false，timeformat=12h） |

## B. 只读 Service 组

| 命令 | 预期 | 状态 |
|---|---|---|
| `apps --system/--all/--list`, `--filesharing` | bundle/filter 正确 | passed（iOS 26，24 user / 237 system / 261 all / 8 filesharing） |
| `diskspace` | total >= free，used = total - free | passed（iOS 26 AFC） |
| `diagnostics list`, `batterycheck`, `batteryregistry` | plist 转 JSON，无字段混用 | passed（diagnostics 与 go-ios 同为 23 leaves，envelope/字段/值差异 0；不展开含 serial 的 registry 证据） |
| `mobilegestalt <safe-key> [--plist]` | JSON/XML 与 go-ios envelope 一致；不改变设备 | passed（iOS 26 `ProductVersion` 返回 `MobileGestaltDeprecated`，外层 `Status=Success`） |
| `ps`, `ps --apps` | PID/应用过滤与参考一致 | passed（iOS 26 shared RSD，多轮 `--apps` 10/14 项；最近总进程 571） |
| `image list` | 已挂载 image 列表，不改变 mount | passed（iOS 26，挂载前/后/卸载后） |
| `image auto --basedir`（iOS 17+） | 先核对选中的 build/identity，再授权 personalized mount | passed（iOS 26，旧版 DDI 下载改动后复测仍选择 `17F113`，mount/list/unmount 闭环） |
| `image auto --basedir`（iOS < 17） | 下载或复用匹配 `.dmg/.signature`，mount/list/unmount，失败无 partial cache | passed（iOS 15.8 兼容覆盖选择本地 15.5 cache，返回 `imageVersion=15.5` 并挂载到 readonly `/Developer`） |
| `get-icon-layout` | JSON 可 round-trip，但不调用 set | passed（4 个顶层项、65 个 bundle entry；不输出排列明细） |
| `get-app-icon <bundleID>` | PNG signature/尺寸/完整解码有效；失败不留下 partial file | passed（iOS 26，Settings 128x128） |
| `get-wallpaper` | PNG signature/尺寸有效 | passed（828x1792 / 2,474,758 bytes；文件仅留在本地 evidence） |
| `screenshot` | PNG signature/尺寸有效 | passed（iOS 26 shared RSD，828x1792 / 4,605,578 bytes） |
| `fsync tree --path /` | 递归路径、type、size 可解析，不改变文件 | passed（232 项：129 dir / 102 file / 1 symlink） |
| `ip --timeout 60` | 产生流量后返回匹配静态 MAC 的 IPv4/IPv6 | blocked（Rust 15 秒 timeout；go-ios bounded probe 同期 15 秒未完成且已按 PID 清理） |
| `prepare cloudconfig/printskip` | IsSupervised/complete 字段与 Configurator 对比；skip 列表稳定 | passed（`CloudConfiguration.IsSupervised=false`，Status=Acknowledged，69 个 skip key） |
| `profile list` | MCInstall envelope/Status 可解析，不安装或删除 profile | passed（Identifier/Manifest/Metadata/Status） |
| `devicestate list` | condition/profile ID 与设备支持列表一致，不 enable | not-run |
| `webinspector list/eval` | page 字段可解析；只读 eval 返回预期值 | blocked（service advertised，设备 Web Inspector 设置关闭） |
| `webinspector cdp` | `/json` target 与 page WebSocket CDP command/response | blocked（offline/restart 已测，设备 Web Inspector 设置关闭） |
| `webinspector js-shell` | attach page 后连续 eval；error 后 session 仍可用；退出后 task/session 清理 | blocked（offline/IPC 已测，设备 Web Inspector 设置关闭） |
| `ax audit` | API version/types/issues 可解析，重复执行可重新连接 | passed（iOS 26，API v26，7 类 audit type） |

## C. 持续任务组

| 能力 | 验证 |
|---|---|
| forward | 多连接、半关闭、大 payload、device detach、server restart、stop 释放 port |
| userspace tunnel | CoreDevice handshake、RSD port、20-byte endpoint、并发 TCP、restart |
| debug proxy | RSD service、GDB remote handshake、server restart、stop 释放 port |
| app debug | installation_proxy app/path 匹配、Windows LLDB `remote-ios` target、launch、退出/断连回收 client-owned proxy |
| syslog | raw/JSONL、stop、detach/attach、文件追加 |
| ostrace | PID/process 等待、level/subsystem/message filters、JSONL 字段、CPU 负载、stop |
| pcap | Wireshark 打开、PID/process 过滤、restart 后仅一个 global header |
| app process | AppService/OpenStdio、raw output、EOF completed、restart、stop 回收 owned PID |
| webinspector cdp | `/json` discovery、单活动 WebSocket、command/event、restart、stop 关闭 device session 并释放 port |
| accessibility inspector | AX focus NDJSON、同设备互斥、restart、stop 关闭 selection/monitoring；真机不传 `--font` |

userspace tunnel、RSD registry、debug proxy、forward、syslog 和 AppProcess 的 iOS 26 基础路径已通过：
RSD 74 services，forward 到 WDA port 8100 可访问且 stop 释放 host port，syslog 捕获超过 250 KiB，
OpenStdio 捕获 238 bytes 并在 PID 退出后进入 completed。
Accessibility inspector 两代 session、6 个真实 focus event 和 stop 后 audit 重连也已通过。
MobileGestalt 使用安全 key `ProductVersion` 完成 JSON/XML 双路径和 go-ios `5804fa9` 对照：两者均为
外层 `Status=Success`，内层 `Diagnostics.MobileGestalt.Status=MobileGestaltDeprecated`；`--plist`
直接输出可解析 XML，测试前后 `ios/go-ios` 进程均为 0。
`info display` 复用同一 server-owned userspace tunnel 的 `com.apple.coredevice.deviceinfo`：Rust 与
go-ios 均返回 `backlightState/current/displays/orientation`，6 个 display、`activeOn` 和相同
portrait/faceUp orientation。测试结束 tunnel stop、DDI unmount exit 0，`image list=[]`、进程为 0。
20-byte endpoint 并发、detach/attach、pcap 真机路径及 AppProcess stop 的真机 PID 回收尚未执行。capture 可能包含用户内容和网络
流量，只能在专用设备/网络执行并及时清理。

2026-07-17 app debug 离线/主机侧验证：两套隔离 process smoke 均完成 `.app` preflight、LLDB source
capture 和退出后 task 清理；runtime 单测验证 owner IPC 断开自动清理且 state 不持久化。Windows LLVM
17 的 `remote-ios` plugin 成功加载 `GZBank_WDA.ipa` 内真实 arm64 Mach-O，并设置 device platform file
spec。真机在本阶段重新枚举时 `deviceList=[]`，因此 installation_proxy 路径匹配、GDB connect 和 app
launch 不作为已通过证据，待设备恢复后补测。

### 2026-07-17 Windows Kernel Tunnel 非管理员验证

| 命令 | 预期 | 结果 | 状态变化 | 结论 |
|---|---|---|---|---|
| protocol 1.18 release + 不存在的绝对 `--wintun-path` | 不接触设备/adapter，明确失败 | task `failed`，message 含 `Wintun DLL does not exist or is not a file` | 仅创建后停止隔离 task | passed |
| protocol 1.18 release + `C:\Windows\System32\wintun.dll` | 非管理员必须在设备解析前明确失败 | task `failed`，message 含 `permission denied` 和 `os error 5` | 仅创建后停止隔离 task | passed |
| stop + kill-server | 释放 task、server PID 和 IPC port | 最终复跑 PID 14308 退出，port 60231 无 listener | 隔离 server 已关闭 | passed |

证据目录为 `target/process-tests/kernel-tunnel-protocol-118-final`；release SHA-256 为
`5D70FAA6C086D9BDC4D7081214E597EA114B9AEF275FCBCDC3943D2F5D6DE619`，Wintun DLL SHA-256 为
`E5DA8447DC2C320EDC0FC52FA01885C103DE8C118481F683643CACC3220DAFCE`。当前进程非管理员且
rust-ios/go-ios 均未看到设备，因此 adapter 创建、raw IPv6 packet pump 和 RSD 真机连接均为
`not-run`，不能由 mock 或失败路径推断成功。

### 2026-07-17 Protocol 1.19 Dproxy 与设备重新枚举

| 检查 | 结果 | 结论 |
|---|---|---|
| Windows PnP | Apple composite/device/WPD 三个节点均为 `OK`，VID/PID 为 `05AC/12A8`，driver 538.0.0.0、ProblemCode 0 | USB/PnP 层存在设备 |
| rust-ios release `list --details` | protocol 1.19 隔离 server 返回 `deviceList=[]` | Apple 未向 rust-ios 发布设备 |
| go-ios `5804fa9` `list` | 返回 `deviceList=[]` | 与 rust-ios 一致 |
| dproxy -> Apple `127.0.0.1:27015` -> go-ios | 捕获 357-byte `ListDevices` plist 与 237-byte response；response 为 `DeviceList: []` | 空列表来自 Apple upstream，不是 go-ios JSON 或 rust-ios parser |
| dproxy -> Apple -> 第二个 rust-ios server | 正确使用裸 `IP:port` 后完成 72 次连接、0 failed，26,627/17,064 directional bytes；所有枚举仍为空 | rust-ios 通过显式 proxy 的真实数据面正常 |
| Apple process 恢复 | 原 PID 19428 停止后 AppX ACL 拒绝直接启动；重启 iTunes 后新 `AppleMobileDeviceProcess` PID 19964 监听 `127.0.0.1:27015` | Apple usbmuxd listener 已恢复，但首次 `DeviceList` 仍为空 |
| `pnputil /restart-device` | 非管理员首次返回 `Access is denied`；UAC 提权后 exit code 0，三个 PnP 节点重新枚举为 `OK` | 可作为物理重插的主机侧软重启；必须在 Apple listener 恢复后再次触发 arrival event |
| listener 恢复后的第二次 PnP restart | go-ios 与 rust-ios 分别返回同一台 `<udid>` USB 设备 | 无需物理拔插；结束 go-ios 没有留下 listener 或进程占用 |

dproxy task `20f95d2b-df7d-4a82-98d6-cb787d5f0ad0` 使用 loopback 60243 与真实 Apple upstream，
连接统计、raw/parsed capture、stop 均通过；证据位于
`target/process-tests/device-recheck-protocol-119-final/device-dproxy-events.ndjson*`。第二个 rust-ios
证据目录为 `target/process-tests/device-dproxy-client-protocol-119-final-2`。该早期 capture 使用的 release
SHA-256 为 `9C049ED302E1293E89567A26FC43A9A7A3C9FBFDCC6A884C0B222CCC5058D15E`。

设备恢复后使用最终 release（28,642,304 bytes，SHA-256
`881CD81FE88B3768FE17E726D48E0406B0B2B0BBFB9686EE528FB7AD2AD6B4A2`）和新绝对隔离目录
`target/process-tests/device-core-matrix-protocol-119-final-after-itunes` 完成以下回归：

| 检查 | 实际结果 | 结论 |
|---|---|---|
| `info lockdown` | 测试 iPhone / iOS 26.5.2 / 23F84 / arm64e；Activated，TrustedHostAttached | passed |
| `apps --list` / `apps --all --list` | user 24、all 261；runner `com.nktest.WebDriverAgentRunner.40.xctrunner` | passed |
| `image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport` | 初始 image 0，选择并挂载 personalized `ddi-17F113` | passed |
| `runwda` / direct `/status` / `task output` | generation 1、`dedicated_core_device`、ready=true；188 条 / 61,902 bytes NDJSON | passed |
| `syslog --parse` / `task output` | 3 秒 14,645 bytes；首个 page 103 条有效 NDJSON | passed |
| `forward 0 8100 --usb` | 动态 host port 53290 的 `/status` ready=true；stop 后立即可重绑 | passed |
| cleanup | WDA/MJPEG/forward/IPC ports 可重绑；task/image 0；`ios/go-ios` process 0 | passed |

DDI 已显式卸载；没有执行 pair、安装、文件写入、定位、Developer Mode、reboot/shutdown 或 UI mutation。
隔离证据目录保留，没有删除既有 capture。最终 fake-upstream dproxy process smoke 证据另存于
`target/process-tests/dproxy-protocol-119-final-4`。

### 2026-07-18 Protocol 1.20 Shared XCTest RSD

iTunes 重启后 `AppleMobileDeviceProcess` 已运行但最初没有监听 27015。普通权限执行
`pnputil /restart-device "USB\VID_05AC&PID_12A8\<usb-instance>"` 返回 `Access is denied`；
通过 UAC `RunAs` 后 exit code 3010，随后 PnP 状态为 `OK/CM_PROB_NONE`，Apple listener 恢复到
`127.0.0.1:27015`。go-ios `nk-20260610` 与隔离 rust-ios protocol 1.20 均重新发现同一 USB 设备。

专项 offline process smoke 位于
`target/process-tests/shared-xctest-protocol-120-smoke-2`：WDA/UI runner restore generation 2、finite
XCTest explicit restart generation 2、shared tunnel generation 4，三类 XCTest consumer 均为
`shared_rsd`，最终 state tasks 为空，标记 `SHARED_XCTEST_PROCESS_SMOKE_OK`。

真机主回归位于 `target/process-tests/device-shared-rsd-protocol-120-real-1`：

| 检查 | 实际结果 | 结论 |
|---|---|---|
| protocol/device | server `1.20`，USB device_count=1；测试 iPhone / iOS 26.5.2 / 23F84 | passed |
| `apps --list` / `apps --all` | user 24、all 261 | passed |
| `image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport` | 选择 personalized `ddi-17F113` 并挂载 | passed |
| tunnel / RSD | userspace tunnel running；74 services，包含 dtservicehub 与 testmanagerd.remote | passed |
| `runwda` generation 1 | `serviceTransport=shared_rsd`、ready=true；189 条 / 62,177 bytes NDJSON | passed |
| syslog / forward | 1,272 条全部有效 NDJSON；动态 port 52811 转发到 8100，`/status` ready | passed |
| server restart | tunnel/WDA/syslog/forward 均 generation 2；WDA 仍 `shared_rsd`，forward 原 port ready；WDA capture 126,231 bytes，syslog 6,338 条 | passed |

增加 info 级 transport 日志后又以最终 release 在独立目录
`target/process-tests/device-shared-rsd-protocol-120-real-transport-log-1` 重跑 tunnel + WDA。WDA 一次
进入 ready，runtime 为 `shared_rsd`，194 条 / 64,055 bytes event；server.log 中
`reusing caller-owned shared RSD connector for DVT and testmanagerd` 恰好 1 条，
`creating dedicated CoreDeviceProxy + RSD stack` 为 0 条。这同时给出正向 shared 分支证据和没有第二套
CoreDeviceProxy 的反向证据。

最后依次停止 WDA/syslog/forward/tunnel，两个隔离 state 均为 `tasks=[]`；DDI unmount 后 image list
为空，IPC/WDA/forward/tunnel ports 无 listener，`ios.exe` 进程为 0。没有执行 pair、安装、文件写入、
定位、Developer Mode、reboot/shutdown 或 UI mutation；全部 evidence 目录保留。

### 2026-07-18 Protocol 1.20 只读核心与单张 Screenshot

iTunes 再次重启后，参考 go-ios 进程已经退出；AppleMobileDeviceProcess 恢复，原隔离 rust-ios server
仍监听 `127.0.0.1:49323`，显式指定绝对 `--server-home` 与该非默认 port 后重新发现同一 USB 设备。
证据保留在
`C:\Users/<user>\RustroverProjects\rust_ios\target\process-tests\device-readonly-core-protocol-120-1`。

| 检查 | 实际结果 | 结论 |
|---|---|---|
| 基础只读 | `buid`、`lockdown get ProductVersion`、`devicename`、`timeformat get`、`devmode get` | passed |
| `date` | 原实现未启动 paired session，设备拒绝请求；修正后返回 numeric epoch、`formatedDate` 与 RFC850 UTC plain text | passed |
| service 只读 | battery 两条路径、diagnostics list、diskspace、profile list、cloudconfig、printskip | passed；registry serial 未写入文档 |
| Accessibility get | AssistiveTouch/VoiceOver/Zoom 均为 false | passed；未执行任何 toggle |
| iOS 26 DDI | 绝对 basedir 仍选择 `ddi-17F113`，挂载到 `/System/Developer` | passed |
| tunnel / `ps` | shared RSD；571 个总进程、14 个 application process，未发现 WebDriverAgentRunner | passed |
| 单张 `screenshot` | 先发现旧 screenshotr 路径 `InvalidService`；改为 task manager 注入 active tunnel + Instruments DVT channel | passed；shared RSD warning，828x1792 PNG / 4,605,578 bytes |
| screenshot evidence | SHA-256 `E7B55967E533980C10046A5F5B4ECEA594833AA80E205174FB9BD1DAE8B6FA08` | 文件保留，不展示屏幕内容 |
| cleanup | tunnel task stop、DDI unmount、task/image 为空、IPC 49323 与 tunnel 52758 无 listener | passed；rust-ios/go-ios process 0，server metadata 已移除 |
| final Cargo gate | format/check/clippy `-D warnings`/test/release 串行通过；workspace 222/222 tests | release 28,672,512 bytes，SHA-256 `4FB3E1C6665E200766E5D11942C8E8E00D0F933ED78E333800C41A5E002F01D9` |

参考 go-ios 在没有其自身 tunnel agent 时同样以 `InvalidService` 拒绝 iOS 26 screenshot；源码对照确认其
正确路径为 `com.apple.instruments.dtservicehub` +
`com.apple.instruments.server.services.screenshot`。Rust 修正复用已有 server-owned shared RSD，没有创建
第二套长期进程。未执行 pair、安装、定位、UI mutation、Developer Mode 修改、reboot 或 shutdown。

### 2026-07-18 Protocol 1.20 扩展只读与 Diagnostics 对照

使用新的绝对隔离目录
`C:\Users/<user>\RustroverProjects\rust_ios\target\process-tests\device-readonly-extended-protocol-120-1`
和 IPC `127.0.0.1:49324`。本阶段没有挂载 DDI、没有创建持续 task，也不输出 wallpaper、icon layout、
AFC 文件名、网络地址或设备 serial。

| 检查 | 实际结果 | 结论 |
|---|---|---|
| `lang` | 当前 language/locale 可解析；58 个 supported language、369 个 locale | passed；未执行 set |
| `get-wallpaper` | 828x1792 PNG / 2,474,758 bytes；SHA-256 `0851FBD4082198C9978E97BB4D0178FE545979CE2A2DA956CC1F092597DE0D3E` | passed；内容仅保留在本地 evidence |
| `get-icon-layout` | 15,623-byte JSON；4 个顶层项、65 个 bundle entry | passed；未执行 set/回写 |
| `file ls --path /` | 16 项，所有 row 含 path | passed；未输出名称 |
| `fsync tree --path /` | 232 项：129 directory、102 regular file、1 symlink | passed；未执行 pull/push/rm/mkdir |
| 初始 `diagnostics list` | Rust 错误地解包为四个裸 subsystem；go-ios 顶层为 `Diagnostics/Status` | compatibility bug found |
| 修正后 `diagnostics list` | 双方 `Status=Success`；5 个 subsystem、23 个 leaf，路径和值差异 0 | passed；裸 `diagnostics` exit 3/invalid_request |
| `ip` | Rust `--timeout=15` 结构化超时；go-ios bounded probe 15 秒也未完成 | blocked by missing simultaneous matching IPv4/IPv6 traffic；参考 PID 已终止且无残留 |
| cleanup | task/image 为空，server PID 43308 退出，IPC 49324 无 listener | passed；rust-ios/go-ios process 0，metadata 已移除 |
| final Cargo gate | format/check/clippy `-D warnings`/test/release 串行通过；workspace 223/223 tests | release 28,689,408 bytes，SHA-256 `BBCA0B827B9E52CF30CC28813F356B280B529935E2C67A352DB479F6087BB5AB` |

`crash ls` 未纳入只读组，因为当前实现会先调用 `flush_reports`；不能把它当成零状态变化的查询。`ip`
错误文本可能包含设备 MAC，证据与文档均不展开该值。

### 2026-07-18 Protocol 1.20 可逆写操作与 reboot 后 tunnel 自愈

本阶段使用完全测试用 `测试 iPhone / iOS 26.5.2` USB 设备、隔离目录
`C:\Users/<user>\AppData\Local\Temp\rust-ios-codex-write-audit` 和 IPC `127.0.0.1:62106`。安装测试使用
`C:\Users/<user>\ClionProjects\WebDriverAgent\build\Debug-iphoneos\WebDriverAgentRunner-Runner.app` 的
签名产物；WebDriverAgent 项目的远程 macOS 构建主机可达且为 Xcode 26.6。本节不记录完整 UDID、设备
serial、文件内容、Wi-Fi credential 或 P12/private key。

| 检查 | 实际结果 | 结论/恢复状态 |
|---|---|---|
| `uninstall -> apps -> install -> apps` | WDA runner 先消失、重新安装后恢复可见 | passed；恢复测试前已安装状态 |
| AFC 与 WDA app container | 临时目录完成 mkdir/push/pull，SHA-256 相同；单项 rm 与 recursive rm 后均不存在 | passed；临时文件已删除 |
| `fsync mkdir/push/pull/rm` | 单文件和目录 pull 均把 `--dstPath` 当作最终目标；内容 hash 与递归结构一致 | passed；修正了错误追加 source basename 的实现 |
| `fsync rm --r --path /` | 返回结构化 `invalid_request` | passed；根目录保护未被绕过 |
| `launch` / `kill` | MobileSafari 启动返回 PID 1944；kill 后新 `ps` 列表中 PID 消失 | passed；修正 DVT `killPid:` wire argument/reply |
| `setlocation` / `resetlocation` | 上海坐标写入后立即清除，runtime 均为 `shared_rsd` | passed；未保留模拟位置 |
| `timeformat` | `12h -> 24h -> 12h` 每一步均读回 | passed；恢复原值 |
| AssistiveTouch / VoiceOver / Zoom | 三项分别完成 `false -> true -> false` 读回 | passed；全部恢复原值 |
| `lang` | `zh-Hans/zh_CN -> en-US/en_US -> zh-Hans/zh_CN` | passed；恢复可写的原 language/locale |
| language 异常路径 | 原始读取 alias `zh-Hans-CN` 不在 SupportedLanguages，设备返回 `SetProhibited` | fixed；增加 supported-value preflight 和 locale rollback |
| `set-icon-layout` | 首次原样写回时 SpringBoard 补入 orphan app `com.zplay.popstar`；以规范化布局再写一次完全幂等 | passed；保留 SpringBoard 规范化结果 |
| `profile remove` | 删除 `com.pgyer.encrypted-profile-service`，随后 `profile list` 为空 | passed；没有剩余 profile |
| `crash cp/rm` | 备份并删除本轮产生的 3 个 `DTServiceHub-2026-07-18-153*.ips`，随后列举为 0 | passed；临时本地备份随 cleanup 删除 |
| `devmode reveal` | 命令成功，Developer Mode 仍为 enabled | passed；没有执行 disabled -> enabled 转换 |
| 第一次 `reboot` | 返回 restarting，约 25 秒后 USB 重新枚举且 Lockdown 恢复 | passed |
| reboot 后旧 tunnel | 旧实现错误保持 `running`，`setlocation/resetlocation` 返回 `I/O error: adapter closed` | compatibility/lifecycle bug found |
| 修正后的正常探针 | task `attempts=1`，RSD 62736、endpoint 54861；两轮探针后 location 写入/清除成功 | passed；正常数据面无误报 |
| 修正后的第二次 `reboot` | 离线阶段 task 进入 `waiting`，attempts 从 1 增至 10；设备约 25 秒后重新枚举 | passed；旧 adapter 已注销/关闭 |
| tunnel 自动恢复 | 同一 task 以 attempts 11 回到 `running`，RSD 变为 64722、endpoint 变为 52664 | passed；东京坐标写入/清除再次为 `shared_rsd` |
| cleanup | `task stop all`、`kill-server`；IPC 62106 与旧/新 endpoint 52210/54861/52664 均无 listener | passed；隔离 home 和本轮 8 个其他 Temp artifact 已删除 |
| final Cargo gate | format/check/clippy `-D warnings`/test/release 串行通过；workspace 227/227 tests | release 28,705,280 bytes，SHA-256 `CA7FD3F5CE53D94403AB581E52D372C4D428581DE69C636E0CE6C71F4122828B` |

未执行的边界：普通 `profile add` 仍需要设备端人工批准；Wi-Fi 缺少隔离测试 SSID/password；HTTP proxy
和 wallpaper 需要可用 supervision P12；`shutdown` 没有重启后的主机侧开机闭环；`erase` 只有在可完成
擦除后激活、配对和恢复时才能执行。因此这些能力仍保留 `device-test-needed`，不以 mock 或部分前置
检查冒充真机通过。

### 2026-07-20 Protocol 1.20 多 iOS 版本只读与 AFC 回归

使用最终 release `ios.exe`、隔离目录 `target/process-tests/device-multiversion-20260720` 和 IPC
`127.0.0.1:62220`，显式选择两台 USB 测试设备：iOS 17.5.1 (21F90) 与 iOS 26.5.2 (23F84)。

| 检查 | iOS 17.5.1 | iOS 26.5.2 |
|---|---|---|
| `list --details` | USB，设备枚举成功 | USB，设备枚举成功 |
| `listen` 拔插 | 捕获 detach、短暂重枚举和最终 attach；最后快照含两台设备 | 连接保持 |
| `readpair` | JSON 结构可解析；只在内存核对字段，未落盘 PairRecord | 同左 |
| Lockdown/diagnostics | `ProductVersion=17.5.1`；diagnostics `Status=Success`、5 个 subsystem | `ProductVersion=26.5.2`；diagnostics `Status=Success`、5 个 subsystem |
| `file ls --path /` | 1200 rows；1182 个嵌套反斜杠条目，18 个带 metadata | 15 rows，全部带 path 和 metadata |
| `fsync tree --path /` | 1039 项：787 directory、252 regular；0 个不安全路径 | 230 项：127 directory、102 regular、1 symlink |
| `image list` | 1 个既有挂载 image；本阶段未 mount/unmount | 0 个 image；本阶段未 mount/unmount |

iOS 17 的 AFC root listing 会返回 `PublicStaging` 下的扁平化 stale 条目。旧实现把反斜杠当成非法
单文件名，随后 `fsync tree /` 失败；修正后统一规范 `/` 与 `\\`、去除当前目录重复前缀，只对遍历子项的
明确 `AFC ObjectNotFound` 跳过，仍拒绝 `..`、绝对路径和空组件。修正后的两台真机结果如上，路径
遍历保护单测通过。

本阶段只执行读取、监听和路径遍历，没有 pair、安装、文件写入、定位、Developer Mode、reboot、
shutdown、UI mutation 或 image mount/unmount；设备原有 image 状态未改变。
测试结束后隔离 server、task、`62220` listener、`ios/go-ios` 进程均为 0；证据目录保留。

最终 Cargo gate 按 format、check、clippy `-D warnings`、test、release 串行通过；workspace 测试
为 228/228。最终 release SHA-256 为 `2313CB6517E58ABA037F94E15276C5165669568D3359DDA4F2B0471FC1372D3D`。
### 2026-07-20 iOS 15.8 legacy 路径只读基线与兼容镜像尝试

新连接设备为 `iPhone9,2 / iOS 15.8 (19H370)`，使用同一隔离 server home 和 IPC `127.0.0.1:62220`。

| 检查 | 实际结果 | 状态 |
|---|---|---|
| Lockdown/date/timeformat/readpair | 全部 exit 0；readpair 只在内存解析，未落盘 | passed |
| diagnostics/battery/diskspace/profile | diagnostics `Status=Success`、5 个 subsystem；diskspace 关系一致；profile 为空 | passed |
| apps/AFC | user app 36、filesharing 7；AFC root 14 rows；`fsync tree /` 547 项（156 dir/391 regular） | passed |
| Accessibility get | AssistiveTouch=true、VoiceOver=false、Zoom=false；未执行 set | passed |
| `image list`（尝试前） | 已有 1 个 readonly Developer image，mount path `/Developer` | observed |
| 本地 legacy cache | `D:\iOSDeviceSupport\iOSDeviceSupport` 含 15.7 `.dmg/.signature` 及更旧版本；15.7 signature SHA-256=`ACB3C5809014F94C7993CFA2F732D1F9D027085D9500E4C3244B6469CFB77D6B` | available, mount failed |
| 开发用 Mac Xcode 26.6 | iPhoneOS DeviceSupport 含 15.0/15.2/15.4/15.5/16.0/16.1/16.4；没有 15.7/15.8 | 15.5 available |

`info` 返回 `ProductType=iPhone9,2`、`ProductVersion=15.8`、`BuildVersion=19H370`、`BoardId=10`、
`ChipID=32784`、`HardwareModel=D11AP`。旧版 `image auto --basedir` 按 legacy floor 表选择 15.7，
设备返回结构化 `device_unavailable: ImageMountFailed`；这说明 15.7 floor 选择并不代表该缓存的签名
适用于这台设备。

为验证兼容版本，先显式卸载原有 `/Developer` 镜像（`image unmount` 返回 `status=unmounted`，随后
`image list=[]`），再挂载 开发用 Mac Xcode 26.6 的 15.5 原始镜像：`DeveloperDiskImage.dmg` 9,093,272
bytes，`.signature` 128 bytes，signature SHA-256=`3FF0E57A302B03AAD3039362337C7411A2957636BDE905D2A58586BEFBC0E0A9`。
挂载返回 `status=mounted`；随后 `image list` 显示 1 个 `DiskImage/Developer`、`hfs`、`/dev/disk2`、
`IsMounted=true`、`IsReadOnly=true`、`MountPath=/Developer`，并且 `ps --apps` 成功返回应用进程记录。

因此，iOS 15.8 (`19H370`) 已实机证明可使用 开发用 Mac Xcode 和本地 legacy cache 的 15.5 DDI 作为兼容
镜像；程序已增加 15.8/15.8.x -> 15.5 兼容覆盖，后续 `image auto` 会直接选择 15.5，不再尝试失败的
15.7 floor。

使用新 release 重复完整闭环：先 `image unmount`，再执行
`image auto --basedir D:\iOSDeviceSupport\iOSDeviceSupport`，返回
`productVersion=15.8`、`imageVersion=15.5`、`source=cache`、`status=mounted`；随后 `image list`
返回单个 `/Developer`、`IsMounted=true`、`IsReadOnly=true`，`ps --apps` 成功返回应用进程记录。


## D. 可逆写操作组

按顺序逐项授权：

- AFC app sandbox push/pull 临时文件并删除。
- `fsync mkdir/push/pull/rm` 只操作带随机名称的临时目录，并核对 bytes 与递归结果。
- `setlocation` 后立即 `resetlocation`。
- 安装专用测试 IPA，验证后 uninstall。
- launch/kill 专用测试 app。
- `set-icon-layout` 仅使用刚读取的原始 layout 做 no-op round-trip。
- 先读取并记录 `assistivetouch/voiceover/zoom/timeformat`，切换后验证并立即恢复原值。
- `lang --setlocale/--setlang` 只使用专用设备；先记录原值，允许 SpringBoard 重启，完成后恢复。
- 普通 `profile add` 只安装可识别、可删除的测试 mobileconfig；记录 identifier 后立即 remove。
- `wifi` 只使用隔离测试 SSID；安装后验证 warning/连接，再按同一 SSID `--remove`。

任何一步失败都停止本组，不继续下一个写操作。

## E. 高风险组

以下命令不得在日常设备自动执行：

- `devmode enable/reveal`
- Developer Disk Image mount/unmount
- `reboot`, `shutdown`
- `erase`、`set-wallpaper`、`pair --p12file`（均已实现但普通验证禁止执行）、完整 `prepare`、supervised profile、Wi-Fi/global proxy
- `devicestate enable`（会持续改变设备网络/thermal 等 condition）
- `resetax`（会重置用户 Accessibility 设置）

要求专用设备、明确人工确认、恢复步骤和测试窗口。`erase` 必须同时提供显式 `--udid` 与 `--force`。

MCInstall 高风险组还必须准备：原始 cloud config、原 language/locale/timezone、测试 profile identifier、
Wi-Fi SSID、proxy identifier，以及可用的 supervision identity 备份。验证顺序为 read cloud config -> 单一
写操作 -> read back -> remove/恢复 -> 再进入下一项。任何 P12/password/private key 都不得写入结果模板。

### 2026-08-07 go-ios v1.2.1 同步真机验证（pasteboard 闭环与协议差异修复）

使用本仓库 release `ios.exe`（28,801,536 bytes，2026-08-07 构建）、workspace 内隔离绝对 home
`target/process-tests/device-verify-20260807` 和动态 IPC port。设备为文档同一台
`测试 iPhone / iOS 26.5.2 / 23F84`，USB 连接。

| 检查 | 实际结果 | 结论 |
|---|---|---|
| 设备枚举 | 初始 `list` 返回 `deviceList=[]`（PnP OK、AppleMobileDeviceProcess 运行中）；UAC 提权 `pnputil /restart-device` 两次（第二次 exit 3010，与 2026-07-18 记录一致）后重新发布同一台 `<udid>` | passed；usbmuxd 直接探测确认空列表来自 Apple upstream |
| `info` / `apps --list` | 测试 iPhone / iOS 26.5.2 / 23F84，Activated；user app 24 | passed |
| DDI / tunnel / RSD | 设备已挂载 Personalized DDI（`/System/Developer`，用户原有状态，本轮未动）；userspace tunnel running；`rsd ls` 返回 **83 个 service**，包含 `com.apple.coredevice.pasteboardservice` 与 `com.apple.instruments.dtservicehub` | passed |
| **协议差异发现** | tunnel refresh 后 `rsd ls` 报 `malformed XPC message`；定位为新增的 XPC `body_len >= 8` 校验误伤——rust-ios 的 XPC body 直接承载 XPCObject 编码（空字典控制帧可小于 8 字节），与 go-ios 含 8 字节 body header 的结构不同；移除该校验后 83 services 正常 | fixed；go-ios 检查不适用于 rust-ios 协议结构 |
| `pasteboard get` | 原始剪贴板空：`items=0`、`changeCount=5` | passed |
| `pasteboard set` | `status=set`、`serviceTransport=shared_rsd`；内容未回显 | passed |
| `pasteboard get` 读回 | `text='rust-ios-verify-20260807'`、`items=1`、`changeCount=6` | passed；set/get 闭环一致 |
| `pasteboard set ""` 恢复 | 剪贴板清空，`text=''` | passed；恢复原始空状态 |
| `readpair` | 解析 10 键 PairRecord（Device/Host/Root cert+key、EscrowBag、SystemBUID 等），仅在内存核对，未落盘 | passed |
| `webinspector list` | 结构化错误 `Web Inspector is unavailable; enable Settings > Safari > Advanced > Web Inspector...` | blocked by device setting（与历史一致） |
| `launch com.apple.mobilesafari` | warning `reused server-owned shared RSD tunnel`；返回 `pid=2413`；`ps --apps` 16 个 app process 中可见 MobileSafari 2413 | passed |
| `kill com.apple.mobilesafari` | `status=killed`；随后 `ps --apps` 中 MobileSafari 消失 | passed |
| cleanup | `task stop all` 后 `tasks=[]`；`kill-server`；IPC port 无 listener；无 `ios.exe` 进程残留；未修改设备原有 DDI/image 状态 | passed |

本轮未执行：erase/shutdown/set-wallpaper/supervised pair/mdm 真机写（无 supervision identity 资产）、
WebInspector 设置开启、RemotePairing（Windows 无 iPhone IPv6 接口）、Wintun（非管理员）。
mdm 四子命令仅完成 mock/离线测试，真机路径待监督设备与 identity 资产。


## go-ios 对比模板

```text
Date:
Host OS:
rust-ios commit:
go-ios commit: 5804fa9
Device/iOS (redacted):
Connection:
Command:
Expected semantic fields:
rust-ios exit/stdout/stderr:
go-ios exit/stdout/stderr:
State changed:
Result:
Notes:
```
### 2026-08-10 WebInspector RSDCheckin 修复与 mdm/RemotePairing 验证

使用本仓库 release `ios.exe`（28,806,656 bytes，2026-08-10 构建，含 RSDCheckin 修复）、
workspace 内隔离绝对 home 和动态 IPC port。设备为文档同一台 `测试 iPhone / iOS 26.5.2 / 23F84`，
USB 连接；用户已开启 iOS 26 新路径的 Web 检查器与远程自动化
（设置 > App > Safari浏览器 > 高级）并保持 Safari 打开必应页面。

| 检查 | 实际结果 | 结论 |
|---|---|---|
| `mdm security-info`（自签名 identity） | 设备返回 `Status: CertificateRejected` 结构化错误，未改变设备状态；identity 用后即删 | passed（未监督设备拒绝路径；完整写路径仍需监督设备 + 受信任 identity） |
| WebInspector 根因 | rust-ios 连接 `com.apple.webinspector.shim.remote` 后未执行 RSDCheckin 握手（go-ios `RsdCheckin` 发送 `RSDCheckin` plist 并消费两条响应），iOS 26 设备立即 EOF → 误报 "Web Inspector is unavailable"；go-ios 对照在同一设备成功 | fixed；`rsd_checkin` 已加入 vendored ios-core 并在 `connect_service` tunnel 路径调用 |
| `webinspector list` | 返回 Safari 必应页面（与 go-ios 输出一致），`automationAvailability=WIRAutomationAvailabilityAvailable` | passed |
| `webinspector eval 1 "1+1"` / `document.title` | `2` / `"搜索 - Microsoft 必应"` | passed |
| `webinspector cdp --port 0` | task running，runtime `endpoint=127.0.0.1:54009`；`/json` discovery 返回 1 个 page target；page WebSocket 完成 `Runtime.enable` + `Runtime.evaluate` 返回页面标题 | passed |
| `webinspector js-shell` | stdin `1+1`/`JSON.stringify({a:1})` 输出 NDJSON 结果，`.exit` 后 task 清理为 0 | passed（需先停 CDP task：同设备 webinspector 连接互斥，占用时 js-shell 启动超时而非明确拒绝——已记录为可改进点） |
| RemotePairing 网络层 | iPhone 同网（<device-lan-ip>，单播 ping 2ms）；mDNS 环境验证正常（join 组后可收多播）；`tunnel pair --manual` 与单播/多播 mDNS 探测均无 remoted 实例 | blocked by device-side advertising（网络阻塞已排除，iOS 26 设备未广告 `_remoted._tcp`，host 侧不可解） |
| cleanup | 各隔离 server `task stop all` + `kill-server`；IPC/CDP 端口无 listener；无 `ios.exe` 进程残留 | passed |

本轮未执行：erase/shutdown/set-wallpaper/supervised pair/mdm 写路径（设备未监督）、
Wintun（非管理员）、App Store Connect（无真实账号）。
### 2026-08-10 续：webinspector launch 补测与 CDP/js-shell 互斥改进

同一设备（测试 iPhone / iOS 26.5.2），release `ios.exe`（28,806,656 bytes，2026-08-10 构建，
含互斥改进）、隔离 home 与动态 IPC port。

| 检查 | 实际结果 | 结论 |
|---|---|---|
| `webinspector launch https://www.apple.com` | RSDCheckin 修复后补测：返回 `{"bundleID":"com.apple.mobilesafari","title":"Apple","url":"https://www.apple.com/"}`，远程自动化路径可用 | passed |
| CDP 运行时启动 js-shell | **立即**返回 `{"code":"conflict", ..."already has WebInspector CDP task ...; the webinspector service allows a single connection per device"}`，不再是 20 秒超时 | passed（互斥改进，task manager Conflict 语义） |
| 停止 CDP 后 js-shell | `2+2` → `4` 正常执行，互斥不误伤 | passed |
| js-shell 结束后 CDP | `running`，新动态端口 54891 正常提供服务 | passed |
| cleanup | `task stop all` + `kill-server`；IPC/CDP 端口无 listener；无 `ios.exe` 残留 | passed |

互斥实现：`start_webinspector_cdp` 与 `start_webinspector_js_shell` 互相检查同设备
`WebInspectorJsShell`/`WebInspectorCdp` task，返回 `TaskManagerError::Conflict`；
server 单测新增双向互斥用例（8/8 通过）。
### 2026-08-13 runtest 完整 completion 与 WDA 测试资产构建

使用本仓库 release `ios.exe`、隔离 home 与动态 IPC port；设备为文档同一台
`测试 iPhone / iOS 26.5.2 / 23F84`。WDA 项目（`C:\Users/<user>\ClionProjects\WebDriverAgent`）
经远程 macOS 构建主机（开发用 Mac 构建主机，Xcode 26.6）构建测试资产。

**WDA 项目构建修复**（本机源码，已同步远程）：
- `FBMainViewButtonLabels` 文件级数组字面量初始化改为 `__attribute__((constructor))` 惰性填充
  （Xcode 26.6 clang 拒绝非编译期常量）
- `SceneDelegate.m/.h` 从未加入 pbxproj（文件在磁盘但 IntegrationApp target 缺引用）→ xcodeproj 补引用
- `FB_SAFARI_BUNDLE_ID` 只声明无定义 → FBSession.m 补定义
- IntegrationTests_1/2/3、UnitTests、IntegrationApp 缺 **Copy frameworks phase**
  （WebDriverAgentLib.framework 动态链接但未嵌入 → dyld 加载失败 "Failed to load the test bundle"）
- 手动重签必须带 entitlements（xcent），否则安装报
  "missing the application-identifier entitlement"
- 产物经 .ipa 打包传输（zip 保留符号链接；scp 目录会丢 symlink）

| 检查 | 实际结果 | 结论 |
|---|---|---|
| `debug <app_path>`（WDA Runner.app） | preflight（installation_proxy 匹配设备路径）、debug.lldb 执行（remote-ios/arm64/env-vars）、debug proxy task 建立与 client-owned 清理全部通过；`process connect` 报 `remote connections are not supported` —— Windows 官方 LLVM lldb 无 gdb-remote 插件 | passed（链路）/ blocked by lldb build |
| `runtest` XCUITest（IntegrationTests_1） | 安装后自然结束：`completed: 3 passed, 56 failed, 0 skipped`；7058 条 NDJSON 事件含 case/test_plan/target 全生命周期；56 失败为 WDA 测试对 iOS 26 兼容问题 | **passed（完整 completion 闭环）** |
| unit XCTest（UnitTests + IntegrationApp host） | host 启动正常、注入 dylib 存在可加载；go-ios 对照已到测试执行阶段（环境/资产就绪）；rust-ios fork 报 "test runner did not connect"（iOS 26 testmanagerd 会话握手） | partial；fork 需 DTX 层专项调试 |
| test filters（--test-to-run/--test-to-skip） | 单元/archive 测试已覆盖；真机随 unit 握手修复后验证 | 待 fork 修复 |
| cleanup | 测试 server/task/端口清理；保留 go-ios agent（用户环境）与设备已装测试 app | passed |

未执行：erase/shutdown/set-wallpaper/supervised pair、RemotePairing（设备未广告 remoted）、
Wintun（非管理员）。
