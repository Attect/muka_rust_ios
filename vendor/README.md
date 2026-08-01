# Vendored dependencies

`ios-core-0.1.7` comes from the crates.io `ios-core` 0.1.7 package. rust-ios
keeps a narrow local patch because the published crate has two FileService
integration defects and lacks the go-ios-compatible Accessibility setting update:

- the `fileservice` feature does not expose the XPC client/framing modules it
  needs unless the unrelated native-OpenSSL `tunnel` feature is enabled;
- FileService operations after `CreateSession` on `ServerClientStream` need
  go-ios-compatible fixed request ID 1 and next-message sequencing rather than
  incrementing request-ID correlation.
- Accessibility inspector `--font` needs
  `deviceUpdateAccessibilitySetting:withValue:` with the AXAuditDeviceSetting
  archive used by go-ios.

The patch is limited to feature gates in `src/lib.rs`, `src/xpc/mod.rs` and
`src/xpc/rsd.rs`, the explicit XPC sequencing API in `src/xpc/client.rs`, and
its FileService call sites in `src/services/fileservice/mod.rs`. The
Accessibility addition is isolated to
`src/services/accessibility_audit/mod.rs`.
