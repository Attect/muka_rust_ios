# rust-ios 受控补丁

本目录以 crates.io `idevice 0.1.65` 发布包为基线。除本文件、`.gitignore`、`.gitattributes`、
`Cargo.toml` 与下列四个源文件外，内容应与发布包保持一致：

- `src/services/dvt/process_control.rs`
- `src/services/dvt/xctest/mod.rs`
- `src/services/dvt/xctest/types.rs`
- `src/tss.rs`
- `src/services/mobile_image_mounter.rs`
- `src/services/syslog_relay.rs`
- `src/xpc/format.rs`
- `src/xpc/mod.rs`
- `src/lib.rs`（`read_plist_value` 长度上限）
- `src/services/dvt/message.rs`
- `src/services/installation_proxy.rs`
- `src/services/rsd.rs`

补丁范围：

1. unit XCTest 可显式指定 `.xctest` bundle，并使用 runner app 本身作为 test host。
2. unit launch 注入 `libXCTestBundleInject.dylib`，不使用 UI activate options，且
   `initializeForUITesting=false`。
3. unit configuration 使用实际 test bundle 的 `productModuleName`。
4. `testsToRun/testsToSkip` 使用 legacy `NSSet`，并同时生成 modern
   `XCTTestIdentifierSet`，替代会被 iOS 26 忽略的 `NSArray<String>`。
5. `killPid:` 使用 NSKeyedArchiver `NSNumber` 参数和 correlated reply；与 go-ios
   `MethodCall` 一致，避免发送裸 `U32`/`expect_reply=false` 后在设备未终止进程时伪报成功。
6. TSS 请求改走 `https://gs.apple.com`（同步 go-ios `ec90af1`）：gs.apple.com 链到
   Apple 私有 CA "Apple Server Authentication CA"，Windows/精简 Linux 信任库缺失
   该根证书时完整链验证会以 unknown authority 失败并阻断冷 DDI 挂载。改为跳过
   链验证、仅对叶子证书做主机名（gs.apple.com）与有效期 sanity check，握手签名
   仍按 provider 算法验证。`tss` feature 相应拉起 `rustls/ring`、`rustls-webpki`
   与 `reqwest/rustls-no-provider`。真实端点握手以 `#[ignore]` 测试
   `tss::tests::tls_handshake_with_sanity_check_reaches_real_endpoint` 覆盖，
   需要网络时用 `-- --ignored` 手动运行。
7. image mounter 拒绝响应解析（同步 go-ios `65bbc9f`）：`MountImage`/`UnmountImage`
   响应带 `Error` 键时按设备拒绝处理并在错误信息中保留 `DetailedError`；其中
   `ImageMountFailed` + detail 含 `already mounted`（iOS 27.2 真机样本）视为挂载
   成功——个性化 DDI 驻留 `/System/Developer` 而 image mounter 注册表脱节时，
   `LookupImage` 返回空签名，该拒绝是镜像存在的唯一可靠信号，使挂载幂等。
   `UnmountImage` 的 `Error: UnknownCommand`（旧设备）与 `Status: UnknownCommand`
   两种形状均按 no-op 处理。离线单测
   `services::mobile_image_mounter::refusal_tests` 覆盖真机抓包样本。
8. syslog relay BSD vis(3) 转义解码（同步 go-ios `a5bbcbe` + `1cc5c28`）：`next()`
   返回前解码 `\M-x`/`\M^X`/受控八进制/`\\`，CJK 等多字节 UTF-8 日志从
   `\M-f\M^H\M^V` 形态恢复为可读文本。安全策略与 go-ios 一致：控制值八进制、
   `\^X`、未知/截断序列一律原样保留，解码绝不重新引入裸控制字节；孤立高字节
   解码后按 `from_utf8_lossy` 语义替换。离线单测 `services::syslog_relay::vis_tests`
   覆盖 meta/八进制/控制值/截断四类形态。
9. 不可信 wire 长度的防御性上限（同步 go-ios hardening 精神，rust-ios 受控补丁）：
   - `xpc::format`：XPC `body_len` 先做 128 MiB 上限与 `checked_add` 溢出防护，
     再驱动切片；`xpc::RemoteXpcClient` 的跨调用 partial 缓冲超过单消息上限时
     清空并报错，阻断无界内存累积。
   - `Idevice::read_plist_value`：lockdown plist 4 字节长度前缀（不可信，最高
     4 GiB）加 128 MiB 上限后再分配。
   - `dvt::Message::from_reader`：分片计数 0/超 1024、累计体积超 128 MiB 直接
     拒绝（与 ios-core dtx 同级），防止 `vec![0; length]` 被损坏流放大。
   - `installation_proxy::browse`：同步 go-ios `de44995`（issue-818），无
     Complete 的 chunk 上限 1000、条目上限 1,000,000（go-ios 实测无界累积
     ~2.1GB live heap）。
10. RSD host announce 重构（同步 go-ios `6910f3b`/`51887da`）：`send_device_handshake`
    （固定值、先于设备握手发送、无法回显且重试不安全）替换为
    `send_host_handshake(protocol_version, version_flags)`——`RsdHandshake::new`
    先接收并解析设备握手（Services/MessagingProtocolVersion/UUID/Properties），
    全部解析成功后 announce 恰一次：回显设备的 MessagingProtocolVersion 与
    `RemoteXPCVersionFlags`（缺失时回退旧固定值），携带新的 `BootSessionUUID`，
    移除 `SensitivePropertiesVisible`。解析失败即未 announce，握手可安全重试。
    iOS 27.2 起每隧道仅接受一次 host announce（重复 announce 使服务迁新端口且
    旧端口立即失效），大 data 回复仅对已 announce 的 peer 可用。真机回归待做。
11. remote-XPC 引用式文件流发送端（同步 go-ios `273d3e0`）：`XPCObject::FileTransfer`
    编解码本就存在，补上传输层——`RemoteXpcClient::open_file_transfer`（新 HTTP/2
    流上发 `AlwaysSet|0x00100000` FileOpen 消息）、`wait_file_transfer_accepted`
    （等待 `0x00200000` FileOpenReply 且 message id 匹配）与 `FileTransferWriter`
    （载荷裸写 DATA 帧，设备按引用中的 TransferSize 精确读取，无需半关闭）。
    离线单测 `xpc::file_transfer_tests` 覆盖 FileOpen/FileOpenReply/FileTransfer
    引用编解码。cryptexd install 的五路载荷（image/volumehash/info/trustcache/
    im4m）依赖此能力。

workspace 根 `Cargo.toml` 通过 `[patch.crates-io]` 固定使用本目录。定向验证：

```powershell
cargo test --manifest-path C:\Users/<user>\RustroverProjects\rust_ios\vendor\idevice-0.1.65\Cargo.toml `
  --no-default-features --features xctest,ring services::dvt::xctest::

cargo test --manifest-path C:\Users/<user>\RustroverProjects\rust_ios\vendor\idevice-0.1.65\Cargo.toml `
  --no-default-features --features tss,ring tss::
```

该命令会在本目录生成独立 `Cargo.lock` 和 `target/`；二者都是本地测试产物，不进入提交。
