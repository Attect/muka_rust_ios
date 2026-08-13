# go-ios 功能迁移矩阵

参考基线：`C:\Users/<user>\GolandProjects\go-ios`，分支 `main-Attect-dev`，提交 `5804fa9`。
矩阵按命令和 flag 的实际行为记录，不以“能编译”代替协议验证。

状态：

- `implemented`：实现和与风险相称的当前环境验证已完成。
- `offline-tested`：实现已通过 mock/replay/进程测试，尚未执行真实设备协议验证。
- `device-test-needed`：真实 `idevice` service 路径已接入，但需真机验证。
- `planned`：入口会返回结构化 `not_implemented`。
- `intentionally-changed`：功能存在，但生命周期按 server/client 设计有意改变。

## Server/Client 控制面

| 能力 | 状态 | 证据或边界 |
|---|---|---|
| 单一 `ios.exe` client/server | offline-tested | 自动启动与显式 start/kill 进程 smoke |
| loopback IPC、token、protocol 1.20 | offline-tested | codec/auth/version/request ID、交互 task input、typed OsTrace options、client-owned debug lease、WDA event、XCTest unit/filter/shared RSD、Windows kernel tunnel 与 persistent dproxy capability 测试 |
| `server status/nodaemon` | offline-tested | Windows 原生 detached spawn，前台模式可调试 |
| 独占 lock、metadata、日志 | offline-tested | 第二实例拒绝，强杀后陈旧 metadata 自愈 |
| desired-state 原子持久化 | offline-tested | 损坏 state 隔离、manager/server 重启恢复 |
| `task list/show/stop/restart` | offline-tested | 统一控制 forward/tunnel/stream |
| `task output --offset/--limit/--raw` | offline-tested | 1 MiB 限制、base64 wire、文本、WDA/XCTest/dproxy NDJSON 和 PCAP 测试 |
| 16 个并发 client | offline-tested | `smoke-resilience.mjs` |

## 设备发现与基础服务

| 命令/能力 | 状态 | 当前范围 |
|---|---|---|
| `--version`, `version`, `--help`, `help <cmd>` | offline-tested | legacy help 在连接 server 前处理 |
| `list [--details]` | implemented | Windows Apple Mobile Device Process 只读验证通过 |
| `listen` | offline-tested | server registry watch + 超时长轮询，Ctrl+C 退出 |
| `buid` | implemented | usbmuxd `ReadBUID`；iTunes 重启后 Windows 真机返回有效 BUID |
| `readpair` | implemented | 显式 `--udid`；输出可能含私钥，recording 会脱敏；2026-08-07 iOS 26 真机解析 10 键 PairRecord（仅内存核对，未落盘） |
| `pair [--name]`, `pair --p12file [--password]` | device-test-needed | 标准 trust-dialog 路径；supervised P12 challenge-response、纯 Rust cert chain/CMS、private-key 隔离和 usbmuxd save 已离线测试；真实 record 替换未执行 |
| `info`, `info lockdown` | implemented | Windows USB Lockdown；iOS 26 stable identity 字段真机通过 |
| `info display` | implemented | server-owned shared RSD `com.apple.coredevice.deviceinfo`；iOS 26 与 go-ios 四顶层字段/6 displays 一致 |
| `lockdown get` | implemented | key/domain/全部值；iOS 26 `ProductVersion=26.5.2` 真机通过 |
| `devicename`, `date` | implemented | `date` 显式启动 paired Lockdown session，兼容 go-ios `TimeIntervalSince1970/formatedDate` 与 RFC850 `--nojson`；iOS 26 真机通过 |
| `ip [--timeout seconds]` | device-test-needed | Lockdown WiFiAddress + pcapd Ethernet/VLAN 源地址识别；Rust 15 秒结构化超时，go-ios 同环境 15 秒 bounded probe 也未完成，待设备同时产生匹配的 IPv4/IPv6 流量 |
| `activate` | device-test-needed | 幂等 ActivationState fast path；mobileactivationd 三阶段 request、Apple DRM/activation HTTP、本地 server 合约测试；未激活真机待测 |

