//! cryptexd.remote 开发者磁盘挂载器（同步 go-ios `ca2c56d`/`14f8098`/`65bbc9f`）。
//!
//! 近期设备（iPhone 17+ / 新 iOS）通过 `com.apple.security.cryptexd.remote` 以
//! cryptex 形式安装开发者磁盘，而不是 `mobile_image_mounter`。镜像以 Apple TSS
//! 的 `Cryptex1,Ticket` 个性化，票据绑定设备在 cryptex nonce 域生成的 nonce。
//!
//! 协议要点（与 go-ios `ios/imagemounter/cryptex.go` 逐字段对齐）：
//! - XPC 消息形如 `{"routine": <name>, "argv": {...}}`，flags 恒为
//!   `AlwaysSet|Data|HeartbeatRequest(0x10000)`，message id 恒为 1；
//! - 应答在 serverClient 流上（本 crate 的 reply 通道），错误以 `cferr`
//!   （序列化 CFError）或 `error`（errno）报告；
//! - install 的五个载荷（image/volumehash/info/trustcache/im4m）各自独立
//!   HTTP/2 流，消息体只携带 `FileTransfer` 引用，载荷裸传；
//! - img4 nonce 为 56 字节序列化 `img4_nonce_t`：u16 version + 48B nonce +
//!   padding + u32 LE 长度（offset 52）。

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha384};
use tokio::io::AsyncReadExt;

use crate::backend::UserSpaceTunnel;
use crate::error::{CoreError, CoreResult};
use crate::real::{IdeviceSharedRsdSocket, map_device_error};

/// cryptexd 的 RemoteXPC 服务名；只有 RSD 服务表里出现的设备才支持 cryptex DDI。
pub const CRYPTEX_SERVICE_NAME: &str = "com.apple.security.cryptexd.remote";

/// cryptex_info 中开发者磁盘的 CFBundleIdentifier，cryptexd 以
/// `remote-cryptex-identifier` 上报它。
const DDI_CRYPTEX_IDENTIFIER: &str = "com.apple.MobileAsset.DDI";

/// CoreDevice 安装开发者磁盘时使用的常量（go-ios cryptex.go 原值）。
const CRYPTEX_CLIENT_VERSION: u64 = 3;
const CRYPTEX_IMAGE_TYPE_INDEX: i64 = 10;
const CRYPTEX_PERSISTENCE: u64 = 2;
const CRYPTEX_NONCE_PERSISTENCE: u64 = 1;

/// Cryptex1 build identity 中需要上传的 manifest 条目键。
const CRYPTEX_DMG_KEY: &str = "Cryptex1,GenericDmg";
const CRYPTEX_TRUST_CACHE_KEY: &str = "Cryptex1,GenericTrustCache";
const CRYPTEX_VOLUME_KEY: &str = "Cryptex1,GenericVolume";
const CRYPTEX_INFO_PLIST_KEY: &str = "Cryptex1,CryptexInfoPlist";

/// TSS 请求中固定携带的版本串（CoreDevice/CryptexKitHost 同款）。
const CRYPTEX_TSS_VERSION_INFO: &str = "libauthinstall-1104.0.9";

/// install 载荷的分块读取大小（DDI 镜像为多 GiB 文件，必须流式上传）。
const UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;

/// cryptexd 通过 remote 服务安装的 cryptex 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCryptex {
    pub identifier: String,
    pub version: String,
}

/// `read-personalization-id` 报告的 img4 chip 属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptexChip {
    pub chip: u64,
    pub ecid: u64,
    pub fchp: u64,
    pub typ: u64,
    pub clas: u64,
    pub production: bool,
    pub secure: bool,
}

/// BuildManifest 中选出的 Cryptex1 build identity。
#[derive(Debug, Clone)]
pub struct CryptexIdentity {
    /// `Cryptex1,` manifest 条目：键名 →（Info.Path、Personalize、Digest）。
    pub entries: BTreeMap<String, CryptexManifestEntry>,
    pub chip_id: u64,
    pub typ: u64,
    pub clas: u64,
    pub use_product_class: bool,
    pub sub_type: u64,
    pub nonce_domain: u64,
    pub version: String,
    pub preauthorization_version: plist::Value,
    /// `Cryptex1,CryptexInfoPlist` 条目的 Info.Path，用于读取 cryptex_info。
    pub info_plist_path: Option<String>,
}

/// BuildManifest identity 的单个 manifest 条目。
#[derive(Debug, Clone)]
pub struct CryptexManifestEntry {
    pub path: Option<String>,
    pub personalize: bool,
    pub digest: Vec<u8>,
}

type CryptexClient = idevice::xpc::RemoteXpcClient<IdeviceSharedRsdSocket>;

