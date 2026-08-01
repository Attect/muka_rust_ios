use std::borrow::Cow;

use idevice::{Idevice, IdeviceError, IdeviceService};
use serde_json::{Value as JsonValue, json};
use sha1::{Digest, Sha1};

use crate::management::WallpaperScreen;
use crate::supervision::{certificate_from_identity, sign_pkcs12_challenge};

pub(crate) const HTTP_PROXY_PROFILE_IDENTIFIER: &str =
    "Go-iOS.CD15976B-E205-4213-9B8E-FDAA5FAB1C22";

#[derive(Debug)]
pub(crate) struct McInstallClient {
    idevice: Idevice,
}

impl IdeviceService for McInstallClient {
    fn service_name() -> Cow<'static, str> {
        Cow::Borrowed("com.apple.mobile.MCInstall")
    }

    async fn from_stream(idevice: Idevice) -> Result<Self, IdeviceError> {
        Ok(Self { idevice })
    }
}

impl McInstallClient {
    pub(crate) async fn flush(&mut self) -> Result<(), IdeviceError> {
        self.request_acknowledged("Flush", plist::Dictionary::new())
            .await
    }

    pub(crate) async fn hello_host_identifier(&mut self) -> Result<(), IdeviceError> {
        let mut request = plist::Dictionary::new();
        request.insert("RequestType".into(), "HelloHostIdentifier".into());
        self.exchange(request).await.map(|_| ())
    }

    pub(crate) async fn get_cloud_configuration(
        &mut self,
    ) -> Result<plist::Dictionary, IdeviceError> {
        let mut request = plist::Dictionary::new();
        request.insert("RequestType".into(), "GetCloudConfiguration".into());
        self.exchange(request).await
    }

    pub(crate) async fn set_cloud_configuration(
        &mut self,
        configuration: plist::Dictionary,
    ) -> Result<(), IdeviceError> {
        let mut fields = plist::Dictionary::new();
        fields.insert(
            "CloudConfiguration".into(),
            plist::Value::Dictionary(configuration),
        );
        self.request_acknowledged("SetCloudConfiguration", fields)
            .await
    }

    pub(crate) async fn escalate_unsupervised(&mut self) -> Result<bool, IdeviceError> {
        let mut request = plist::Dictionary::new();
        request.insert("RequestType".into(), "Escalate".into());
        request.insert("SupervisorCertificate".into(), plist::Value::Data(vec![0]));
        let response = self.exchange(request).await?;
        Ok(response.get("Status").and_then(plist::Value::as_string) == Some("Acknowledged"))
    }

    pub(crate) async fn list_profiles(&mut self) -> Result<JsonValue, IdeviceError> {
        let mut request = plist::Dictionary::new();
        request.insert("RequestType".into(), "GetProfileList".into());
        let response = self.exchange(request).await?;
        profile_list_json(&response)
    }

    pub(crate) async fn install_profile(&mut self, profile: Vec<u8>) -> Result<(), IdeviceError> {
        self.install_profile_with_type(profile, "InstallProfile")
            .await
    }

    pub(crate) async fn install_profile_silent(
        &mut self,
        profile: Vec<u8>,
        p12_bytes: &[u8],
        password: &str,
    ) -> Result<(), IdeviceError> {
        self.escalate(p12_bytes, password).await?;
        self.install_profile_with_type(profile, "InstallProfileSilent")
            .await
    }

    pub(crate) async fn set_wallpaper_supervised(
        &mut self,
        image: Vec<u8>,
        screen: WallpaperScreen,
        p12_bytes: &[u8],
        password: &str,
    ) -> Result<(), IdeviceError> {
        self.escalate(p12_bytes, password).await?;
        let response = self.exchange(wallpaper_request(image, screen)).await?;
        require_wallpaper_acknowledged(&response)
    }