## 持续任务

| 命令/能力 | 状态 | 生命周期变化与边界 |
|---|---|---|
| `forward <host> <device>` / `--port` | intentionally-changed | iOS 26 Lockdown `QueryType` 真机转发与 stop 释放端口通过；echo、冲突、强杀恢复已测 |
| `forward list/stop/clear` | offline-tested | 新增统一任务控制语义 |
| `tunnel start --userspace` | intentionally-changed | server 持有 CoreDeviceProxy/jktcp；iOS 26 USB 建链、refresh、server restart 和设备 reboot 后数据面探针自动重建真机通过；兼容 go-ios 20-byte handshake |
| `tunnel pair --manual`、`tunnel start --remote-pairing` | device-test-needed | `_remoted._tcp`、external RSD、RemoteXPC/pair verify、TLS-PSK/CDTunnel/jktcp 已接入；identity/一次性授权/重启/echo 进程测试通过，当前 Windows 真机缺少 iPhone IPv6/link-local interface，mDNS 发现阶段阻塞 |
| `tunnel ls/stop/refresh/stopagent` | intentionally-changed | 操作 task，不调用独立 agent HTTP API |
| Windows kernel tunnel | device-test-needed | Wintun path/admin gate、IPv6 packet validation、mock lifecycle/RSD reuse/restart 已测；当前会话非管理员且 usbmuxd 未发布设备，真实 adapter/CoreDevice 数据面待复验 |
| `syslog [--parse] [--output]` | intentionally-changed | iOS 26 JSONL capture、分页 output 和 stop 真机通过；server 写 raw/JSONL capture |
| `ostrace --pid/--process [--follow] [--level/--subsystem/--match/--exclude]` | implemented | typed options、组合过滤、state/restart 双 process smoke；iOS 26 真机 108 条组合过滤全通过；`idevice 0.1.65` 固定采集全量 trace，filters 不降低设备侧负载 |
| `pcap --pid/--process [--output]` | intentionally-changed | server 写标准 PCAP；header/追加恢复已测 |
| `devicestate list` | implemented | iOS 26 shared RSD 返回 3 组 condition，复用 warning 真机通过 |
| `devicestate enable <condition> <profile>` | intentionally-changed | server 持有 DVT session；stop 时 disable，server 重启恢复已测 |
| `screenshot --stream [--port]` | intentionally-changed | server 托管 MJPEG listener；iOS 26 shared RSD 返回 multipart/JPEG，stop/restart/offline 恢复已测 |
| `instruments notifications`, `sysmontap` | intentionally-changed | iOS 26 shared RSD 真机写入 JSONL；sysmontap attributes/嵌套 batch、task output、stop/restart 已测 |
| `debug proxy [--bind/--port]` | intentionally-changed | server-owned GDB remote listener，复用 tunnel RSD service；iOS 26 `qSupported`、server restart、DDI 缺失 waiting 真机通过 |
| `debug [--stop-at-entry] <app_path>` | device-test-needed | typed app/installation_proxy preflight、Windows extended path 清理、前台 LLDB source、client-owned IPC lease；2026-08-13 真机完成 preflight（设备已安装 Runner 路径解析）、debug.lldb 执行（remote-ios platform、arm64 target）、debug proxy task 建立与断连清理；**Windows 官方 LLVM lldb 不支持 `process connect`（无 gdb-remote 插件）**，完整 launch 需支持 gdb-remote 的 lldb 构建 |
| `dproxy [--binary] [--mode] [--bind/--port/--upstream/--output]` | intentionally-changed | Windows 使用强制 loopback 的显式 server-owned usbmuxd endpoint，不移动 Apple socket；双向 raw、初始 plist/redaction、半关闭、fake upstream、output、stop/restart/restore/端口释放已测；utun、系统级拦截和 DTX decode 未实现 |
| `webinspector cdp` | offline-tested | server-owned Axum `/json` discovery/page WebSocket；单活动连接、stop 释放、server restart 与两套 process smoke |
| `webinspector js-shell` | offline-tested | server-owned Inspector session、串行 eval input、NDJSON result/error、stop close、server interruption 不重放；mock CLI/process smoke 通过 |

