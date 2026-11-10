# AGENTS.md

本仓库使用中文注释、文档和交接说明；Apple service、协议字段、CLI flag 保留官方英文名。

## 结构约束

- `rust-ios-protocol` 不得依赖其他 workspace crate 或设备库。
- 设备协议只在 `rust-ios-core`；CLI 不得直接调用 `idevice`。
- 持续资源必须归 server/task manager 所有，禁止从普通命令生成独立长期子进程。
- 所有真实设备代码必须能被 mock backend 替换；普通测试不得要求设备。
- 未完成命令必须返回明确的结构化错误，禁止伪造成功。

## 验证

Cargo gate 串行运行：format、check、clippy `-D warnings`、test、release build。
进程测试必须隔离 `RUST_IOS_HOME` 和 server port，不能操作用户实例。

## 文档脱敏

文档（docs/、README、交接说明）禁止提及开发环境的设备与主机信息，包括但不限于：
UDID、序列号、USB instance id、设备型号/板号、设备名、设备与主机的内网 IP/主机名、
本机用户名路径。统一使用占位符（如 `<udid>`、`<user>`）或通用描述（如
「测试 iPhone / iOS 26.5.2」、「开发用 Mac」）。Apple 公开 OS build 标签（如 23F84）
作为资产/版本矩阵可保留。新增真机记录时即脱敏，不事后补。
