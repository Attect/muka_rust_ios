# IPC Protocol 1.20

## Transport 与 Frame

默认 transport 是 `127.0.0.1:60106` TCP。每条消息是一个独立 frame：

```text
0               4                                      4 + N
+---------------+------------------------------------------+
| N: u32 big-endian | N bytes UTF-8 JSON                  |
+---------------+------------------------------------------+
```

`N` 必须为 1 到 16 MiB。codec 在分配前拒绝 0、超限、截断和非法 JSON。消息使用 serde 的
tagged enum，未知 required variant 会成为 protocol/frame error。

## Hello

client 的第一帧只能是：

```json
{
  "type": "hello",
  "payload": {
    "protocol_major": 1,
    "protocol_minor": 20,
    "client_version": "0.1.0",
    "client_pid": 1234,
    "auth_token": "64 lowercase hex chars"
  }
}
```

server 验证 loopback peer、token 和 major 后返回 `ServerHello`，包括 PID、instance UUID 和
capabilities。major 必须完全相同；minor 在同一 major 内向后兼容。当前 capabilities：

```text
server_v1, device_list, pair_record, persistent_tasks,
persistent_forward, persistent_tunnel, windows_kernel_tunnel, persistent_dproxy,
remote_pairing_tcp, persistent_streams, ostrace_filters,
persistent_accessibility_inspector, persistent_ui_stream,
persistent_ui_runner,
persistent_device_state, persistent_webinspector_cdp, interactive_webinspector_js_shell,
debug_app_session, wda_event_capture, xctest_unit_filters, shared_xctest_rsd,
finite_xctest, finite_app_process,
finite_location_simulation, task_output
```

认证失败和版本不兼容使用 `Fatal`，随后关闭连接。认证失败绝不能触发 client 自动启动新实例。

## Request/Response

每个 request 包含随机 UUID：

```json
{
  "type": "request",
  "payload": {
    "request_id": "...",
    "request": {
      "command": "server_status"
    }
  }
}
```

response 必须回显相同 ID。一个连接可串行复用多个 request；client 拒绝 ID 不匹配和第二个
hello。当前 request 家族：

| 类别 | Request |
|---|---|
| 生命周期 | `ping`, `server_status`, `shutdown` |
| 设备 | `list_devices`, `listen_snapshot`, `wait_device_change`, `read_buid`, `read_pair_record` |
| Task | `task_list`, `task_show`, `task_stop`, `task_restart`, `task_output` |
| 创建 | `forward_start`, `tunnel_start`, `tunnel_refresh`, `stream_start`, `accessibility_inspector_start`, `location_simulation_start`, `screenshot_stream_start`, `ui_stream_start`, `ui_runner_start`, `web_inspector_cdp_start`, `web_inspector_js_shell_start`, `debug_proxy_start`, `dproxy_start`, `wda_start`, `xctest_start`, `app_launch_start` |
| 一次性控制 | `tunnel_pair`, `web_inspector_js_shell_eval` |
| 一次性命令 | `execute` |

`execute` 传递 argv、client cwd、UDID 和输出选项，但不传 shell command line；server 不执行
shell。文件参数由 core 使用 client cwd 解析。

## Error

错误 body 示例：

```json
{
  "status": "error",
  "payload": {
    "code": "device_not_found",
    "message": "device not found: ...",
    "retryable": true,
    "details": null
  }
}
```

稳定 code：

```text
authentication_failed, protocol_mismatch, invalid_request, server_unavailable,
already_running, not_found, conflict, device_not_found, device_unavailable,
usbmuxd_unavailable, pairing_required, permission_denied, unsupported_platform,
not_implemented, timeout, task_failed, internal
```

`retryable` 描述 server/supervisor 是否可在外部条件改变后重试，不表示 client 应无上限重发。

## Task Spec

`TaskSpec` 为 tagged union：

- `forward`：UDID、bind host/port、device port、connection preference。
- `tunnel`：UDID、mode、`usb_core_device`/`remote_pairing_tcp` transport、pair record path、userspace bind/port、可选 kernel interface name 与绝对 Wintun path。
- `stream`：kind、UDID、绝对 output path、format、PID/process filter；`os_trace` kind 可附带 typed
  level、subsystem/message include/exclude 和 follow options，其他 kind 必须拒绝该 options。