    async fn escalate(&mut self, p12_bytes: &[u8], password: &str) -> Result<(), IdeviceError> {
        let certificate = certificate_from_identity(p12_bytes, password)
            .map_err(|error| IdeviceError::UnexpectedResponse(error.to_string()))?;
        let mut request = plist::Dictionary::new();
        request.insert("RequestType".into(), "Escalate".into());
        request.insert(
            "SupervisorCertificate".into(),
            plist::Value::Data(certificate),
        );
        let response = self.exchange(request).await?;
        require_acknowledged(&response, "Escalate")?;
        let challenge = response
            .get("Challenge")
            .and_then(plist::Value::as_data)
            .ok_or_else(|| {
                IdeviceError::UnexpectedResponse(
                    "MCInstall Escalate response missing Challenge".into(),
                )
            })?;
        let signed = sign_pkcs12_challenge(p12_bytes, password, challenge)
            .map_err(|error| IdeviceError::UnexpectedResponse(error.to_string()))?;
        let mut response_fields = plist::Dictionary::new();
        response_fields.insert(
            "SignedRequest".into(),
            plist::Value::Data(signed.signed_request),
        );
        self.request_acknowledged("EscalateResponse", response_fields)
            .await?;
        self.request_acknowledged("ProceedWithKeybagMigration", plist::Dictionary::new())
            .await
    }

    pub(crate) async fn remove_profile(&mut self, identifier: &str) -> Result<(), IdeviceError> {
        let mut request = plist::Dictionary::new();
        request.insert("RequestType".into(), "RemoveProfile".into());
        request.insert("ProfileIdentifier".into(), identifier.into());
        let response = self.exchange(request).await?;
        require_acknowledged(&response, "RemoveProfile")
    }