## App、文件与主机路径

| 命令/能力 | 状态 | 当前范围 |
|---|---|---|
| `apps --system/--all/--list/--filesharing` | implemented | iOS 26 user 24、system 237、all 261、filesharing 8；字段与过滤关系真机通过 |
| `install --path` | implemented | installation_proxy；iOS 26 签名 WDA `.app` 安装、apps 读回真机通过；IPA package helper 已离线测试，进度事件 planned |
| `uninstall <bundleID>` | implemented | installation_proxy；iOS 26 WDA 卸载、apps 不存在读回真机通过，随后重新安装恢复测试前状态 |
| `file ls/pull/push` | implemented | 无 domain selector 保留 AFC；无 tunnel 的 `--app` 保留 HouseArrest，active tunnel 时改用 FileService；iOS 17/26 AFC root listing 真机通过，iOS 26 AFC 与 WDA app container 完成 mkdir/push/pull/SHA-256/rm 真机闭环，`--crash` ls/pull 也已通过 |
| `file --app-group/--crash/--temp` | implemented | iOS 17+ shared RSD FileService control/data；四 domain process smoke、原子 pull、inline/stream push、1 GiB 上限；缺 tunnel 明确拒绝 |
| `fsync tree [--app]` | implemented | AFC/HouseArrest；iOS 17.5.1 根目录返回 1039 项（787 dir/252 regular），兼容 `PublicStaging` 扁平化 stale `ObjectNotFound` 条目且保持路径遍历保护；iOS 26.5.2 返回 230 项（127 dir/102 regular/1 symlink） |
| `fsync pull/push/rm/mkdir [--app]` | implemented | 流式复制、递归 pull、根目录删除保护；iOS 26 文件与目录的 go-ios `dstPath` 精确目标语义、内容 hash、递归删除及 `/` 结构化拒绝真机通过 |
| `crash ls/cp/rm` | implemented | crashreportmover + 递归 AFC + glob basename 匹配；iOS 26 备份并删除 3 个本轮 DTServiceHub report，随后 ls 为 0 |
| `profile list/remove` | implemented | MCInstall；iOS 26 删除测试 profile 后 list 为空 |
| `profile add` | device-test-needed | 普通安装需要设备端人工批准，尚未完成批准闭环；`--p12file/--password` supervision CMS silent install 已离线测试 |

## 诊断与设备管理