/// 开发者磁盘安装为 cryptex 后将获得的 identifier/version（取自 cryptex_info，
/// 与 BuildManifest 的 `Cryptex1,Version` 不同——cryptexd 上报的是 info 里的值；
/// go-ios `CryptexImageVersion`）。
pub fn cryptex_image_version(
    build_manifest: &plist::Value,
    restore_dir: &Path,
) -> CoreResult<RemoteCryptex> {
    for identity in build_manifest_identities(build_manifest)? {
        let Ok(parsed) = parse_cryptex_identity(&identity) else {
            continue;
        };
        let Some(info_path) = parsed.info_plist_path else {
            continue;
        };
        let file_path = restore_dir.join(&info_path);
        let bytes = std::fs::read(&file_path).map_err(|error| {
            CoreError::InvalidArgument(format!(
                "failed to read cryptex_info {}: {error}",
                file_path.display()
            ))
        })?;
        let info: plist::Value = plist::from_bytes(&bytes).map_err(|error| {
            CoreError::InvalidArgument(format!(
                "failed to decode cryptex_info {}: {error}",
                info_path
            ))
        })?;
        let dict = info.as_dictionary().ok_or_else(|| {
            CoreError::InvalidArgument(format!("cryptex_info {info_path} is not a dictionary"))
        })?;
        let identifier = string_value(dict, "CFBundleIdentifier")
            .ok_or_else(|| {
                CoreError::InvalidArgument(format!(
                    "cryptex_info {info_path} is missing CFBundleIdentifier"
                ))
            })?
            .to_owned();
        let version = string_value(dict, "CFBundleVersion")
            .ok_or_else(|| {
                CoreError::InvalidArgument(format!(
                    "cryptex_info {info_path} is missing CFBundleVersion"
                ))
            })?
            .to_owned();
        return Ok(RemoteCryptex {
            identifier,
            version,
        });
    }
    Err(CoreError::InvalidArgument(
        "the build manifest has no Cryptex1 identity with a Cryptex1,CryptexInfoPlist entry".into(),
    ))
}

/// 从 BuildManifest 中选出与设备 chip 匹配的 Cryptex1 identity（go-ios
/// `findCryptexIdentity`）：`Cryptex1,ChipID == fchp && Cryptex1,Type == typ`，
/// 且 identity 声明 `Cryptex1,UseProductClass` 时还要求 ProductClass 匹配。
pub fn find_cryptex_identity(
    build_manifest: &plist::Value,
    chip: &CryptexChip,
) -> CoreResult<CryptexIdentity> {
    for identity in build_manifest_identities(build_manifest)? {
        let Ok(parsed) = parse_cryptex_identity(&identity) else {
            continue;
        };
        if parsed.chip_id != chip.fchp || parsed.typ != chip.typ {
            continue;
        }
        if parsed.use_product_class && parsed.clas != chip.clas {
            continue;
        }
        return Ok(parsed);
    }
    Err(CoreError::InvalidArgument(format!(
        "no Cryptex1 identity for chip 0x{:x}, type 0x{:x} and product class 0x{:x}; is this a recent developer disk image?",
        chip.fchp, chip.typ, chip.clas
    )))
}

fn build_manifest_identities(build_manifest: &plist::Value) -> CoreResult<Vec<plist::Dictionary>> {
    let dict = build_manifest
        .as_dictionary()
        .ok_or_else(|| CoreError::InvalidArgument("BuildManifest is not a dictionary".into()))?;
    let identities = dict
        .get("BuildIdentities")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            CoreError::InvalidArgument("BuildManifest is missing BuildIdentities".into())
        })?;
    Ok(identities
        .iter()
        .filter_map(|v| v.as_dictionary().cloned())
        .collect())
}

fn string_value<'a>(dict: &'a plist::Dictionary, key: &str) -> Option<&'a str> {
    dict.get(key).and_then(|v| v.as_string())
}

/// 解析单个 build identity 的 Cryptex1 字段（go-ios `buildIdentity` 的 Cryptex1
/// 部分；数值可能是整数或十六进制字符串）。缺少 `Cryptex1,ChipID`/`Cryptex1,Type`
/// 的 identity（普通 Ap 风格 identity）会解析失败，调用方按“非 Cryptex1 identity”
/// 跳过。
fn parse_cryptex_identity(identity: &plist::Dictionary) -> CoreResult<CryptexIdentity> {
    let get_u64 = |key: &str| -> Option<u64> { identity.get(key).and_then(to_uint64) };
    let chip_id = get_u64("Cryptex1,ChipID")
        .ok_or_else(|| CoreError::InvalidArgument("identity is missing Cryptex1,ChipID".into()))?;
    let typ = get_u64("Cryptex1,Type")
        .ok_or_else(|| CoreError::InvalidArgument("identity is missing Cryptex1,Type".into()))?;
    let manifest = identity.get("Manifest").and_then(|v| v.as_dictionary());
    let mut entries = BTreeMap::new();
    let mut info_plist_path = None;
    if let Some(manifest) = manifest {
        for (key, value) in manifest {
            if !key.starts_with("Cryptex1,") {
                continue;
            }
            let entry = value.as_dictionary();
            let path = entry
                .and_then(|e| e.get("Info"))
                .and_then(|i| i.as_dictionary())
                .and_then(|i| i.get("Path"))
                .and_then(|p| p.as_string())
                .map(str::to_owned);
            let personalize = entry
                .and_then(|e| e.get("Info"))
                .and_then(|i| i.as_dictionary())
                .and_then(|i| i.get("Personalize"))
                .and_then(|p| p.as_boolean())
                .unwrap_or(false);
            let digest = entry
                .and_then(|e| e.get("Digest"))
                .and_then(|d| d.as_data())
                .map(|d| d.to_vec())
                .unwrap_or_default();
            if key == CRYPTEX_INFO_PLIST_KEY {
                info_plist_path = path.clone();
            }
            entries.insert(
                key.clone(),
                CryptexManifestEntry {
                    path,
                    personalize,
                    digest,
                },
            );
        }
    }
    Ok(CryptexIdentity {
        entries,
        chip_id,
        typ,
        clas: get_u64("Cryptex1,ProductClass").unwrap_or(0),
        use_product_class: identity
            .get("Cryptex1,UseProductClass")
            .and_then(|v| v.as_boolean())
            .unwrap_or(false),
        sub_type: get_u64("Cryptex1,SubType").unwrap_or(0),
        nonce_domain: get_u64("Cryptex1,NonceDomain").unwrap_or(0),
        version: identity
            .get("Cryptex1,Version")
            .and_then(|v| v.as_string())
            .unwrap_or_default()
            .to_owned(),
        preauthorization_version: identity
            .get("Cryptex1,PreauthorizationVersion")
            .cloned()
            .unwrap_or(plist::Value::String(String::new())),
        info_plist_path,
    })
}