- `accessibility_inspector`：UDID、绝对 NDJSON output path、可选 font size；font 会改变 `DYNAMIC_TYPE`。
- `location_simulation`：UDID、client-resolved GPX source path、标准化 track point/delay 和绝对 NDJSON output path；坐标属于敏感数据，recording backend 会脱敏。
- `screenshot_stream`：UDID、MJPEG bind host/port、frame interval。
- `ui_stream`：WDA/DeviceKit/auto driver、MJPEG/H264、backend URL、可选 WDA task ID、绝对 output path 与 query 参数。
- `ui_runner`：WDA/DeviceKit target、runner/target bundle、XCTest config、固定 device port/health path、host endpoint、readiness timeout 与绝对 NDJSON output path。
- `web_inspector_cdp`：UDID、HTTP/WebSocket bind host/port、startup timeout；同一 task 只持有一个设备 WebInspector connection。
- `web_inspector_js_shell`：UDID、可选 URL/bundle ID/open Safari、每条命令 timeout 和绝对 NDJSON output path；input channel 仅存在于 server runtime，不写入 state。
- `debug_proxy`：UDID、GDB remote bind host/port 与 `client_owned`；运行时依赖同设备 tunnel。
- `dproxy`：bind host/port、数值 IP upstream、`all/usbmuxd/utun` mode、binary-only、可选 interface
  和绝对 master NDJSON output path。protocol 保留 `utun` typed variant，但 Windows server 明确返回
  `not_implemented`；listener 与 upstream 重叠会返回 `invalid_argument`。
- `wda`：UDID、runner/target bundle、XCTest config、args/env 与 readiness timeout。
- `xctest`：UDID、一个或多个 target、unit/UI 标记、`tests_to_run/tests_to_skip`、绝对 NDJSON
  output path、可选 `.xctestrun` 来源与 timeout。使用 unit/filter 的 client 要求 server 发布
  `xctest_unit_filters`，避免旧 server 静默忽略新语义。
- `app_launch`：UDID、bundle ID、args/env、`kill_existing` 与绝对 raw output path。
- `device_state`：UDID、condition identifier、profile identifier。
- `generic`：为后续 task kind 预留；当前 runner 会明确失败。

`tunnel_pair` 是一次性 request，不是 `TaskSpec`。它要求显式 `manual=true`，只返回 UDID、identity
路径、设备地址和是否新配对；PIN 不属于 wire schema，也不会写入 `state-v1.json`。默认 identity
目录由 server 解析为 `<server-home>/remote-pairing`，显式相对路径必须由 CLI client 先转为绝对路径。

Task response 同时包含持久 spec 和瞬时 runtime。runtime 不是 API schema 的持久承诺，只用于
诊断，如 `boundAddress`、`userspaceEndpoint`、`bytesWritten`。

普通 debug proxy 的 `client_owned=false`，按 persistent task 恢复。`debug <app_path>` 使用
`client_owned=true`：task 不进入 state，创建它的 IPC connection 是 lease owner；连接断开时 server
自动 stop。字段缺失按 `false` 反序列化，兼容 protocol 1.14 state/client。

`web_inspector_js_shell_eval` 只接受 shell task UUID 和单条 expression，最大 1 MiB。server 串行执行
同一 task 的输入，并把 result/error 追加到 NDJSON capture。该 task 使用 `restartPolicy=never`：显式
`task restart` 可建立新 session，但 server interruption 只标记 failed，绝不重放已经提交的 JavaScript。

## Capture Output

`task_output` 请求：

```json
{
  "command": "task_output",
  "arguments": {
    "task_id": "...",
    "offset": 0,
    "limit": 65536
  }
}
```

响应值：

```json
{
  "task_id": "...",
  "offset": 0,
  "next_offset": 128,
  "total_bytes": 128,
  "eof": true,
  "content_type": "application/x-ndjson",
  "bytes_base64": "..."
}
```

server 只读取正在注册的 device/UI stream、UI runner、dproxy、XCTest、app process task 自身 output
path。单次限制 1 MiB；frame 上限仍是最终防线。UI runner/XCTest output 为逐行 `XCTestEvent`，
suite/case/failure/activity/log 均保留 target index/name；dproxy output 是 connection lifecycle/stats
NDJSON，不包含每方向 raw dump；app process output 是 OpenStdio 合并后的 stdout/stderr raw bytes。

## Device Change 长轮询

`wait_device_change` 携带上一份 `DeviceList` 和 1 到 30,000 ms timeout。server 先订阅 registry
watch，再比较当前 snapshot，避免“比较后、订阅前”的竞态。变化时立即返回；超时返回当前
snapshot。CLI 默认 2 秒，只有 snapshot 不同时才打印。

## Event Envelope

protocol 已保留 `ServerMessage::Event`、`DeviceSnapshot/TaskChanged/Output` 类型，但 0.1 client
尚未开启 multiplex subscription；当前持续输出使用 task file + offset，设备变化使用长轮询。
未来启用 event 时不会改变 framing 或 request ID 规则。

## 兼容性规则

- 新增 optional 字段必须带 serde default。
- 删除/改名 request、response、error code 或已有字段需要提高 major。
- 新增 request/capability 可提高 minor。
- state schema 与 wire protocol 独立；state migration 不通过 IPC major 表示。
- protocol crate 不允许依赖 core/server/cli 或 `idevice`。