    pub(crate) async fn erase_device(&mut self) -> Result<bool, IdeviceError> {
        let request = erase_device_request();
        let mut bytes = Vec::new();
        plist::Value::Dictionary(request).to_writer_xml(&mut bytes)?;
        self.idevice
            .send_raw(&(bytes.len() as u32).to_be_bytes())
            .await?;
        self.idevice.send_raw(&bytes).await?;

        let length = match self.idevice.read_raw(4).await {
            Ok(length) => length,
            Err(error) if is_expected_erase_disconnect(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let length = u32::from_be_bytes(length.try_into().expect("four bytes")) as usize;
        const MAX_MCINSTALL_FRAME: usize = 16 * 1024 * 1024;
        if length == 0 || length > MAX_MCINSTALL_FRAME {
            return Err(IdeviceError::UnexpectedResponse(format!(
                "invalid MCInstall erase response length {length}"
            )));
        }
        let response = match self.idevice.read_raw(length).await {
            Ok(response) => response,
            Err(error) if is_expected_erase_disconnect(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let response: plist::Dictionary = plist::from_bytes(&response)?;
        require_acknowledged(&response, "EraseDevice")?;
        Ok(true)
    }

    async fn install_profile_with_type(
        &mut self,
        profile: Vec<u8>,
        request_type: &str,
    ) -> Result<(), IdeviceError> {
        let mut fields = plist::Dictionary::new();
        fields.insert("Payload".into(), plist::Value::Data(profile));
        self.request_acknowledged(request_type, fields).await
    }

    async fn request_acknowledged(
        &mut self,
        request_type: &str,
        mut fields: plist::Dictionary,
    ) -> Result<(), IdeviceError> {
        fields.insert("RequestType".into(), request_type.into());
        let response = self.exchange(fields).await?;
        require_acknowledged(&response, request_type)
    }

    async fn exchange(
        &mut self,
        request: plist::Dictionary,
    ) -> Result<plist::Dictionary, IdeviceError> {
        const MAX_MCINSTALL_FRAME: usize = 16 * 1024 * 1024;
        let mut bytes = Vec::new();
        plist::Value::Dictionary(request).to_writer_xml(&mut bytes)?;
        if bytes.len() > MAX_MCINSTALL_FRAME {
            return Err(IdeviceError::UnexpectedResponse(format!(
                "MCInstall request exceeds {MAX_MCINSTALL_FRAME} bytes"
            )));
        }
        self.idevice
            .send_raw(&(bytes.len() as u32).to_be_bytes())
            .await?;
        self.idevice.send_raw(&bytes).await?;

        let length = self.idevice.read_raw(4).await?;
        let length = u32::from_be_bytes(length.try_into().expect("four bytes")) as usize;
        if length == 0 || length > MAX_MCINSTALL_FRAME {
            return Err(IdeviceError::UnexpectedResponse(format!(
                "invalid MCInstall response length {length}"
            )));
        }
        let response = self.idevice.read_raw(length).await?;
        plist::from_bytes(&response).map_err(Into::into)
    }
}

fn erase_device_request() -> plist::Dictionary {
    plist::Dictionary::from_iter([
        (
            "RequestType".to_owned(),
            plist::Value::String("EraseDevice".into()),
        ),
        (
            "PreserveDataPlan".to_owned(),
            plist::Value::Integer(1_u64.into()),
        ),
    ])
}

fn is_expected_erase_disconnect(error: &IdeviceError) -> bool {
    matches!(
        error,
        IdeviceError::Socket(error)
            if error.kind() == std::io::ErrorKind::UnexpectedEof
    )
}

fn wallpaper_request(image: Vec<u8>, screen: WallpaperScreen) -> plist::Dictionary {
    let setting = plist::Dictionary::from_iter([
        ("Item".to_owned(), plist::Value::String("Wallpaper".into())),
        (
            "Where".to_owned(),
            plist::Value::Integer(screen.where_value().into()),
        ),
        ("Image".to_owned(), plist::Value::Data(image)),
    ]);
    plist::Dictionary::from_iter([
        (
            "RequestType".to_owned(),
            plist::Value::String("Settings".into()),
        ),
        (
            "Settings".to_owned(),
            plist::Value::Array(vec![plist::Value::Dictionary(setting)]),
        ),
    ])
}

fn require_wallpaper_acknowledged(response: &plist::Dictionary) -> Result<(), IdeviceError> {
    require_acknowledged(response, "Settings/Wallpaper")?;
    let setting = response
        .get("Settings")
        .and_then(plist::Value::as_array)
        .and_then(|settings| settings.first())
        .and_then(plist::Value::as_dictionary)
        .ok_or_else(|| {
            IdeviceError::UnexpectedResponse(
                "MCInstall Settings/Wallpaper response missing Settings[0] dictionary".into(),
            )
        })?;
    require_acknowledged(setting, "Settings/Wallpaper item")
}

pub(crate) fn wifi_profile(
    ssid: &str,
    password: &str,
    encryption_type: &str,
) -> Result<(String, Vec<u8>), IdeviceError> {
    if ssid.is_empty() || password.is_empty() {
        return Err(IdeviceError::UnexpectedResponse(
            "wifi requires non-empty --ssid and --password".into(),
        ));
    }
    if !matches!(
        encryption_type,
        "WEP" | "WPA" | "WPA2" | "WPA3" | "Any" | "None"
    ) {
        return Err(IdeviceError::UnexpectedResponse(format!(
            "unsupported Wi-Fi encryption type {encryption_type:?}"
        )));
    }
    let identifier = wifi_profile_identifier(ssid)?;
    let mut payload = plist::Dictionary::new();
    payload.insert("AutoJoin".into(), true.into());
    payload.insert("EncryptionType".into(), encryption_type.into());
    payload.insert("HIDDEN_NETWORK".into(), false.into());
    payload.insert("SSID_STR".into(), ssid.into());
    payload.insert("Password".into(), password.into());
    payload.insert("PayloadDisplayName".into(), "Wi-Fi".into());
    payload.insert(
        "PayloadIdentifier".into(),
        format!("{identifier}.payload").into(),
    );
    payload.insert("PayloadType".into(), "com.apple.wifi.managed".into());
    payload.insert(
        "PayloadUUID".into(),
        profile_uuid(&format!("{ssid}.payload")).into(),
    );
    payload.insert("PayloadVersion".into(), 1_u64.into());

    let mut profile = plist::Dictionary::new();
    profile.insert(
        "PayloadContent".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(payload)]),
    );
    profile.insert("PayloadIdentifier".into(), identifier.clone().into());
    profile.insert("PayloadType".into(), "Configuration".into());
    profile.insert("PayloadUUID".into(), profile_uuid(ssid).into());
    profile.insert("PayloadVersion".into(), 1_u64.into());
    Ok((identifier, plist_xml(profile)?))
}

pub(crate) fn wifi_profile_identifier(ssid: &str) -> Result<String, IdeviceError> {
    if ssid.is_empty() {
        return Err(IdeviceError::UnexpectedResponse(
            "wifi requires a non-empty --ssid".into(),
        ));
    }
    let safe_ssid = sanitize_identifier(ssid);
    if safe_ssid.is_empty() {
        return Err(IdeviceError::UnexpectedResponse(
            "Wi-Fi SSID does not contain a usable profile identifier character".into(),
        ));
    }
    Ok(format!("com.apple.wifi.managed.{safe_ssid}"))
}

pub(crate) fn http_proxy_profile(
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<Vec<u8>, IdeviceError> {
    if host.trim().is_empty() || port == 0 {
        return Err(IdeviceError::UnexpectedResponse(
            "httpproxy requires a non-empty host and port 1..65535".into(),
        ));
    }
    if username.is_none() && password.is_some() {
        return Err(IdeviceError::UnexpectedResponse(
            "httpproxy password requires a username".into(),
        ));
    }
    let mut payload = plist::Dictionary::new();
    payload.insert("PayloadDescription".into(), "Global HTTP Proxy".into());
    payload.insert("PayloadDisplayName".into(), "Global HTTP Proxy".into());
    payload.insert(
        "PayloadIdentifier".into(),
        "com.apple.proxy.http.global.20A1B29D-7945-4C7C-9A49-649D3751F85D".into(),
    );
    payload.insert("PayloadType".into(), "com.apple.proxy.http.global".into());
    payload.insert(
        "PayloadUUID".into(),
        "20A1B29D-7945-4C7C-9A49-649D3751F85D".into(),
    );
    payload.insert("PayloadVersion".into(), 1_u64.into());
    payload.insert("ProxyCaptiveLoginAllowed".into(), true.into());
    payload.insert("ProxyServer".into(), host.into());
    payload.insert("ProxyServerPort".into(), u64::from(port).into());
    payload.insert("ProxyType".into(), "Manual".into());
    if let Some(username) = username {
        payload.insert("ProxyUsername".into(), username.into());
        payload.insert("ProxyPassword".into(), password.unwrap_or_default().into());
    }

    let mut profile = plist::Dictionary::new();
    profile.insert(
        "PayloadContent".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(payload)]),
    );
    profile.insert("PayloadDisplayName".into(), "Untitled".into());
    profile.insert(
        "PayloadIdentifier".into(),
        HTTP_PROXY_PROFILE_IDENTIFIER.into(),
    );
    profile.insert("PayloadRemovalDisallowed".into(), false.into());
    profile.insert("PayloadType".into(), "Configuration".into());
    profile.insert(
        "PayloadUUID".into(),
        "20A1B29D-7945-4C7C-9A49-649D3751F85D".into(),
    );
    profile.insert("PayloadVersion".into(), 1_u64.into());
    plist_xml(profile)
}

pub(crate) fn initial_prepare_profile() -> Result<Vec<u8>, IdeviceError> {
    let mut restrictions = plist::Dictionary::new();
    restrictions.insert(
        "PayloadDescription".into(),
        "Configures Restrictions".into(),
    );
    restrictions.insert("PayloadDisplayName".into(), "Restrictions".into());
    restrictions.insert(
        "PayloadIdentifier".into(),
        "com.apple.applicationaccess.8D384EDB-E60E-4D14-8E07-98ED164374D3".into(),
    );
    restrictions.insert("PayloadType".into(), "com.apple.applicationaccess".into());
    restrictions.insert(
        "PayloadUUID".into(),
        "889918E1-B5AA-4AD6-B819-240081988CAB".into(),
    );
    restrictions.insert("PayloadVersion".into(), 1_u64.into());
    restrictions.insert("allowUSBRestrictedMode".into(), false.into());
    restrictions.insert("allowUnpairedExternalBootToRecovery".into(), false.into());

    let mut profile = plist::Dictionary::new();
    profile.insert(
        "PayloadContent".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(restrictions)]),
    );
    profile.insert(
        "PayloadDescription".into(),
        "USB Accessories while locked allowed".into(),
    );
    profile.insert(
        "PayloadDisplayName".into(),
        "USB Accessories while locked allowed".into(),
    );
    profile.insert(
        "PayloadIdentifier".into(),
        "com.apple.configurator.usbrestrictedmode".into(),
    );
    profile.insert("PayloadRemovalDisallowed".into(), false.into());
    profile.insert("PayloadType".into(), "Configuration".into());
    profile.insert(
        "PayloadUUID".into(),
        "7E1B12F3-FCF1-4120-83A0-26E950519103".into(),
    );
    profile.insert("PayloadVersion".into(), 1_u64.into());
    plist_xml(profile)
}