/// manifest 数值的宽松解析：整数或十六进制字符串（`0x` 前缀），对齐 go-ios
/// `toUint64`（`strconv.ParseUint(n, 0, 64)`）。
pub fn to_uint64(value: &plist::Value) -> Option<u64> {
    match value {
        plist::Value::Integer(integer) => {
            let signed = integer.as_signed()?;
            u64::try_from(signed).ok()
        }
        plist::Value::String(text) => {
            let text = text.trim();
            if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).ok()
            } else {
                text.parse::<u64>().ok()
            }
        }
        _ => None,
    }
}

/// 计算 identity 中所有 `Cryptex1,` 且 `Info.Personalize=true` 条目的 SHA-384
/// （与 manifest Digest 比对；设备会校验收到的文件。go-ios `cryptexDigests`）。
pub fn cryptex_digests(
    identity: &CryptexIdentity,
    restore_dir: &Path,
) -> CoreResult<BTreeMap<String, Vec<u8>>> {
    let mut digests = BTreeMap::new();
    for (key, entry) in &identity.entries {
        if !entry.personalize {
            continue;
        }
        let Some(path) = &entry.path else {
            return Err(CoreError::InvalidArgument(format!(
                "manifest entry {key} has no Info.Path"
            )));
        };
        let file_path = restore_dir.join(path);
        let bytes = std::fs::read(&file_path).map_err(|error| {
            CoreError::InvalidArgument(format!(
                "failed to read manifest entry file {}: {error}",
                file_path.display()
            ))
        })?;
        let digest = Sha384::digest(&bytes).to_vec();
        if !entry.digest.is_empty() && entry.digest != digest {
            return Err(CoreError::InvalidArgument(format!(
                "digest of {path} does not match the build manifest"
            )));
        }
        digests.insert(key.clone(), digest);
    }
    if digests.is_empty() {
        return Err(CoreError::InvalidArgument(
            "the Cryptex1 identity has no entries to personalize".into(),
        ));
    }
    Ok(digests)
}

/// 构造 `Cryptex1,Ticket` 个性化请求（go-ios `cryptexTSSRequest`）：与
/// ApImg4Ticket 不同，它不带 ApECID/ApChipID/ApBoardID（Apple TSS 会拒收），
/// 设备以 `Cryptex1,UDID`（4 个零字节 + BE chip + BE ecid）标识。
pub fn cryptex_tss_request(
    identity: &CryptexIdentity,
    chip: &CryptexChip,
    nonce: &[u8],
    digests: &BTreeMap<String, Vec<u8>>,
) -> plist::Dictionary {
    let mut request = plist::Dictionary::new();
    request.insert("@Cryptex1,Ticket".into(), plist::Value::Boolean(true));
    request.insert(
        "@HostPlatformInfo".into(),
        plist::Value::String("mac".into()),
    );
    request.insert(
        "@VersionInfo".into(),
        plist::Value::String(CRYPTEX_TSS_VERSION_INFO.into()),
    );
    request.insert(
        "@UUID".into(),
        plist::Value::String(uuid::Uuid::new_v4().to_string().to_uppercase()),
    );
    request.insert("ApSecurityMode".into(), plist::Value::Boolean(chip.secure));
    request.insert(
        "Cryptex1,UDID".into(),
        plist::Value::Data(cryptex_udid(chip.chip, chip.ecid)),
    );
    request.insert("Cryptex1,ChipID".into(), int_value(chip.fchp));
    request.insert("Cryptex1,Type".into(), int_value(chip.typ));
    request.insert("Cryptex1,ProductClass".into(), int_value(chip.clas));
    request.insert(
        "Cryptex1,ProductionMode".into(),
        plist::Value::Boolean(chip.production),
    );
    request.insert(
        "Cryptex1,UseProductClass".into(),
        plist::Value::Boolean(identity.use_product_class),
    );
    request.insert("Cryptex1,SubType".into(), int_value(identity.sub_type));
    request.insert(
        "Cryptex1,NonceDomain".into(),
        int_value(identity.nonce_domain),
    );
    request.insert("Cryptex1,Nonce".into(), plist::Value::Data(nonce.to_vec()));
    request.insert(
        "Cryptex1,Version".into(),
        plist::Value::String(identity.version.clone()),
    );
    request.insert(
        "Cryptex1,PreauthorizationVersion".into(),
        identity.preauthorization_version.clone(),
    );
    // 缺少它票据会缺 'uniq' tag。
    request.insert(
        "Cryptex1,UniqueTagList".into(),
        plist::Value::Data(Vec::new()),
    );
    for (key, digest) in digests {
        let mut entry = plist::Dictionary::new();
        entry.insert("Digest".into(), plist::Value::Data(digest.clone()));
        request.insert(key.clone(), plist::Value::Dictionary(entry));
    }
    request
}

fn int_value(value: u64) -> plist::Value {
    plist::Value::Integer(value.into())
}