| 命令/能力 | 状态 | 当前范围 |
|---|---|---|
| `diagnostics list` | implemented | diagnostics relay `All`；保留 go-ios `Diagnostics/Status` envelope，iOS 26 真机逐键对照通过 |
| `batterycheck` | implemented | GasGauge；iOS 26 真机通过 |
| `batteryregistry` | implemented | `IOPMPowerSource` IORegistry；iOS 26 真机通过，证据不展开设备 serial |
| `mobilegestalt <keys> [--plist]` | passed | go-ios envelope；JSON/XML plist；iOS 26 返回 `MobileGestaltDeprecated` |
| `diskspace` | implemented | AFC total/free/used/block size；iOS 26 真机通过 |
| `reboot` | implemented | iOS 26 真机返回 restarting，约 25 秒后重新枚举并恢复 Lockdown；server-owned userspace tunnel 自动淘汰旧 adapter 并重建数据面 |
| `shutdown` | device-test-needed | destructive，尚未执行真机测试 |
| `erase --force --udid` | device-test-needed | 不可逆操作强制显式设备 + force；MCInstall Flush/cloud preflight、PreserveDataPlan、ack/断开语义已离线测试；真机禁止普通 smoke |
| `prepare cloudconfig/printskip` | implemented | iOS 26 MCInstall 返回嵌套 `CloudConfiguration.IsSupervised=false`；host `printskip` 返回 69 项 |
| `prepare create-cert` | offline-tested | 无设备生成 DER/PEM/CSR/PKCS#12；纯 Rust round-trip + 进程 smoke |
| `prepare [--skip...] [--certfile...]` | device-test-needed | activation gate、MCInstall cloud config、可选 supervision、language/time、AFC SkipSetup |
| `devmode get/reveal` | implemented | AMFI；iOS 26 `get=enabled`，`reveal` 真机成功且设备保持 enabled |
| `devmode enable` | device-test-needed | post-restart action 已接入；当前设备原本已 enabled，未触发从 disabled 到 enabled 的完整状态变化 |
| `setlocation`, `resetlocation` | implemented | iOS 17+ 复用 server-owned shared-RSD DVT，旧系统保留 classic service；iOS 26 写入/清除及 reboot 后重建 tunnel 再写入/清除均通过 |
| `setlocationgpx --gpxfilepath` | offline-tested | client 限流解析 GPX；server-owned finite task、NDJSON progress、取消 clear、显式 restart；iOS 17+ shared-RSD DVT 真机待测 |
| `lang` 读取 | implemented | paired Lockdown `com.apple.international`；iOS 26 返回当前 language/locale、58 languages、369 locales |
| `lang --setlocale/--setlang` | implemented | 写前按 SupportedLanguages/SupportedLocales 校验，locale 成功而 language 失败时回滚；iOS 26 `zh-Hans/zh_CN -> en-US/en_US -> zh-Hans/zh_CN` 真机闭环通过 |
| `timeformat 24h/12h/toggle/get [--force]` | implemented | 全局 `Uses24HourClock`，兼容 bool/integer `0/1`；iOS 26 `12h -> 24h -> 12h` 逐步读回通过 |
| `wifi --ssid/--password/--enc-type/--remove` | device-test-needed | activation gate、结构化 profile、未监督设备明确 warning |
| `httpproxy <host> <port> [user] [pass] --p12file` / `remove` | device-test-needed | global proxy silent install；固定 identifier remove |

## Developer 与 SpringBoard

