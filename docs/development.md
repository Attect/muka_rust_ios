# 开发与扩展指南

## 仓库约束

- 注释、文档和交接说明使用中文；Apple service、protocol field、CLI flag 保留英文。
- `rust-ios-protocol` 不依赖其他 workspace crate 或设备库。
- 所有 Apple service 只在 `rust-ios-core`；CLI 不得直接依赖 `idevice`。
- 持续资源只能由 `TaskManager`/server 拥有。
- 真实设备路径必须有 mock/replay 替代，普通测试不要求设备。
- 未完成命令或 flag 返回结构化错误，禁止忽略后伪造成功。

## 添加一次性命令

1. 从参考 `internal/clihelp/help.yaml` 固定 Usage 和 flag 语义。
2. 若需要 typed CLI 行为，在 `TopCommand` 添加；其余 legacy 命令通过 `External` 形成
   `CommandInvocation`。
3. 在 `DeviceBackend::execute` 的真实实现中连接 service；路径使用 invocation cwd 解析。
4. mock 返回确定性最小结果；replay fixture 添加至少一个真实形状响应。
5. 对未支持的子命令/flag 在执行前返回 `CoreError::NotImplemented`。
6. 更新 command help、功能矩阵和真机清单。

不要在 CLI 中连接 Lockdown，也不要从 server dispatch 直接调用 `idevice`。如果第三方 API 不稳定，
先在 core 内包装成项目自己的结构。

## 添加持续任务

1. 在 protocol 定义明确的 `TaskSpec`；敏感/live 对象不能序列化。
2. 在 `TaskKind`、selector、CLI start/list/stop 中建立稳定入口。
3. 在 core 定义最小 trait，如 `DeviceStream`，并实现 real/mock/replay/recording。
4. `TaskManager::start_*` 先验证参数和冲突，再持久化 desired state。
5. supervisor 必须响应 `CancellationToken`，区分 waiting/backoff/failed，并有重试上限策略。
6. server restart 从 spec 重建对象；测试同一 task ID 和数据面恢复。
7. 输出使用 task file、受控 socket 或 protocol event；不能把 live handle 写入 state。
8. 增加 stop 释放、强杀恢复、端口/路径冲突和并发进程测试。

## Backend

### Mock

用于生命周期和错误注入。流应节流并确定性产出，避免测试 busy loop。mock 不应“实现所有未知命令”；
未知 argv 必须返回 `NotImplemented`。

### Replay

fixture 以最长 argv 前缀匹配。适合输出兼容回归，不应模拟未知命令。fixture 只放脱敏字段。

### Recording

包装任意 backend，JSONL 记录 operation/request/outcome。当前不录制数据面；PairRecord 无条件只记录
`success/redacted`。`--password/--p12password`、global proxy 位置密码和 `launch --env` value 会在 argv
中替换为 `<redacted>`。新增含 token、证书、私钥或 provisioning data 的 API 时必须先定义 redaction。

### Supervision

`supervision` 模块使用纯 Rust `p12-keystore`、`rsa`、`sha1/sha2`：PKCS#12 只在 host 解析，private
key 不发送给设备；MCInstall 仅接收 certificate 和 CMS SignedData。不得把 password、private key、
challenge 或 SignedData 放进 log/recording。生成 identity 后至少验证 P12 round-trip 和 CMS DER 外层。

### Idevice

版本固定为 `=0.1.65`、`full + ring`。升级步骤：

1. 阅读 crate changelog/API diff。
2. `cargo update -p idevice --precise <version>`。
3. 编译所有 features，确认 core wrapper 吸收变化。
4. 跑 mock/replay/process gate。
5. 按 service 分组执行真机只读测试，再执行授权写操作。

## Protocol 演进

- compatible request/optional field 提高 minor，当前为 1.6。
- 删除/改名 wire variant 或改变语义提高 major。
- 每个新增 request 加 serde round-trip 和 socket dispatch 测试。
- frame 上限、token 和 request ID 规则不可绕过。
- capabilities 应反映 server 实际支持，不作为“某个设备一定支持”的保证。

## State 演进

`state-v1.json` schema 与 IPC 分开。新增有 default 的 spec variant 可以保留 v1；无法向后读取时：

1. 定义 vN 结构与显式迁移函数。
2. 先读取旧 state，验证并写新文件。
3. 保留原文件备份，不在原路径原地半写。
4. 增加旧 fixture、损坏迁移和中断测试。

## Windows 调试

构建前若 `ios.exe` 被锁，先读 metadata 并确认 executable/PID 属于当前 workspace，再优雅
`kill-server`。不要盲目 `Stop-Process -Name ios`，系统可能有其他 home/版本实例。

```powershell
$env:RUST_IOS_LOG = 'rust_ios_server=debug,rust_ios_core=debug'
$env:RUST_BACKTRACE = '1'
target\debug\ios.exe server nodaemon `
  --server-home "$env:TEMP\rust-ios-debug" `
  --server-addr 127.0.0.1:60250
```

## 验证顺序

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
rustup run stable cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo build --workspace --release
node scripts/smoke-tasks.mjs target/debug/ios.exe
node scripts/smoke-resilience.mjs target/debug/ios.exe
node scripts/smoke-mcinstall.mjs target/debug/ios.exe
node scripts/compat-cli.mjs target/debug/ios.exe C:/Users/<user>/bin/ios.exe
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/smoke-tasks.ps1
```

Cargo gate 串行；四个 Node 脚本也分别运行，其中 task/resilience/mcinstall 会启动真实子进程。脚本必须保留 `finally`
清理，并验证递归删除目标位于 `%TEMP%`。

## 网络代理

需要下载依赖/Developer Image 时可使用用户提供的本机代理：

```powershell
$env:HTTP_PROXY = 'http://127.0.0.1:10800'
$env:HTTPS_PROXY = 'http://127.0.0.1:10800'
$env:ALL_PROXY = 'socks5://127.0.0.1:10800'
```

不要把代理地址硬编码进二进制或 fixture。