fn xpc_value(value: &plist::Value) -> idevice::xpc::XPCObject {
    match value {
        plist::Value::Boolean(b) => idevice::xpc::XPCObject::Bool(*b),
        plist::Value::Integer(i) => match i.as_signed() {
            Some(signed) if signed >= 0 => {
                idevice::xpc::XPCObject::UInt64(u64::try_from(signed).unwrap_or_default())
            }
            Some(signed) => idevice::xpc::XPCObject::Int64(signed),
            None => idevice::xpc::XPCObject::Int64(0),
        },
        plist::Value::Real(f) => idevice::xpc::XPCObject::Double(*f),
        plist::Value::String(s) => idevice::xpc::XPCObject::String(s.clone()),
        plist::Value::Data(d) => idevice::xpc::XPCObject::Data(d.clone()),
        plist::Value::Array(items) => {
            idevice::xpc::XPCObject::Array(items.iter().map(xpc_value).collect())
        }
        plist::Value::Dictionary(dict) => idevice::xpc::XPCObject::Dictionary(
            dict.iter()
                .map(|(key, value)| (key.clone(), xpc_value(value)))
                .collect(),
        ),
        plist::Value::Date(date) => idevice::xpc::XPCObject::String(format!("{date:?}")),
        _ => idevice::xpc::XPCObject::Null,
    }
}

/// `Cryptex1,UDID`：4 个零字节后跟大端 chip id 与 ECID（go-ios `cryptexUDID`）。
pub fn cryptex_udid(chip: u64, ecid: u64) -> Vec<u8> {
    let mut udid = vec![0_u8; 16];
    udid[4..8].copy_from_slice(&(chip as u32).to_be_bytes());
    udid[8..16].copy_from_slice(&ecid.to_be_bytes());
    udid
}

/// 解析 `read-personalization-id` 的 argv（go-ios `parseCryptexChip`）。
pub fn parse_cryptex_chip(argv: &plist::Dictionary) -> CoreResult<CryptexChip> {
    let get = |name: &str| -> CoreResult<u64> {
        argv.get(&format!("img4_chip_{name}"))
            .and_then(to_uint64)
            .ok_or_else(|| {
                CoreError::DeviceUnavailable(format!(
                    "read-personalization-id response is missing img4_chip_{name}"
                ))
            })
    };
    Ok(CryptexChip {
        chip: get("chip")?,
        ecid: get("ecid")?,
        fchp: get("fchp")?,
        typ: get("type")?,
        clas: get("clas")?,
        production: get("cpro")? != 0,
        secure: get("csec")? != 0,
    })
}

/// 从序列化 `img4_nonce_t` 提取 nonce：56 字节 = u16 version + 48B nonce +
/// padding + u32 LE 长度（offset 52）；nonce 为 `b[2..2+len]`（go-ios
/// `parseImg4Nonce`）。
pub fn parse_img4_nonce(bytes: &[u8]) -> CoreResult<Vec<u8>> {
    const MAX_NONCE_LENGTH: usize = 48;
    if bytes.len() != 56 {
        return Err(CoreError::DeviceUnavailable(format!(
            "expected 56-byte serialized img4_nonce_t but got {} bytes",
            bytes.len()
        )));
    }
    let length = u32::from_le_bytes([bytes[52], bytes[53], bytes[54], bytes[55]]) as usize;
    if length == 0 || length > MAX_NONCE_LENGTH {
        return Err(CoreError::DeviceUnavailable(format!(
            "invalid nonce length {length} in img4_nonce_t"
        )));
    }
    Ok(bytes[2..2 + length].to_vec())
}

/// 解析 cryptexd 例程应答（go-ios `parseCryptexReply`）：`cferr` 或非零 `error`
/// 视为失败；否则返回 `argv`（可为空字典）。
pub fn parse_cryptex_reply(
    routine: &str,
    reply: &plist::Dictionary,
) -> CoreResult<plist::Dictionary> {
    if let Some(cferr) = reply.get("cferr") {
        return Err(CoreError::DeviceUnavailable(format!(
            "cryptexd routine {routine:?} failed: {cferr:?}"
        )));
    }
    if let Some(error) = reply.get("error").and_then(|v| v.as_signed_integer())
        && error != 0
    {
        return Err(CoreError::DeviceUnavailable(format!(
            "cryptexd routine {routine:?} failed with errno {error}"
        )));
    }
    Ok(reply
        .get("argv")
        .and_then(|v| v.as_dictionary())
        .cloned()
        .unwrap_or_default())
}

/// 解析 `copy-installed` 的 `remote-cryptex-array` 条目。
pub fn parse_remote_cryptex(dict: &plist::Dictionary) -> RemoteCryptex {
    RemoteCryptex {
        identifier: string_value(dict, "remote-cryptex-identifier")
            .unwrap_or_default()
            .to_owned(),
        version: string_value(dict, "remote-cryptex-version")
            .unwrap_or_default()
            .to_owned(),
    }
}

/// 已安装的开发者磁盘 cryptex 列表（过滤 identifier==DDI），格式
/// `identifier@version`（对齐 go-ios `ListImages` 的输出形状）。
pub fn filter_ddi_cryptexes(installed: &[RemoteCryptex]) -> Vec<String> {
    installed
        .iter()
        .filter(|cryptex| cryptex.identifier == DDI_CRYPTEX_IDENTIFIER)
        .map(|cryptex| format!("{}@{}", cryptex.identifier, cryptex.version))
        .collect()
}