| 命令/能力 | 状态 | 当前范围 |
|---|---|---|
| `image list` | implemented | iOS 26 personalized DDI 挂载前/后/卸载后真机通过 |
| `image mount/unmount`（iOS < 17） | implemented | iOS 15.8 (`19H370`) 显式卸载并挂载 开发用 Mac Xcode 15.5 `.dmg` + `.signature`；`image list` 显示 readonly `/Developer` |
| `image mount`（iOS 17+） | implemented | Restore/BuildManifest identity、TSS 和 trust cache；iOS 26 personalized mount/unmount 真机通过 |
| `image auto --basedir`（iOS 17+） | implemented | iOS 26 从 `15F31d/17E5179g/17F113` 正确选择 `17F113`；已同步 go-ios `5804fa9` |
| `image auto`（iOS < 17） | implemented | go-ios floor version table/raw GitHub source、短版/build-label cache 兼容、2 GiB/64 KiB bound、UDIF/signature 校验、临时文件清理与本地 HTTP/cache 测试；iOS 15.8/15.8.x 兼容覆盖选择 15.5，release 真机 auto mount/list/`ps --apps` 闭环通过 |
| `ps [--apps]` | implemented | iOS 26 shared RSD 多轮返回 10/14 个 application process，最近一次总进程 571；旧系统保留 Lockdown |
| `launch` | implemented | 一次性 DVT process control；iOS 26 真机启动 MobileSafari 并返回 PID；args/env/kill-existing 由 mock 覆盖 |
| `launch --wait` | implemented | iOS 26 AppService/OpenStdio 真机通过：shared RSD、PID、238-byte raw output、EOF completed、显式 generation 2 restart；stop 回收由 mock lifecycle 覆盖 |
| `kill` | implemented | shared RSD DVT；修正 `killPid:` 的 NSKeyedArchiver `NSNumber` 参数和 correlated reply 后，iOS 26 PID 从新 `ps` 消失 |
| `memlimitoff` | device-test-needed | 已接 shared RSD；PID/process/bundle resolution，写操作尚未真机验证 |
| `screenshot` 单张 | implemented | go-ios Instruments screenshot channel；iOS 17+ 复用 server-owned shared RSD、旧系统保留 Lockdown DVT；iOS 26 生成 828x1792 PNG，server 代写 client 绝对路径 |
| `get-wallpaper` | implemented | SpringBoard home preview；iOS 26 输出有效 828x1792 PNG，只保留在本地 evidence |
| `get-icon-layout` | implemented | SpringBoard format version 2；iOS 26 输出可解析 JSON，4 个顶层项/65 个 bundle entry |
| `set-icon-layout` | implemented | format version 2 JSON 写回；iOS 26 原样写回时 SpringBoard 补入 orphan app，读取规范化结果后再次写回完全幂等 |
| `set-wallpaper <image> --p12file ... [--screen]` | device-test-needed | JPEG/PNG + client-relative path 前置校验；supervision escalation；MCInstall Settings/Wallpaper 外层与 item ack 已离线测试；高风险写操作未真机执行 |
| `get-app-icon <bundleID> [--output]` | passed | Rust 扩展；classic SpringBoard `getIconPNGData`、PNG 校验、client-relative 原子输出；iOS 26 返回 128x128 图标并完成解码；go-ios 当前无 CLI 入口 |
| `rsd ls` | implemented | iOS 26 DDI 挂载后返回 74 个 service；共享 tunnel、刷新与 server restart 真机通过 |

## 测试、UI、Web 与签名

