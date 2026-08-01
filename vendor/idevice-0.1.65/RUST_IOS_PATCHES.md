# rust-ios 受控补丁

本目录以 crates.io `idevice 0.1.65` 发布包为基线。除本文件、`.gitignore`、`.gitattributes` 与
下列三个源文件外，内容应与发布包保持一致：

- `src/services/dvt/process_control.rs`
- `src/services/dvt/xctest/mod.rs`
- `src/services/dvt/xctest/types.rs`

补丁范围：

1. unit XCTest 可显式指定 `.xctest` bundle，并使用 runner app 本身作为 test host。
2. unit launch 注入 `libXCTestBundleInject.dylib`，不使用 UI activate options，且
   `initializeForUITesting=false`。
3. unit configuration 使用实际 test bundle 的 `productModuleName`。
4. `testsToRun/testsToSkip` 使用 legacy `NSSet`，并同时生成 modern
   `XCTTestIdentifierSet`，替代会被 iOS 26 忽略的 `NSArray<String>`。
5. `killPid:` 使用 NSKeyedArchiver `NSNumber` 参数和 correlated reply；与 go-ios
   `MethodCall` 一致，避免发送裸 `U32`/`expect_reply=false` 后在设备未终止进程时伪报成功。

workspace 根 `Cargo.toml` 通过 `[patch.crates-io]` 固定使用本目录。定向验证：

```powershell
cargo test --manifest-path C:\Users/<user>\RustroverProjects\rust_ios\vendor\idevice-0.1.65\Cargo.toml `
  --no-default-features --features xctest,ring services::dvt::xctest::
```

该命令会在本目录生成独立 `Cargo.lock` 和 `target/`；二者都是本地测试产物，不进入提交。