/// 隧道是否支持 cryptex DDI：RSD 服务表含 cryptexd.remote（需要运行中的 tunnel）。
pub async fn supports_cryptex_ddi(tunnel: &dyn UserSpaceTunnel) -> bool {
    tunnel
        .rsd_services()
        .await
        .map(|services| services.contains_key(CRYPTEX_SERVICE_NAME))
        .unwrap_or(false)
}

/// 打开一条 cryptexd XPC 连接：隧道路由拨号 → RemoteXPC 三步引导（与 RSD 连接
/// 相同，go-ios `CreateXpcConnection`/`initializeXpcConnection`）。
pub async fn connect_cryptex_client(tunnel: &dyn UserSpaceTunnel) -> CoreResult<CryptexClient> {
    let services = tunnel.rsd_services().await?;
    let port = services
        .get(CRYPTEX_SERVICE_NAME)
        .map(|service| service.port)
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!(
                "RSD service {CRYPTEX_SERVICE_NAME:?} is not advertised"
            ))
        })?;
    let port = u16::try_from(port)
        .map_err(|_| CoreError::DeviceUnavailable(format!("cryptexd port {port} out of range")))?;
    let io = tunnel.connect(port).await?;
    let mut client = idevice::xpc::RemoteXpcClient::new(IdeviceSharedRsdSocket(io))
        .await
        .map_err(map_device_error)?;
    client.do_handshake().await.map_err(map_device_error)?;
    Ok(client)
}

/// 调用一个 cryptexd 例程并返回其应答的 argv（go-ios `cryptexCall`）。
pub async fn cryptex_call(
    client: &mut CryptexClient,
    routine: &str,
    argv: idevice::xpc::XPCObject,
) -> CoreResult<plist::Dictionary> {
    let mut body = idevice::xpc::Dictionary::new();
    body.insert(
        "routine".into(),
        idevice::xpc::XPCObject::String(routine.into()),
    );
    body.insert("argv".into(), argv);
    // HeartbeatRequestFlag（wire 值 0x10000）= 本 crate 的 `WantingReply`；
    // send_object 的 expect_reply 附加的正是该位。
    client
        .send_object(body, true)
        .await
        .map_err(map_device_error)?;
    let reply = client.recv().await.map_err(map_device_error)?;
    let dict = reply.as_dictionary().cloned().ok_or_else(|| {
        CoreError::DeviceUnavailable(format!(
            "cryptexd routine {routine:?} reply is not a dictionary"
        ))
    })?;
    parse_cryptex_reply(routine, &dict)
}

/// `copy-installed`：列出 cryptexd 已安装的全部 cryptex。
pub async fn list_cryptexes(client: &mut CryptexClient) -> CoreResult<Vec<RemoteCryptex>> {
    let argv = cryptex_call(
        client,
        "copy-installed",
        idevice::xpc::XPCObject::Dictionary(idevice::xpc::Dictionary::new()),
    )
    .await?;
    let entries = argv
        .get("remote-cryptex-array")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(entries
        .iter()
        .filter_map(|entry| entry.as_dictionary().map(parse_remote_cryptex))
        .collect())
}

/// `read-personalization-id`：读取设备的 img4 chip 属性。
pub async fn read_cryptex_chip(client: &mut CryptexClient) -> CoreResult<CryptexChip> {
    let mut argv = idevice::xpc::Dictionary::new();
    argv.insert(
        "client-version".into(),
        idevice::xpc::XPCObject::UInt64(CRYPTEX_CLIENT_VERSION),
    );
    let argv = cryptex_call(
        client,
        "read-personalization-id",
        idevice::xpc::XPCObject::Dictionary(argv),
    )
    .await?;
    parse_cryptex_chip(&argv)
}

/// `get-nonce`：读取 cryptex nonce 域的 nonce 并解析 img4_nonce_t。
pub async fn get_cryptex_nonce(
    client: &mut CryptexClient,
    nonce_domain: u64,
) -> CoreResult<Vec<u8>> {
    let mut argv = idevice::xpc::Dictionary::new();
    argv.insert(
        "nonce-domain-handle".into(),
        idevice::xpc::XPCObject::UInt64(nonce_domain),
    );
    let argv = cryptex_call(
        client,
        "get-nonce",
        idevice::xpc::XPCObject::Dictionary(argv),
    )
    .await?;
    let raw = argv.get("nonce").and_then(|v| v.as_data()).ok_or_else(|| {
        CoreError::DeviceUnavailable("get-nonce response is missing nonce data".into())
    })?;
    parse_img4_nonce(raw)
}

/// 单个待上传的 cryptex 载荷：manifest 条目文件或内联字节（im4m）。
enum CryptexUpload {
    File {
        arg: &'static str,
        path: std::path::PathBuf,
        size: Option<u64>,
    },
    Bytes {
        arg: &'static str,
        data: Vec<u8>,
    },
}

impl CryptexUpload {
    fn arg(&self) -> &'static str {
        match self {
            CryptexUpload::File { arg, .. } | CryptexUpload::Bytes { arg, .. } => arg,
        }
    }
}

fn upload_payload_size(upload: &CryptexUpload) -> CoreResult<u64> {
    match upload {
        CryptexUpload::File { path, size, .. } => size.ok_or_else(|| {
            CoreError::InvalidArgument(format!("unresolved file size for {}", path.display()))
        }),
        CryptexUpload::Bytes { data, .. } => Ok(data.len() as u64),
    }
}