| 命令族 | 状态 | 计划归属 |
|---|---|---|
| `runwda` | implemented | iOS 26 custom runner、HTTP/MJPEG bridge、health/stop/server restart 真机通过；active tunnel 下 runtime `shared_rsd`，XCTest NDJSON capture 真机 194 条/64,055 bytes；transport log 证明未创建第二套 CoreDeviceProxy |
| `runtest` XCUITest | implemented | server-owned finite task、NDJSON event/summary、completed/failed、显式 restart；**2026-08-13 真机完整 completion 闭环**：WDA 项目 `IntegrationTests_1`（会自然结束的普通 UI 测试 runner）构建并安装后自然结束，`completed: 3 passed, 56 failed, 0 skipped`，7058 条 NDJSON 事件（case/test_plan/target 生命周期完整）；56 失败为 WDA IntegrationTests 测试代码对 iOS 26 的兼容问题（非 rust-ios） |
| `runxctest` | offline-tested | binary/XML `.xctestrun` v1/v2、多 target 串行、target app name resolution；真机普通 bundle 待测 |
| unit XCTest、test filters | device-test-needed | 受控 `idevice 0.1.65` fork 注入 `libXCTestBundleInject.dylib`、关闭 UI activate/initialize，并编码 `NSSet` + `XCTTestIdentifierSet`；mock/task/archive 测试通过；2026-08-13 真机推进：WDA `UnitTests`+`IntegrationApp` host 构建/签名/嵌入修复完成，go-ios 对照已到测试执行阶段（环境就绪），rust-ios fork 的 iOS 26 testmanagerd 会话握手（host 未连接）需 vendored DTX 层专项调试 |
| `ui download` | offline-tested | host-only streaming download、原子替换、WDA ZIP 安全解压；默认 WDA/DeviceKit URL 与参考一致 |
| `ui install` | device-test-needed | `.app/.ipa/.zip`、CMS profile/P12/UDID/bundle 校验、纯 Rust 重签名后 installation_proxy；mock process smoke 完成 |
| `ui run` | implemented | WDA/DeviceKit server-owned XCUITest runner、固定 target transport、health、forward、NDJSON log、stop/restart/restore；shared RSD 注入已通过 process smoke，iOS 26 WDA 真机通过，DeviceKit 未安装故保持离线验证 |
| `ui status/api/raw` | implemented | WDA HTTP iOS 26 真机通过；DeviceKit JSON-RPC 2.0、auto probe、HTTP 错误已离线测试 |
| `ui tap/swipe/longpress/type/button` | offline-tested | DeviceKit RPC；WDA lazy/explicit session 与 action endpoint |
| `ui screenshot/source/size/orientation/app` | offline-tested | client-relative 文件输出；binary stdout 明确拒绝 |
| `ui stream` | implemented | server-owned MJPEG/H264 HTTP capture；WDA 独立 MJPEG bridge、task output、动态 URL 跟随、stop/restart iOS 26 真机通过；DeviceKit H264 离线通过 |
| `webinspector list/launch/eval` | implemented | `ios-core 0.1.7` WebInspector；Lockdown/iOS 17+ shared RSD；**2026-08-10 修复缺少 RSDCheckin 握手**（shim service 连接后必须交换 `RSDCheckin` plist 并消费两条响应，否则 iOS 26 立即 EOF）；iOS 26 真机 list/eval/launch 闭环通过（Safari 必应/apple.com 页面、`1+1`→`2`、`document.title`、launch 导航返回标题） |
| `webinspector cdp` | implemented | 同一 task/设备 connection、Chrome discovery、Inspector bridge、mock command/response 和恢复测试；iOS 26 真机 `/json` 发现 1 个 page target，page WebSocket `Runtime.enable`/`Runtime.evaluate` 返回页面标题；stop 释放端口 |
| `webinspector js-shell` | implemented | Web/WebPage/JavaScript page 选择、可选 Safari launch/navigation、持续 Runtime.evaluate；iOS 26 真机 stdin eval NDJSON 输出与 `.exit` 清理通过；同设备与 CDP task 互斥（启动即返回 `conflict`，2026-08-10 真机验证：CDP 运行时 js-shell 立即 conflict、释放后恢复） |
| `assistivetouch/voiceover/zoom` | implemented | `com.apple.Accessibility`；iOS 11 gate、`--force`、读回校验；iOS 26 三项均完成 `false -> true -> false` 真机恢复闭环 |
| `ax audit` | implemented | Accessibility DTX；AX API v20/v21 selector、issue/rect/platform element/label、Lockdown TLS handshake-only；iOS 26 API v26 两次 completion 真机通过 |
| `resetax` | offline-tested | `deviceResetToDefaultAccessibilitySettings` 已接入；会修改用户设置，不在普通真机 smoke 中执行 |
| `ax [--font]` inspector | implemented | server-owned AX DTX session、同设备互斥、NDJSON output、stop/restart/restore；iOS 26 两代 session 各 3 个真实 focus event，cleanup 后 audit 重连通过；`--font` 会改设置，仅 offline-tested |
| `sign app [--install]` | offline-tested | host-only `.app/.ipa` 重签名；generated Mach-O/CMS/P12 与 IPA mode/repackage 闭环；`--install` 复用 backend |
| `sign certificate/provision appstoreconnect` | offline-tested | host-only ES256 JWT/JSON:API、Bundle/device ensure、RSA CSR、certificate/P12、profile 与 existing certificate reuse；原子 client-relative output、secret/state 隔离和本地 Apple API process smoke 通过，真实账号未测 |

## 新增：go-ios v1.2.1 同步（2026-08-07）

参考基线推进到 `main-Attect-dev@7c9a4dd`（v1.2.1，50 个新提交）。对照结论：

- 已对齐无需改动：TSS RestoreRequestRules 引擎与 TLS 验证（vendor `tss.rs` 已有）、
  mount/unmount 回复校验（含 pre-iOS17）、RSD service 缺失 fail-fast、Instruments PID 类型守卫、
  XCTest iOS 14-16 内存配置（vendor 已是内存构建）、AFC pull zip-slip（`remote_entry_components`
  拒绝 `..`/绝对路径，2026-07-20 回归修正时已加入）。
