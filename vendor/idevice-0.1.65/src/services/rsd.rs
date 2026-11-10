//! Remote Service Discovery
//! Communicates via XPC and returns advertised services

use std::collections::HashMap;

use serde::Deserialize;
use tracing::{debug, warn};

use crate::{IdeviceError, ReadWrite, RemoteXpcClient, provider::RsdProvider};

/// 设备握手未携带 `RemoteXPCVersionFlags` 时 host announce 的回退值（旧版
/// `send_device_handshake` 的固定值）。正常设备都会声明 flags，此常量仅为兼容。
const LEGACY_REMOTE_XPC_VERSION_FLAGS: u64 = 0x0100_0000_0000_0006;

/// Describes an available XPC service
#[derive(Debug, Clone, Deserialize)]
pub struct RsdService {
    /// Required entitlement to access this service
    pub entitlement: String,
    /// Port number where the service is available
    pub port: u16,
    /// Whether the service uses remote XPC
    pub uses_remote_xpc: bool,
    /// Optional list of supported features
    pub features: Option<Vec<String>>,
    /// Optional service version number
    pub service_version: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct RsdHandshake {
    pub services: HashMap<String, RsdService>,
    pub protocol_version: usize,
    pub properties: HashMap<String, plist::Value>,
    pub uuid: String,
}

impl RsdHandshake {
    pub async fn new(socket: impl ReadWrite) -> Result<Self, IdeviceError> {
        let mut xpc_client = RemoteXpcClient::new(socket).await?;
        xpc_client.do_handshake().await?;
        // 先接收并解析设备握手：所有解析失败都发生在 announce 之前，此时 host
        // 尚未向 remoted 声明自己，整个握手可以安全重试（go-ios 6910f3b 的
        // 发送次序不变量）。
        let data = xpc_client.recv_root().await?;

        let dict = match data.as_dictionary() {
            Some(d) => d,
            None => {
                return Err(IdeviceError::UnexpectedResponse(
                    "RSD handshake is not a dictionary".into(),
                ));
            }
        };

        // Parse available services
        let mut services: HashMap<String, RsdService> = HashMap::new();
        let services_dict = match dict
            .get("Services")
            .and_then(|x| x.as_dictionary())
        {
            Some(d) => d,
            None => {
                return Err(IdeviceError::UnexpectedResponse(
                    "missing Services dictionary in RSD handshake".into(),
                ));
            }
        };
        for (name, service) in services_dict.into_iter() {
            match service.as_dictionary() {
                Some(service) => {
                    let entitlement = match service.get("Entitlement").and_then(|x| x.as_string()) {
                        Some(e) => e.to_string(),
                        None => {
                            warn!("Service did not contain entitlement string");
                            continue;
                        }
                    };
                    let port = match service
                        .get("Port")
                        .and_then(|x| x.as_string())
                        .and_then(|x| x.parse::<u16>().ok())
                    {
                        Some(e) => e,
                        None => {
                            warn!("Service did not contain port string");
                            continue;
                        }
                    };
                    let uses_remote_xpc = match service
                        .get("Properties")
                        .and_then(|x| x.as_dictionary())
                        .and_then(|x| x.get("UsesRemoteXPC"))
                        .and_then(|x| x.as_boolean())
                    {
                        Some(e) => e.to_owned(),
                        None => false, // default is false
                    };

                    let features = service
                        .get("Properties")
                        .and_then(|x| x.as_dictionary())
                        .and_then(|x| x.get("Features"))
                        .and_then(|x| x.as_array())
                        .map(|f| {
                            f.iter()
                                .filter_map(|x| x.as_string())
                                .map(|x| x.to_string())
                                .collect::<Vec<String>>()
                        });

                    let service_version = service
                        .get("Properties")
                        .and_then(|x| x.as_dictionary())
                        .and_then(|x| x.get("ServiceVersion"))
                        .and_then(|x| x.as_signed_integer())
                        .map(|e| e.to_owned());

                    services.insert(
                        name.to_string(),
                        RsdService {
                            entitlement,
                            port,
                            uses_remote_xpc,
                            features,
                            service_version,
                        },
                    );
                }
                None => {
                    warn!("Service is not a dictionary!");
                    continue;
                }
            }
        }

        let protocol_version = match dict.get("MessagingProtocolVersion").and_then(|x| {
            x.as_signed_integer()
        }) {
            Some(p) => p as usize,
            None => {
                return Err(IdeviceError::UnexpectedResponse(
                    "missing MessagingProtocolVersion in RSD handshake".into(),
                ));
            }
        };

        let uuid = match dict.get("UUID").and_then(|x| x.as_string()) {
            Some(u) => u.to_string(),
            None => {
                return Err(IdeviceError::UnexpectedResponse(
                    "missing UUID in RSD handshake".into(),
                ));
            }
        };

        let properties = match dict
            .get("Properties")
            .and_then(|x| x.as_dictionary())
        {
            Some(d) => d
                .into_iter()
                .map(|(name, prop)| (name.to_owned(), prop.to_owned()))
                .collect::<HashMap<String, plist::Value>>(),
            None => {
                return Err(IdeviceError::UnexpectedResponse(
                    "missing Properties dictionary in RSD handshake".into(),
                ));
            }
        };

        // 设备握手解析成功后 announce 恰一次：回显设备声明的 MessagingProtocolVersion
        // 与 RemoteXPCVersionFlags，携带新的 BootSessionUUID（同步 go-ios `6910f3b`/
        // `51887da`）。设备缺 flags 时回退旧版固定值。
        let version_flags = properties
            .get("RemoteXPCVersionFlags")
            .and_then(|x| x.as_signed_integer())
            .filter(|value| *value >= 0)
            .map(|value| value as u64)
            .unwrap_or(LEGACY_REMOTE_XPC_VERSION_FLAGS);
        xpc_client
            .send_host_handshake(protocol_version as u64, version_flags)
            .await?;

        Ok(Self {
            services,
            protocol_version,
            properties,
            uuid,
        })
    }

    pub async fn connect<T>(&mut self, provider: &mut impl RsdProvider) -> Result<T, IdeviceError>
    where
        T: crate::RsdService,
    {
        let service_name = T::rsd_service_name();
        let service = match self.services.get(&service_name.to_string()) {
            Some(s) => s,
            None => {
                return Err(IdeviceError::ServiceNotFound);
            }
        };

        debug!(
            "Connecting to RSD service {service_name} on port {}",
            service.port
        );
        let stream = provider.connect_to_service_port(service.port).await?;
        T::from_stream(stream).await
    }
}