/// `install`：上传五个载荷并安装 cryptex（go-ios `installCryptex`）。每个文件
/// 一条独立 HTTP/2 流，消息体只放 FileTransfer 引用（msg id 2..6），install
/// 请求本身使用 id 1。
pub async fn install_cryptex(
    client: &mut CryptexClient,
    identity: &CryptexIdentity,
    restore_dir: &Path,
    im4m: Vec<u8>,
) -> CoreResult<RemoteCryptex> {
    let from_manifest = |arg: &'static str, key: &'static str| -> CoreResult<CryptexUpload> {
        let entry = identity.entries.get(key).ok_or_else(|| {
            CoreError::InvalidArgument(format!("the Cryptex1 identity has no {key} entry"))
        })?;
        let path = entry.path.as_deref().ok_or_else(|| {
            CoreError::InvalidArgument(format!("manifest entry {key} has no Info.Path"))
        })?;
        Ok(CryptexUpload::File {
            arg,
            path: restore_dir.join(path),
            size: None,
        })
    };
    let mut uploads = [
        from_manifest("image", CRYPTEX_DMG_KEY)?,
        from_manifest("volumehash", CRYPTEX_VOLUME_KEY)?,
        from_manifest("info", CRYPTEX_INFO_PLIST_KEY)?,
        from_manifest("trustcache", CRYPTEX_TRUST_CACHE_KEY)?,
        CryptexUpload::Bytes {
            arg: "im4m",
            data: im4m,
        },
    ];
    // File 载荷的大小需要 async stat（go-ios 用 getFileSize）。
    for upload in &mut uploads {
        if let CryptexUpload::File {
            path, size: None, ..
        } = upload
        {
            let metadata = tokio::fs::metadata(&*path).await.map_err(|error| {
                CoreError::InvalidArgument(format!("failed to stat {}: {error}", path.display()))
            })?;
            if let CryptexUpload::File { size, .. } = upload {
                *size = Some(metadata.len());
            }
        }
    }

    let mut argv = idevice::xpc::Dictionary::new();
    argv.insert("auth".into(), idevice::xpc::XPCObject::UInt64(0));
    argv.insert(
        "client-version".into(),
        idevice::xpc::XPCObject::UInt64(CRYPTEX_CLIENT_VERSION),
    );
    argv.insert(
        "image-type-index".into(),
        idevice::xpc::XPCObject::Int64(CRYPTEX_IMAGE_TYPE_INDEX),
    );
    argv.insert(
        "persistence".into(),
        idevice::xpc::XPCObject::UInt64(CRYPTEX_PERSISTENCE),
    );
    argv.insert(
        "nonce-persistence".into(),
        idevice::xpc::XPCObject::UInt64(CRYPTEX_NONCE_PERSISTENCE),
    );
    let mut cryptex1_properties = idevice::xpc::Dictionary::new();
    cryptex1_properties.insert(
        "Cryptex1,NonceDomain".into(),
        idevice::xpc::XPCObject::UInt64(identity.nonce_domain),
    );
    cryptex1_properties.insert(
        "Cryptex1,PreauthVersion".into(),
        xpc_value(&identity.preauthorization_version),
    );
    cryptex1_properties.insert(
        "Cryptex1,SubType".into(),
        idevice::xpc::XPCObject::UInt64(identity.sub_type),
    );
    cryptex1_properties.insert(
        "Cryptex1,UseProductClass".into(),
        idevice::xpc::XPCObject::Bool(identity.use_product_class),
    );
    cryptex1_properties.insert(
        "Cryptex1,Version".into(),
        idevice::xpc::XPCObject::String(identity.version.clone()),
    );
    cryptex1_properties.insert(
        "MountedCryptex".into(),
        idevice::xpc::XPCObject::Bool(false),
    );
    argv.insert(
        "cryptex1-properties".into(),
        idevice::xpc::XPCObject::Dictionary(cryptex1_properties),
    );

    // 每个文件一条独立流，stream id 避开 root(1)/reply(3)，取 `2*(id+1)`。
    for (index, upload) in uploads.iter().enumerate() {
        let id = 2 + index as u64;
        let stream_id = 2_u32 * (id as u32 + 1);
        let size = upload_payload_size(upload)?;
        let reference = idevice::xpc::XPCObject::FileTransfer {
            msg_id: id,
            data: Box::new(idevice::xpc::XPCObject::Dictionary(
                [("s".to_string(), idevice::xpc::XPCObject::UInt64(size))]
                    .into_iter()
                    .collect(),
            )),
        };
        argv.insert(upload.arg().to_string(), reference);
        client
            .open_file_transfer(id, stream_id)
            .await
            .map_err(map_device_error)?;
    }

    let mut request = idevice::xpc::Dictionary::new();
    request.insert(
        "routine".into(),
        idevice::xpc::XPCObject::String("install".into()),
    );
    request.insert("argv".into(), idevice::xpc::XPCObject::Dictionary(argv));
    client
        .send_object(request, true)
        .await
        .map_err(map_device_error)?;

    // 设备只有在收到引用消息后才会接受各传输流；逐个等 accept 并裸传载荷。
    for (index, upload) in uploads.iter().enumerate() {
        let id = 2 + index as u64;
        let stream_id = 2_u32 * (id as u32 + 1);
        let arg = upload.arg();
        client
            .wait_file_transfer_accepted(id, stream_id)
            .await
            .map_err(|error| {
                CoreError::DeviceUnavailable(format!(
                    "cryptexd did not accept upload {arg:?}: {error}"
                ))
            })?;
        write_upload_payload(client, stream_id, upload).await?;
    }

    let reply = client.recv().await.map_err(map_device_error)?;
    let dict = reply.as_dictionary().cloned().ok_or_else(|| {
        CoreError::DeviceUnavailable("cryptexd install reply is not a dictionary".into())
    })?;
    let result = parse_cryptex_reply("install", &dict)?;
    let installed = result
        .get("remote-cryptex")
        .and_then(|v| v.as_dictionary())
        .ok_or_else(|| {
            CoreError::DeviceUnavailable("cryptexd install reply is missing remote-cryptex".into())
        })?;
    Ok(parse_remote_cryptex(installed))
}