fn sanitize_identifier(input: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in input.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-');
            }
            result.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    result
}

fn profile_uuid(seed: &str) -> String {
    let digest = Sha1::digest(format!("go-ios.wifi.{seed}").as_bytes());
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode_upper(&digest[0..4]),
        hex::encode_upper(&digest[4..6]),
        hex::encode_upper(&digest[6..8]),
        hex::encode_upper(&digest[8..10]),
        hex::encode_upper(&digest[10..16]),
    )
}

fn plist_xml(dictionary: plist::Dictionary) -> Result<Vec<u8>, IdeviceError> {
    let mut bytes = Vec::new();
    plist::Value::Dictionary(dictionary).to_writer_xml(&mut bytes)?;
    Ok(bytes)
}

fn require_acknowledged(response: &plist::Dictionary, operation: &str) -> Result<(), IdeviceError> {
    if response.get("Status").and_then(plist::Value::as_string) == Some("Acknowledged") {
        Ok(())
    } else {
        Err(IdeviceError::UnexpectedResponse(format!(
            "MCInstall {operation} was not acknowledged: {response:?}"
        )))
    }
}

fn profile_list_json(response: &plist::Dictionary) -> Result<JsonValue, IdeviceError> {
    let identifiers = response
        .get("OrderedIdentifiers")
        .and_then(plist::Value::as_array)
        .ok_or_else(|| {
            IdeviceError::UnexpectedResponse(
                "MCInstall GetProfileList missing OrderedIdentifiers".into(),
            )
        })?;
    let manifests = response
        .get("ProfileManifest")
        .and_then(plist::Value::as_dictionary)
        .ok_or_else(|| {
            IdeviceError::UnexpectedResponse("MCInstall missing ProfileManifest".into())
        })?;
    let metadata = response
        .get("ProfileMetadata")
        .and_then(plist::Value::as_dictionary)
        .ok_or_else(|| {
            IdeviceError::UnexpectedResponse("MCInstall missing ProfileMetadata".into())
        })?;
    let status = response
        .get("Status")
        .and_then(plist::Value::as_string)
        .unwrap_or_default();

    let mut profiles = Vec::with_capacity(identifiers.len());
    for identifier in identifiers {
        let identifier = identifier.as_string().ok_or_else(|| {
            IdeviceError::UnexpectedResponse(
                "MCInstall OrderedIdentifiers contains a non-string".into(),
            )
        })?;
        let manifest = manifests
            .get(identifier)
            .and_then(plist::Value::as_dictionary)
            .ok_or_else(|| {
                IdeviceError::UnexpectedResponse(format!(
                    "MCInstall missing manifest for {identifier}"
                ))
            })?;
        let profile_metadata = metadata
            .get(identifier)
            .and_then(plist::Value::as_dictionary)
            .ok_or_else(|| {
                IdeviceError::UnexpectedResponse(format!(
                    "MCInstall missing metadata for {identifier}"
                ))
            })?;
        profiles.push(json!({
            "Identifier": identifier,
            "Manifest": {
                "Description": string_value(manifest, "Description"),
                "IsActive": bool_value(manifest, "IsActive"),
            },
            "Metadata": {
                "PayloadDescription": string_value(profile_metadata, "PayloadDescription"),
                "PayloadDisplayName": string_value(profile_metadata, "PayloadDisplayName"),
                "PayloadRemovalDisallowed": bool_value(profile_metadata, "PayloadRemovalDisallowed"),
                "PayloadUUID": string_value(profile_metadata, "PayloadUUID"),
                "PayloadVersion": unsigned_value(profile_metadata, "PayloadVersion"),
            },
            "Status": status,
        }));
    }
    Ok(JsonValue::Array(profiles))
}