- 本阶段补上的加固：usbmuxd plist 16 MiB 分配上限、AFC readPacket 长度下界/一致性校验、
  NSKeyedArchiver 递归深度上限（Rust 固定栈下取 256，go-ios 为 1000）、unmount_image 对
  `UnknownCommand` 容错（旧设备 best-effort）、tss.rs 规则引擎单元测试（6 项）、删除
  `command_afc` 的 v0.1 `--app-group/--crash/--temp` 死代码 gates。
- **协议差异发现**：go-ios 的 XPC body 含 8 字节 body header（`BodyLen >= 8` 检查成立），
  rust-ios 的 XPC body 直接承载 XPCObject 编码（空字典控制帧可小于 8 字节），因此该检查
  不适用并已在真机验证中移除；长度一致性由越界检查与 `XPCObject::decode` 内部边界保证。

| 能力 | 状态 | 当前范围 |
|---|---|---|
| `pasteboard get/set <text>` | implemented | iOS 17+ `com.apple.coredevice.pasteboardservice` RemoteXPC（server-owned shared RSD）；get 返回文本/条目统计，set 覆盖 general pasteboard 且不回显内容；`{"error":{...}}` 帧作为错误返回（对齐 go-ios 1317039）；iOS 26 真机 get/set/读回/恢复闭环通过，缺 tunnel 返回结构化错误 |
| `mdm security-info` | offline-tested | MCInstall `SecurityInfo`（go-ios a778c9e）；只读 passcode 存在性/策略合规/锁屏宽限/硬件加密；要求 supervision identity 先 escalation；错误只渲染 Status/ErrorChain 摘要，绝不输出携带 token 的整个响应；2026-08-10 真机未监督设备返回结构化 `CertificateRejected`（拒绝路径通过，写路径待监督设备） |
| `mdm fetch-unlock-token` | offline-tested | MCInstall `RequestUnlockToken`；token 写入 `--output`（0600）或 `--output=-` base64 stdout；无 passcode 时 DMCKeybagErrorDomain 37002 映射为明确错误；真机未监督设备路径待 supervision 资产 |
| `mdm clear-passcode` | offline-tested | MCInstall `ClearPasscode` + `--token` 文件；错误渲染脱敏（ClearPasscode 响应会回显 token） |
| `mdm clear-screen-time-password` | offline-tested | MCInstall `ClearRestrictionsPassword`；无需 token，supervisor identity 即可 |
| `pair` 锁定设备 | implemented | 设备锁定（passcode screen）时 supervised pair 无 Error 也无 EscrowBag 的场景返回 `DeviceLocked`（go-ios `ErrDeviceLockedPairingDeferred`），exit 2，不保存 pair record；Lockdown `DeviceLocked/PasswordProtected` 同样映射 |

## 已知协议限制

- userspace tunnel 默认使用 USB `CoreDeviceProxy`；`--remote-pairing` 使用独立 RemotePairing
  identity/TCP transport。2026-08-10 已排除网络层阻塞（iPhone 与主机同网、单播可达、mDNS 环境
  正常），剩余阻塞是设备未广告 `_remoted._tcp`（iOS 26 设备侧条件，host 侧不可解）；不得以
  mock echo 代替真机通过。
- iOS 17+ DVT、`launch --wait` AppService/OpenStdio、WDA、UI runner 与 XCTest 均可复用同设备 active
  tunnel provider。WDA/XCTest 没有 active tunnel 时保留 task-owned `dedicated_core_device` fallback；
  runtime 和 info 级 transport log 会明确区分两条路径。
- capture 文件可在任务停止后直接读取，但已停止任务从 task registry 移除，不能再用
  `task output`；stop 响应中的 `output_path` 会保留。
- macOS/Linux 尚未执行宿主机进程测试，不能标记为已验证。