async fn write_upload_payload(
    client: &mut CryptexClient,
    stream_id: u32,
    upload: &CryptexUpload,
) -> CoreResult<()> {
    match upload {
        CryptexUpload::Bytes { data, .. } => {
            let mut offset = 0;
            while offset < data.len() {
                let end = (offset + UPLOAD_CHUNK_BYTES).min(data.len());
                client
                    .write_file_transfer_chunk(stream_id, data[offset..end].to_vec())
                    .await
                    .map_err(map_device_error)?;
                offset = end;
            }
            Ok(())
        }
        CryptexUpload::File { path, .. } => {
            let mut file = tokio::fs::File::open(path).await.map_err(|error| {
                CoreError::InvalidArgument(format!("failed to open {}: {error}", path.display()))
            })?;
            let mut buffer = vec![0_u8; UPLOAD_CHUNK_BYTES];
            loop {
                let read = file.read(&mut buffer).await.map_err(CoreError::Io)?;
                if read == 0 {
                    break;
                }
                client
                    .write_file_transfer_chunk(stream_id, buffer[..read].to_vec())
                    .await
                    .map_err(map_device_error)?;
            }
            Ok(())
        }
    }
}

/// 从 Apple TSS 请求 `Cryptex1,Ticket` 票据（vendor `TSSRequest` 通用参数 +
/// `send()`，应答票据在 `Cryptex1,Ticket` 键下）。
pub async fn request_cryptex_ticket(request_params: plist::Dictionary) -> CoreResult<Vec<u8>> {
    let mut request = idevice::tss::TSSRequest::new();
    for (key, value) in request_params {
        request.insert(key, value);
    }
    let response = request.send().await.map_err(map_device_error)?;
    response
        .as_dictionary()
        .and_then(|dict| dict.get("Cryptex1,Ticket"))
        .and_then(|v| v.as_data())
        .map(|ticket| ticket.to_vec())
        .ok_or_else(|| {
            CoreError::DeviceUnavailable("TSS response is missing Cryptex1,Ticket".into())
        })
}