fn string_value<'a>(dictionary: &'a plist::Dictionary, key: &str) -> &'a str {
    dictionary
        .get(key)
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
}

fn bool_value(dictionary: &plist::Dictionary, key: &str) -> bool {
    dictionary
        .get(key)
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false)
}

fn unsigned_value(dictionary: &plist::Dictionary, key: &str) -> u64 {
    dictionary
        .get(key)
        .and_then(plist::Value::as_unsigned_integer)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_profile_list_to_go_ios_shape() {
        let mut manifest = plist::Dictionary::new();
        manifest.insert("Description".into(), "Test profile".into());
        manifest.insert("IsActive".into(), true.into());
        let mut manifests = plist::Dictionary::new();
        manifests.insert(
            "com.example.profile".into(),
            plist::Value::Dictionary(manifest),
        );
        let mut metadata = plist::Dictionary::new();
        metadata.insert("PayloadDescription".into(), "Description".into());
        metadata.insert("PayloadDisplayName".into(), "Display".into());
        metadata.insert("PayloadRemovalDisallowed".into(), false.into());
        metadata.insert("PayloadUUID".into(), "UUID".into());
        metadata.insert("PayloadVersion".into(), 1_u64.into());
        let mut all_metadata = plist::Dictionary::new();
        all_metadata.insert(
            "com.example.profile".into(),
            plist::Value::Dictionary(metadata),
        );
        let mut response = plist::Dictionary::new();
        response.insert(
            "OrderedIdentifiers".into(),
            plist::Value::Array(vec!["com.example.profile".into()]),
        );
        response.insert("ProfileManifest".into(), manifests.into());
        response.insert("ProfileMetadata".into(), all_metadata.into());
        response.insert("Status".into(), "Acknowledged".into());
        let value = profile_list_json(&response).unwrap();
        assert_eq!(value[0]["Identifier"], "com.example.profile");
        assert_eq!(value[0]["Manifest"]["IsActive"], true);
        assert_eq!(value[0]["Metadata"]["PayloadVersion"], 1);
    }

    #[test]
    fn generated_profiles_preserve_values_without_xml_injection() {
        let (identifier, bytes) = wifi_profile("Lab<& Wi-Fi", "p<&ss", "WPA2").unwrap();
        assert_eq!(identifier, "com.apple.wifi.managed.Lab-Wi-Fi");
        let value: plist::Value = plist::from_bytes(&bytes).unwrap();
        let payload = value
            .as_dictionary()
            .unwrap()
            .get("PayloadContent")
            .and_then(plist::Value::as_array)
            .and_then(|values| values.first())
            .and_then(plist::Value::as_dictionary)
            .unwrap();
        assert_eq!(
            payload.get("SSID_STR").and_then(plist::Value::as_string),
            Some("Lab<& Wi-Fi")
        );
        assert_eq!(
            payload.get("Password").and_then(plist::Value::as_string),
            Some("p<&ss")
        );

        let bytes = http_proxy_profile("proxy<&", 8080, Some("u<&"), Some("p<&")).unwrap();
        let value: plist::Value = plist::from_bytes(&bytes).unwrap();
        let payload = value
            .as_dictionary()
            .unwrap()
            .get("PayloadContent")
            .and_then(plist::Value::as_array)
            .and_then(|values| values.first())
            .and_then(plist::Value::as_dictionary)
            .unwrap();
        assert_eq!(
            payload.get("ProxyServer").and_then(plist::Value::as_string),
            Some("proxy<&")
        );
        assert_eq!(
            payload
                .get("ProxyServerPort")
                .and_then(plist::Value::as_unsigned_integer),
            Some(8080)
        );
        assert_eq!(
            payload
                .get("ProxyPassword")
                .and_then(plist::Value::as_string),
            Some("p<&")
        );
    }

    #[test]
    fn generated_profiles_reject_invalid_inputs() {
        assert!(wifi_profile("", "password", "WPA").is_err());
        assert!(wifi_profile("ssid", "password", "invalid").is_err());
        assert!(http_proxy_profile("host", 0, None, None).is_err());
        assert!(http_proxy_profile("host", 8080, None, Some("password")).is_err());
    }

    #[test]
    fn erase_request_preserves_data_plan_and_has_no_implicit_options() {
        let request = erase_device_request();
        assert_eq!(
            request.get("RequestType").and_then(plist::Value::as_string),
            Some("EraseDevice")
        );
        assert_eq!(
            request
                .get("PreserveDataPlan")
                .and_then(plist::Value::as_unsigned_integer),
            Some(1)
        );
        assert_eq!(request.len(), 2);
    }

    #[test]
    fn erase_accepts_only_eof_as_an_expected_disconnect() {
        let socket_error = |kind| IdeviceError::Socket(std::io::Error::from(kind));
        assert!(is_expected_erase_disconnect(&socket_error(
            std::io::ErrorKind::UnexpectedEof
        )));
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::TimedOut,
        ] {
            assert!(!is_expected_erase_disconnect(&socket_error(kind)));
        }
    }

    #[test]
    fn wallpaper_request_matches_mdm_settings_shape() {
        let request = wallpaper_request(vec![1, 2, 3], WallpaperScreen::Both);
        assert_eq!(
            request.get("RequestType").and_then(plist::Value::as_string),
            Some("Settings")
        );
        let settings = request
            .get("Settings")
            .and_then(plist::Value::as_array)
            .unwrap();
        assert_eq!(settings.len(), 1);
        let setting = settings[0].as_dictionary().unwrap();
        assert_eq!(setting.len(), 3);
        assert_eq!(
            setting.get("Item").and_then(plist::Value::as_string),
            Some("Wallpaper")
        );
        assert_eq!(
            setting
                .get("Where")
                .and_then(plist::Value::as_unsigned_integer),
            Some(3)
        );
        assert_eq!(
            setting.get("Image").and_then(plist::Value::as_data),
            Some([1, 2, 3].as_slice())
        );
        assert_eq!(request.len(), 2);
    }

    #[test]
    fn wallpaper_response_requires_outer_and_item_acknowledgements() {
        let response = |outer: &str, inner: Option<&str>| {
            let mut response = plist::Dictionary::from_iter([(
                "Status".to_owned(),
                plist::Value::String(outer.into()),
            )]);
            if let Some(inner) = inner {
                response.insert(
                    "Settings".into(),
                    plist::Value::Array(vec![plist::Value::Dictionary(
                        plist::Dictionary::from_iter([(
                            "Status".to_owned(),
                            plist::Value::String(inner.into()),
                        )]),
                    )]),
                );
            }
            response
        };
        assert!(
            require_wallpaper_acknowledged(&response("Acknowledged", Some("Acknowledged"))).is_ok()
        );
        assert!(require_wallpaper_acknowledged(&response("Error", Some("Acknowledged"))).is_err());
        assert!(require_wallpaper_acknowledged(&response("Acknowledged", None)).is_err());
        assert!(require_wallpaper_acknowledged(&response("Acknowledged", Some("Error"))).is_err());
    }
}