/// 完整挂载流程（go-ios `CryptexDeveloperDiskImageMounter.MountImage`）：
/// 连接 1 读 chip + nonce，本地算摘要，TSS 取票据，连接 2 安装。
pub async fn mount_cryptex_ddi(
    tunnel: &dyn UserSpaceTunnel,
    build_manifest: &plist::Value,
    restore_dir: &Path,
) -> CoreResult<RemoteCryptex> {
    let mut client = connect_cryptex_client(tunnel).await?;
    let chip = read_cryptex_chip(&mut client).await?;
    let identity = find_cryptex_identity(build_manifest, &chip)?;
    let nonce = get_cryptex_nonce(&mut client, identity.nonce_domain).await?;
    drop(client);

    let digests = cryptex_digests(&identity, restore_dir)?;
    let request = cryptex_tss_request(&identity, &chip, &nonce, &digests);
    let im4m = request_cryptex_ticket(request).await?;

    // CoreDevice 在独立连接上安装 cryptex。
    let mut client = connect_cryptex_client(tunnel).await?;
    install_cryptex(&mut client, &identity, restore_dir, im4m).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_with(entries: &[(&str, Option<&str>, bool)]) -> CryptexIdentity {
        let mut map = BTreeMap::new();
        for (key, path, personalize) in entries {
            map.insert(
                (*key).to_string(),
                CryptexManifestEntry {
                    path: path.map(|p| p.to_string()),
                    personalize: *personalize,
                    digest: Vec::new(),
                },
            );
        }
        CryptexIdentity {
            entries: map,
            chip_id: 0x8110,
            typ: 0x11111,
            clas: 0,
            use_product_class: false,
            sub_type: 3,
            nonce_domain: 9,
            version: "24A427".into(),
            preauthorization_version: plist::Value::String("1.0".into()),
            info_plist_path: Some("cryptex_info.plist".into()),
        }
    }

    #[test]
    fn to_uint64_accepts_integers_and_hex_strings() {
        assert_eq!(
            to_uint64(&plist::Value::Integer(0x8110.into())),
            Some(0x8110)
        );
        assert_eq!(
            to_uint64(&plist::Value::String("0x8110".into())),
            Some(0x8110)
        );
        assert_eq!(to_uint64(&plist::Value::String("42".into())), Some(42));
        assert_eq!(to_uint64(&plist::Value::String("nope".into())), None);
        assert_eq!(to_uint64(&plist::Value::Boolean(true)), None);
    }

    #[test]
    fn cryptex_udid_layout_matches_go_ios() {
        // 4 个零字节 + BE u32 chip + BE u64 ecid。
        let udid = cryptex_udid(0x8110, 0x1234_5678_90ab_cdef);
        assert_eq!(udid.len(), 16);
        assert_eq!(&udid[0..4], &[0, 0, 0, 0]);
        assert_eq!(&udid[4..8], &0x8110_u32.to_be_bytes());
        assert_eq!(&udid[8..16], &0x1234_5678_90ab_cdef_u64.to_be_bytes());
    }

    #[test]
    fn img4_nonce_parses_56_byte_layout() {
        // u16 version + 48B nonce + padding + u32 LE 长度（offset 52）。
        let mut raw = vec![0_u8; 56];
        raw[0..2].copy_from_slice(&1_u16.to_le_bytes());
        for (index, byte) in raw[2..14].iter_mut().enumerate() {
            *byte = (index as u8) + 1;
        }
        raw[52..56].copy_from_slice(&12_u32.to_le_bytes());
        let nonce = parse_img4_nonce(&raw).unwrap();
        assert_eq!(nonce.len(), 12);
        assert_eq!(nonce, raw[2..14]);

        assert!(parse_img4_nonce(&raw[..55]).is_err());
        raw[52..56].copy_from_slice(&0_u32.to_le_bytes());
        assert!(parse_img4_nonce(&raw).is_err());
        raw[52..56].copy_from_slice(&49_u32.to_le_bytes());
        assert!(parse_img4_nonce(&raw).is_err());
    }

    #[test]
    fn parse_cryptex_chip_requires_all_img4_fields() {
        let mut argv = plist::Dictionary::new();
        for (name, value) in [
            ("img4_chip_chip", 1),
            ("img4_chip_ecid", 2),
            ("img4_chip_fchp", 3),
            ("img4_chip_type", 4),
            ("img4_chip_clas", 5),
            ("img4_chip_cpro", 1),
            ("img4_chip_csec", 0),
        ] {
            argv.insert(name.to_string(), plist::Value::Integer(value.into()));
        }
        let chip = parse_cryptex_chip(&argv).unwrap();
        assert_eq!(chip.chip, 1);
        assert_eq!(chip.ecid, 2);
        assert_eq!(chip.fchp, 3);
        assert_eq!(chip.typ, 4);
        assert_eq!(chip.clas, 5);
        assert!(chip.production);
        assert!(!chip.secure);

        argv.remove("img4_chip_fchp").unwrap();
        assert!(parse_cryptex_chip(&argv).is_err());
    }

    #[test]
    fn cryptex_tss_request_has_no_ap_identifiers() {
        let identity = identity_with(&[
            ("Cryptex1,GenericDmg", Some("022-111.dmg"), true),
            ("Cryptex1,GenericTrustCache", Some("trustcache"), true),
            (
                "Cryptex1,CryptexInfoPlist",
                Some("cryptex_info.plist"),
                true,
            ),
        ]);
        let mut digests = BTreeMap::new();
        digests.insert("Cryptex1,GenericDmg".to_string(), vec![1_u8; 48]);
        let chip = CryptexChip {
            chip: 0x8110,
            ecid: 0x42,
            fchp: 0x8110,
            typ: 0x11111,
            clas: 0x0a,
            production: true,
            secure: true,
        };
        let request = cryptex_tss_request(&identity, &chip, &[7; 32], &digests);
        assert_eq!(
            request.get("@Cryptex1,Ticket"),
            Some(&plist::Value::Boolean(true))
        );
        // 与 ApImg4Ticket 的区别：不带 ApECID/ApChipID/ApBoardID。
        assert!(request.get("ApECID").is_none());
        assert!(request.get("ApChipID").is_none());
        assert!(request.get("ApBoardID").is_none());
        assert_eq!(
            request.get("Cryptex1,UDID"),
            Some(&plist::Value::Data(cryptex_udid(0x8110, 0x42)))
        );
        assert_eq!(
            request.get("@VersionInfo"),
            Some(&plist::Value::String("libauthinstall-1104.0.9".into()))
        );
        assert_eq!(
            request.get("Cryptex1,UniqueTagList"),
            Some(&plist::Value::Data(Vec::new()))
        );
        let entry = request
            .get("Cryptex1,GenericDmg")
            .and_then(|v| v.as_dictionary())
            .unwrap();
        assert_eq!(
            entry.get("Digest"),
            Some(&plist::Value::Data(vec![1_u8; 48]))
        );
    }

    #[test]
    fn parse_cryptex_reply_reports_cferr_and_errno() {
        let mut cferr = plist::Dictionary::new();
        cferr.insert("code".into(), plist::Value::Integer(7.into()));
        let mut reply = plist::Dictionary::new();
        reply.insert("cferr".into(), plist::Value::Dictionary(cferr));
        assert!(parse_cryptex_reply("install", &reply).is_err());

        let mut errno = plist::Dictionary::new();
        errno.insert("error".into(), plist::Value::Integer(2.into()));
        assert!(parse_cryptex_reply("install", &errno).is_err());

        let mut ok = plist::Dictionary::new();
        ok.insert("error".into(), plist::Value::Integer(0.into()));
        let mut argv = plist::Dictionary::new();
        argv.insert(
            "remote-cryptex".into(),
            plist::Value::Dictionary(plist::Dictionary::new()),
        );
        ok.insert("argv".into(), plist::Value::Dictionary(argv));
        assert_eq!(parse_cryptex_reply("install", &ok).unwrap().len(), 1);
    }

    #[test]
    fn filter_ddi_cryptexes_keeps_only_ddi_identifier() {
        let installed = vec![
            RemoteCryptex {
                identifier: "com.apple.MobileAsset.SomethingElse".into(),
                version: "1".into(),
            },
            RemoteCryptex {
                identifier: "com.apple.MobileAsset.DDI".into(),
                version: "17F113".into(),
            },
        ];
        assert_eq!(
            filter_ddi_cryptexes(&installed),
            vec!["com.apple.MobileAsset.DDI@17F113"]
        );
    }
}
