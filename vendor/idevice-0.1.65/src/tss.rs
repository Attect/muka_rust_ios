//! Ticket Signature Server (TSS) Client
//!
//! Provides functionality for interacting with Apple's TSS service to:
//! - Request personalized firmware components
//! - Apply restore request rules for device-specific parameters
//! - Handle cryptographic signing operations

use plist::Value;
use plist_macro::plist_to_xml_bytes;
use tracing::{debug, warn};

use crate::IdeviceError;

/// TSS client version string sent in requests
const TSS_CLIENT_VERSION_STRING: &str = "libauthinstall-1033.0.2";
/// Apple's TSS endpoint URL
///
/// HTTPS, mirroring go-ios `ec90af1`: the TSS exchange carries the device
/// personalization parameters (ECID, nonces) and returns signed tickets, so it
/// must not travel over plaintext. Certificate validation is delegated to the
/// leaf sanity check in [`tls_sanity`] instead of full chain verification.
const TSS_CONTROLLER_ACTION_URL: &str = "https://gs.apple.com/TSS/controller?action=2";

/// Builds the HTTP client used for TSS requests.
///
/// The client uses the rustls backend with a custom verifier that skips chain
/// validation (gs.apple.com chains to Apple's private "Apple Server
/// Authentication CA", which is absent from the trust store on Windows and
/// minimal Linux hosts) but sanity-checks the leaf certificate, mirroring
/// go-ios `verifyTssServerCert`.
fn tss_http_client() -> Result<reqwest::Client, IdeviceError> {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| {
            IdeviceError::UnexpectedResponse(format!("rustls protocol config failed: {e}"))
        })?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(
            tls_sanity::TssLeafSanityVerifier::new(&provider),
        ))
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(config)
        .build()
        .map_err(Into::into)
}

/// Leaf-certificate sanity checks for the TSS TLS connection.
///
/// gs.apple.com chains to Apple's private root CA, so full chain verification
/// breaks cold DDI mounts on hosts without that root (upstream go-ios #348
/// skipped verification for exactly this reason, and `ec90af1` restored
/// usability with this same tradeoff). Instead of trusting everything, the
/// leaf must parse, name gs.apple.com, and be inside its validity window;
/// handshake signatures are still verified with the provider's algorithms.
mod tls_sanity {
    use rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use rustls::crypto::{
        verify_tls12_signature, verify_tls13_signature, CryptoProvider,
        WebPkiSupportedAlgorithms,
    };
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{CertificateError, DigitallySignedStruct, Error, SignatureScheme};
    use tracing::debug;
    use webpki::EndEntityCert;

    /// The only hostname a TSS server certificate may vouch for.
    const TSS_HOST: &str = "gs.apple.com";

    /// Verifier that replaces chain validation with leaf sanity checks.
    #[derive(Debug)]
    pub(super) struct TssLeafSanityVerifier {
        algs: WebPkiSupportedAlgorithms,
    }

    impl TssLeafSanityVerifier {
        pub(super) fn new(provider: &CryptoProvider) -> Self {
            Self {
                algs: provider.signature_verification_algorithms,
            }
        }

        fn reject(err: CertificateError) -> Error {
            Error::InvalidCertificate(err)
        }
    }

    impl ServerCertVerifier for TssLeafSanityVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            let expected = ServerName::try_from(TSS_HOST)
                .map_err(|_| Self::reject(CertificateError::NotValidForName))?;
            // The connected name must be the pinned TSS host, and the leaf
            // must actually vouch for it via its SANs.
            let matches_pinned = matches!(
                server_name,
                ServerName::DnsName(name) if name.as_ref() == TSS_HOST
            );
            if !matches_pinned {
                return Err(Self::reject(CertificateError::NotValidForName));
            }
            let leaf = EndEntityCert::try_from(end_entity)
                .map_err(|_| Self::reject(CertificateError::BadEncoding))?;
            leaf.verify_is_valid_for_subject_name(&expected)
                .map_err(|_| Self::reject(CertificateError::NotValidForName))?;
            let (not_before, not_after) = leaf_validity_window(end_entity.as_ref())
                .ok_or_else(|| Self::reject(CertificateError::BadEncoding))?;
            let now_secs = now.as_secs() as i64;
            if now_secs < not_before {
                return Err(Self::reject(CertificateError::NotValidYet));
            }
            if now_secs > not_after {
                return Err(Self::reject(CertificateError::Expired));
            }
            debug!(
                host = TSS_HOST,
                not_after = not_after,
                "tss server certificate accepted"
            );
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(message, cert, dss, &self.algs)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(message, cert, dss, &self.algs)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.algs.supported_schemes()
        }
    }

    /// Returns the leaf's `(not_before, not_after)` as Unix seconds.
    ///
    /// Walks the top-level DER structure only:
    /// `Certificate → tbsCertificate ([0] version, serial, signature alg,
    /// issuer) → validity → notBefore/notAfter` (UTCTime or GeneralizedTime).
    pub(super) fn leaf_validity_window(cert_der: &[u8]) -> Option<(i64, i64)> {
        let mut top = cert_der;
        // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
        // signatureValue } — peel the outer SEQUENCE, then take the first
        // field as tbsCertificate.
        let certificate = expect_tlv(&mut top, 0x30)?;
        let mut fields = certificate;
        let tbs = expect_tlv(&mut fields, 0x30)?;
        let mut tbs = tbs;
        // `[0] EXPLICIT version` is optional and only present for v3 certs.
        let mut probe = tbs;
        if matches!(read_tlv(&mut probe), Some((0xA0, _))) {
            read_tlv(&mut tbs)?;
        }
        read_tlv(&mut tbs)?; // serialNumber
        read_tlv(&mut tbs)?; // signature AlgorithmIdentifier
        read_tlv(&mut tbs)?; // issuer
        let validity = expect_tlv(&mut tbs, 0x30)?;
        let mut validity = validity;
        let (tag_before, before) = read_tlv(&mut validity)?;
        let (tag_after, after) = read_tlv(&mut validity)?;
        Some((
            der_time_to_unix(tag_before, before)?,
            der_time_to_unix(tag_after, after)?,
        ))
    }

    fn expect_tlv<'a>(input: &mut &'a [u8], tag: u8) -> Option<&'a [u8]> {
        let (actual, value) = read_tlv(input)?;
        if actual != tag {
            return None;
        }
        Some(value)
    }

    /// Reads one DER tag-length-value, returning `(tag, content)`.
    pub(super) fn read_tlv<'a>(input: &mut &'a [u8]) -> Option<(u8, &'a [u8])> {
        if input.len() < 2 {
            return None;
        }
        let tag = input[0];
        let mut idx = 1;
        let first_len = input[idx];
        idx += 1;
        let len = if first_len & 0x80 == 0 {
            first_len as usize
        } else {
            let num_bytes = (first_len & 0x7f) as usize;
            if num_bytes == 0 || num_bytes > 4 || input.len() < idx + num_bytes {
                return None;
            }
            let mut len = 0usize;
            for byte in &input[idx..idx + num_bytes] {
                len = (len << 8) | *byte as usize;
            }
            idx += num_bytes;
            len
        };
        if input.len() < idx + len {
            return None;
        }
        let content = &input[idx..idx + len];
        *input = &input[idx + len..];
        Some((tag, content))
    }

    /// Parses X.509 `Time` (UTCTime `YYMMDDHHMMSSZ` / GeneralizedTime
    /// `YYYYMMDDHHMMSSZ`) into Unix seconds.
    pub(super) fn der_time_to_unix(tag: u8, value: &[u8]) -> Option<i64> {
        let (year, rest) = match tag {
            0x17 => {
                // UTCTime: RFC 5280 maps YY >= 50 to 19YY, else 20YY.
                if value.len() < 12 {
                    return None;
                }
                let yy = ascii_digits(&value[..2])?;
                let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
                (year as i64, &value[2..])
            }
            0x18 => {
                if value.len() < 14 {
                    return None;
                }
                (ascii_digits(&value[..4])? as i64, &value[4..])
            }
            _ => return None,
        };
        if rest.len() < 10 {
            return None;
        }
        let month = ascii_digits(&rest[0..2])?;
        let day = ascii_digits(&rest[2..4])?;
        let hour = ascii_digits(&rest[4..6])?;
        let minute = ascii_digits(&rest[6..8])?;
        let second = ascii_digits(&rest[8..10])?;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return None;
        }
        let days = days_from_civil(year, month, day);
        Some(days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64)
    }

    fn ascii_digits(bytes: &[u8]) -> Option<u32> {
        let mut value = 0u32;
        for byte in bytes {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + u32::from(byte - b'0');
        }
        Some(value)
    }

    /// Days since the Unix epoch for a civil date (Howard Hinnant's
    /// `days_from_civil`, valid for the whole i64 year range used here).
    pub(super) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (month as i64 + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
}

/// Represents a TSS request to Apple's signing server
#[derive(Debug)]
pub struct TSSRequest {
    /// The underlying plist dictionary containing request parameters
    inner: plist::Dictionary,
}

impl TSSRequest {
    /// Creates a new TSS request with default headers
    ///
    /// Initializes with:
    /// - Host platform info
    /// - Client version string
    /// - Random UUID for request identification
    pub fn new() -> Self {
        let inner = plist_macro::plist!(dict {
            "@HostPlatformInfo": "mac",
            "@VersionInfo": TSS_CLIENT_VERSION_STRING,
            "@UUID": uuid::Uuid::new_v4().to_string().to_uppercase()
        });
        Self { inner }
    }

    /// Inserts a key-value pair into the TSS request
    ///
    /// # Arguments
    /// * `key` - The parameter name
    /// * `val` - The parameter value (will be converted to plist::Value)
    pub fn insert(&mut self, key: impl Into<String>, val: impl Into<Value>) {
        let key = key.into();
        let val = val.into();
        self.inner.insert(key, val);
    }

    /// Sends the TSS request to Apple's servers
    ///
    /// # Returns
    /// The parsed plist response from Apple
    ///
    /// # Errors
    /// Returns `IdeviceError` if:
    /// - The request fails
    /// - The response is malformed
    /// - Apple returns a non-success status
    ///
    /// # Example
    /// ```rust
    /// let mut request = TSSRequest::new();
    /// request.insert("ApBoardID", board_id);
    /// request.insert("ApChipID", chip_id);
    /// let response = request.send().await?;
    /// ```
    pub async fn send(&self) -> Result<plist::Value, IdeviceError> {
        debug!(
            "Sending TSS request: {}",
            crate::pretty_print_dictionary(&self.inner)
        );
        let client = tss_http_client()?;

        let res = client
            .post(TSS_CONTROLLER_ACTION_URL)
            .header("Cache-Control", "no-cache")
            .header("Content-type", "text/xml; charset=\"utf-8\"")
            .header("User-Agent", "InetURL/1.0")
            .body(plist_to_xml_bytes(&self.inner))
            .send()
            .await?
            .text()
            .await?;

        debug!("Apple responded with {res}");
        let trimmed = res.trim_start_matches("STATUS=0&");
        let trimmed = trimmed.trim_start_matches("MESSAGE=");
        if !trimmed.starts_with("SUCCESS") {
            // On failure Apple returns `STATUS=<n>&MESSAGE=<text>` (no
            // REQUEST_STRING); surface it so the caller sees why it was rejected.
            let detail = res
                .split("&REQUEST_STRING=")
                .next()
                .unwrap_or(res.as_str())
                .trim();
            warn!("TSS responded with non-success value: {detail}");
            return Err(IdeviceError::UnexpectedResponse(format!(
                "TSS server responded with non-success status ({detail})"
            )));
        }
        let res = res.split("REQUEST_STRING=").collect::<Vec<&str>>();
        if res.len() < 2 {
            warn!("Response didn't contain a request string");
            return Err(IdeviceError::UnexpectedResponse(
                "TSS response missing REQUEST_STRING".into(),
            ));
        }
        Ok(plist::from_bytes(res[1].as_bytes())?)
    }

    /// Sets the `@ApImg4Ticket` request flag.
    ///
    /// When true, the server returns a signed IMG4 manifest (`ApImg4Ticket`).
    pub fn set_ap_img4_ticket(&mut self, value: bool) {
        self.insert("@ApImg4Ticket", value);
    }

    /// Sets the `@BBTicket` request flag (request a baseband ticket).
    pub fn set_bb_ticket(&mut self, value: bool) {
        self.insert("@BBTicket", value);
    }

    /// Adds the common `Ap*` identity tags shared by the developer-disk-image
    /// personalization flow and the full IPSW restore flow.
    ///
    /// Sets `ApBoardID`, `ApChipID`, `ApECID`, `ApProductionMode` (true),
    /// `ApSecurityDomain` (1), `ApSecurityMode` (true) and `UID_MODE` (false).
    /// `ap_nonce`/`sep_nonce`, when supplied, are inserted as `ApNonce`/`SepNonce`
    /// data blobs.
    pub fn add_common_tags(
        &mut self,
        board_id: u64,
        chip_id: u64,
        ecid: u64,
        ap_nonce: Option<Vec<u8>>,
        sep_nonce: Option<Vec<u8>>,
    ) {
        self.insert("ApBoardID", board_id);
        self.insert("ApChipID", chip_id);
        self.insert("ApECID", ecid);
        self.insert("ApProductionMode", true);
        self.insert("ApSecurityDomain", 1);
        self.insert("ApSecurityMode", true);
        self.insert("UID_MODE", false);
        if let Some(n) = ap_nonce {
            self.insert("ApNonce", plist::Value::Data(n));
        }
        if let Some(n) = sep_nonce {
            self.insert("SepNonce", plist::Value::Data(n));
        }
    }

    /// Removes a key from the request, returning the removed value if present.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.inner.remove(key)
    }

    pub fn add_build_identity_tags(&mut self, build_identity: &plist::Dictionary, keys: &[&str]) {
        for &key in keys {
            if let Some(v) = build_identity.get(key) {
                let converted = match v {
                    Value::String(s) if s.starts_with("0x") => {
                        match u64::from_str_radix(s.trim_start_matches("0x"), 16) {
                            Ok(n) => Value::from(n),
                            Err(_) => v.clone(),
                        }
                    }
                    _ => v.clone(),
                };
                self.insert(key, converted);
            }
        }
    }

    pub fn add_ap_personalization_identifiers(&mut self, identifiers: &plist::Dictionary) {
        for (key, val) in identifiers {
            if key.starts_with("Ap,") {
                self.insert(key.clone(), val.clone());
            }
        }
    }

    pub fn add_ap_tags(&mut self, build_identity: &plist::Dictionary) {
        const KEYS: &[&str] = &[
            "UniqueBuildID",
            "Ap,OSLongVersion",
            "Ap,OSReleaseType",
            "Ap,ProductType",
            "Ap,SDKPlatform",
            "Ap,SikaFuse",
            "Ap,Target",
            "Ap,TargetType",
            "Ap,ProductMarketingVersion",
            "ApBoardID",
            "ApChipID",
            "ApSecurityDomain",
            "BMU,BoardID",
            "BMU,ChipID",
            "BbChipID",
            "BbProvisioningManifestKeyHash",
            "BbActivationManifestKeyHash",
            "BbCalibrationManifestKeyHash",
            "BbFactoryActivationManifestKeyHash",
            "BbFDRSecurityKeyHash",
            "BbSkeyId",
            "SE,ChipID",
            "Savage,ChipID",
            "Savage,PatchEpoch",
            "Yonkers,BoardID",
            "Yonkers,ChipID",
            "Yonkers,PatchEpoch",
            "Rap,BoardID",
            "Rap,ChipID",
            "Rap,SecurityDomain",
            "Baobab,BoardID",
            "Baobab,ChipID",
            "Baobab,ManifestEpoch",
            "Baobab,SecurityDomain",
            "eUICC,ChipID",
            "PearlCertificationRootPub",
            "Timer,BoardID,1",
            "Timer,BoardID,2",
            "Timer,ChipID,1",
            "Timer,ChipID,2",
            "Timer,SecurityDomain,1",
            "Timer,SecurityDomain,2",
            "NeRDEpoch",
        ];
        for &key in KEYS {
            if let Some(v) = build_identity.get(key) {
                let converted = match v {
                    Value::String(s) if s.starts_with("0x") => {
                        match u64::from_str_radix(s.trim_start_matches("0x"), 16) {
                            Ok(n) => Value::from(n),
                            Err(_) => v.clone(),
                        }
                    }
                    _ => v.clone(),
                };
                self.insert(key, converted);
            }
        }

        if let Some(r) = build_identity
            .get("Info")
            .and_then(|i| i.as_dictionary())
            .and_then(|i| i.get("RequiresUIDMode"))
        {
            self.insert("RequiresUIDMode", r.clone());
        }
    }

    pub fn add_ap_manifest_tags(
        &mut self,
        build_identity: &plist::Dictionary,
        parameters: &plist::Dictionary,
    ) -> Result<(), IdeviceError> {
        const SKIP_KEYS: &[&str] = &[
            "BasebandFirmware",
            "SE,UpdatePayload",
            "BaseSystem",
            "Diags",
            "Ap,ExclaveOS",
        ];

        let manifest = match build_identity.get("Manifest") {
            Some(plist::Value::Dictionary(m)) => m,
            _ => return Err(IdeviceError::BadBuildManifest),
        };
        let supports_img4 = parameters
            .get("ApSupportsImg4")
            .and_then(Value::as_boolean)
            .unwrap_or(false);

        for (key, manifest_item) in manifest {
            if SKIP_KEYS.contains(&key.as_str()) || key.starts_with("Cryptex1,") {
                continue;
            }
            let manifest_item = match manifest_item {
                plist::Value::Dictionary(m) => m,
                _ => continue,
            };
            let info = match manifest_item.get("Info") {
                Some(plist::Value::Dictionary(i)) => i,
                _ => continue,
            };
            // For IMG4 devices, only components with RestoreRequestRules belong
            // in the AP ticket.
            let has_rules = info.contains_key("RestoreRequestRules");
            if supports_img4 && !has_rules {
                debug!("skipping {key}: no RestoreRequestRules");
                continue;
            }
            if info
                .get("IsFTAB")
                .and_then(Value::as_boolean)
                .unwrap_or(false)
            {
                continue;
            }

            let mut tss_entry = manifest_item.clone();
            tss_entry.remove("Info");

            if let Some(plist::Value::Array(rules)) = info.get("RestoreRequestRules") {
                apply_restore_request_rules(&mut tss_entry, parameters, rules);
            }

            let trusted = manifest_item
                .get("Trusted")
                .and_then(Value::as_boolean)
                .unwrap_or(false);
            if trusted && !tss_entry.contains_key("Digest") {
                tss_entry.insert("Digest".into(), plist::Value::Data(Vec::new()));
            }

            self.insert(key.clone(), tss_entry);
        }

        Ok(())
    }

    pub fn add_baseband_tags(&mut self, parameters: &plist::Dictionary) {
        self.insert("@BBTicket", true);

        const KEYS: &[&str] = &[
            "BbChipID",
            "BbProvisioningManifestKeyHash",
            "BbActivationManifestKeyHash",
            "BbCalibrationManifestKeyHash",
            "BbFactoryActivationManifestKeyHash",
            "BbFDRSecurityKeyHash",
            "BbSkeyId",
            "BbNonce",
            "BbGoldCertId",
            "BbSNUM",
            "PearlCertificationRootPub",
            "Ap,OSLongVersion",
        ];
        for &key in KEYS {
            if let Some(v) = parameters.get(key) {
                self.insert(key, v.clone());
            }
        }

        if let Some(bbfw) = parameters
            .get("Manifest")
            .and_then(|m| m.as_dictionary())
            .and_then(|m| m.get("BasebandFirmware"))
            .and_then(|b| b.as_dictionary())
        {
            let mut bbfwdict = bbfw.clone();
            bbfwdict.remove("Info");

            let bb_chip_id = parameters
                .get("BbChipID")
                .and_then(Value::as_unsigned_integer);
            let bb_cert_id = parameters
                .get("BbGoldCertId")
                .and_then(Value::as_unsigned_integer);
            if bb_chip_id == Some(0x68) {
                if matches!(bb_cert_id, Some(0x26F3_FACC | 0x5CF2_EC4E | 0x8399_785A)) {
                    bbfwdict.remove("PSI2-PartialDigest");
                    bbfwdict.remove("RestorePSI2-PartialDigest");
                } else {
                    bbfwdict.remove("PSI-PartialDigest");
                    bbfwdict.remove("RestorePSI-PartialDigest");
                }
            }
            self.insert("BasebandFirmware", Value::Dictionary(bbfwdict));
        }
    }

    pub fn populate_from_manifest(
        &mut self,
        build_identity: &plist::Dictionary,
        parameters: &plist::Dictionary,
        rules_override: Option<&[plist::Value]>,
    ) -> Result<(), IdeviceError> {
        let manifest = match build_identity.get("Manifest") {
            Some(plist::Value::Dictionary(m)) => m,
            _ => return Err(IdeviceError::BadBuildManifest),
        };

        for (key, manifest_item) in manifest {
            let manifest_item = match manifest_item {
                plist::Value::Dictionary(m) => m,
                _ => {
                    debug!("Manifest item {key} wasn't a dictionary");
                    continue;
                }
            };

            let info = match manifest_item.get("Info") {
                Some(plist::Value::Dictionary(i)) => i,
                _ => {
                    debug!("Manifest item {key} didn't contain Info");
                    continue;
                }
            };

            if !matches!(
                manifest_item.get("Trusted"),
                Some(plist::Value::Boolean(true))
            ) {
                debug!("Manifest item {key} isn't trusted");
                continue;
            }

            let mut tss_entry = manifest_item.clone();
            tss_entry.remove("Info");

            let rules = match rules_override {
                Some(r) => Some(r),
                None => info
                    .get("RestoreRequestRules")
                    .and_then(|v| v.as_array())
                    .map(|v| v.as_slice()),
            };
            if let Some(rules) = rules {
                apply_restore_request_rules(&mut tss_entry, parameters, rules);
            }

            if manifest_item.get("Digest").is_none() {
                tss_entry.insert("Digest".into(), plist::Value::Data(Vec::new()));
            }

            self.insert(key.clone(), tss_entry);
        }

        Ok(())
    }
}

impl Default for TSSRequest {
    /// Creates a default TSS request (same as `new()`)
    fn default() -> Self {
        Self::new()
    }
}

/// Applies restore request rules to modify input parameters
///
/// # Arguments
/// * `input` - The dictionary to modify based on rules
/// * `parameters` - Device parameters to check conditions against
/// * `rules` - List of rules to apply
///
/// # Process
/// For each rule:
/// 1. Checks all conditions against the parameters
/// 2. If all conditions are met, applies the rule's actions
/// 3. Actions can add, modify or remove parameters
pub fn apply_restore_request_rules(
    input: &mut plist::Dictionary,
    parameters: &plist::Dictionary,
    rules: &[plist::Value],
) {
    for rule in rules {
        if let plist::Value::Dictionary(rule) = rule {
            let conditions = match rule.get("Conditions") {
                Some(plist::Value::Dictionary(c)) => c,
                _ => {
                    warn!("Conditions doesn't exist or wasn't a dictionary!");
                    continue;
                }
            };

            let mut conditions_fulfilled = true;
            for (key, value) in conditions {
                let value2 = match key.as_str() {
                    "ApRawProductionMode" => parameters.get("ApProductionMode"),
                    "ApCurrentProductionMode" => parameters.get("ApProductionMode"),
                    "ApRawSecurityMode" => parameters.get("ApSecurityMode"),
                    "ApRequiresImage4" => parameters.get("ApSupportsImg4"),
                    "ApDemotionPolicyOverride" => parameters.get("DemotionPolicy"),
                    "ApInRomDFU" => parameters.get("ApInRomDFU"),
                    _ => {
                        warn!("Unhandled key {key}");
                        None
                    }
                };

                if value2.is_none() || value2 != Some(value) {
                    conditions_fulfilled = false;
                    break; // Stop checking other conditions immediately
                }
            }

            if !conditions_fulfilled {
                continue;
            }

            let actions = match rule.get("Actions") {
                Some(plist::Value::Dictionary(a)) => a,
                _ => {
                    warn!("Actions doesn't exist or wasn't a dictionary!");
                    continue;
                }
            };

            for (key, value) in actions {
                // Skip special values (255 typically means "ignore")
                if let Some(i) = value.as_unsigned_integer()
                    && i == 255
                {
                    continue;
                }
                if let Some(i) = value.as_signed_integer()
                    && i == 255
                {
                    continue;
                }

                input.remove(key); // Explicitly remove before inserting
                input.insert(key.to_owned(), value.to_owned());
            }
        } else {
            warn!("Rule wasn't a dictionary");
        }
    }
}

fn parse_hex_field(v: Option<&plist::Value>) -> Option<u64> {
    match v {
        Some(plist::Value::String(s)) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok(),
        Some(plist::Value::Integer(i)) => i.as_unsigned(),
        _ => None,
    }
}

pub fn select_build_identity<'a>(
    build_manifest: &'a plist::Dictionary,
    board_id: u64,
    chip_id: u64,
    restore_behavior: Option<&str>,
) -> Result<&'a plist::Dictionary, IdeviceError> {
    let identities = match build_manifest.get("BuildIdentities") {
        Some(plist::Value::Array(i)) => i,
        _ => return Err(IdeviceError::BadBuildManifest),
    };

    for id in identities {
        let id = match id {
            plist::Value::Dictionary(id) => id,
            _ => {
                debug!("build identity wasn't a dictionary");
                continue;
            }
        };

        if parse_hex_field(id.get("ApBoardID")) != Some(board_id) {
            continue;
        }
        if parse_hex_field(id.get("ApChipID")) != Some(chip_id) {
            continue;
        }
        if let Some(behavior) = restore_behavior {
            let matches = id
                .get("Info")
                .and_then(|i| i.as_dictionary())
                .and_then(|i| i.get("RestoreBehavior"))
                .and_then(|b| b.as_string())
                == Some(behavior);
            if !matches {
                continue;
            }
        }
        return Ok(id);
    }

    Err(IdeviceError::BadBuildManifest)
}

/// Extracts the `ApImg4Ticket` blob from a TSS response dictionary.
///
/// # Errors
/// Returns [`IdeviceError::UnexpectedResponse`] if the ticket is absent.
pub fn extract_img4_ticket(response: &plist::Dictionary) -> Result<Vec<u8>, IdeviceError> {
    match response.get("ApImg4Ticket") {
        Some(plist::Value::Data(d)) => Ok(d.clone()),
        _ => Err(IdeviceError::UnexpectedResponse(
            "missing ApImg4Ticket data in TSS response".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boolean(value: bool) -> plist::Value {
        plist::Value::Boolean(value)
    }

    fn rule(conditions: plist::Dictionary, actions: plist::Dictionary) -> plist::Value {
        plist::Value::Dictionary(plist::Dictionary::from_iter([
            ("Conditions".to_owned(), plist::Value::Dictionary(conditions)),
            ("Actions".to_owned(), plist::Value::Dictionary(actions)),
        ]))
    }

    fn production_parameters() -> plist::Dictionary {
        plist::Dictionary::from_iter([
            ("ApProductionMode".to_owned(), boolean(true)),
            ("ApSecurityMode".to_owned(), boolean(true)),
            ("ApSupportsImg4".to_owned(), boolean(true)),
        ])
    }

    #[test]
    fn applies_actions_when_all_conditions_met() {
        // go-ios TestApplyRestoreRequestRules: a matching rule must overwrite
        // EPRO/ESEC from the input with the rule's action values.
        let mut input = plist::Dictionary::from_iter([
            ("EPRO".to_owned(), boolean(false)),
            ("ESEC".to_owned(), boolean(false)),
        ]);
        let rules = vec![rule(
            plist::Dictionary::from_iter([
                ("ApCurrentProductionMode".to_owned(), boolean(true)),
                ("ApRawSecurityMode".to_owned(), boolean(true)),
                ("ApRequiresImage4".to_owned(), boolean(true)),
            ]),
            plist::Dictionary::from_iter([
                ("EPRO".to_owned(), boolean(true)),
                ("ESEC".to_owned(), boolean(true)),
            ]),
        )];
        apply_restore_request_rules(&mut input, &production_parameters(), &rules);
        assert_eq!(input.get("EPRO"), Some(&boolean(true)));
        assert_eq!(input.get("ESEC"), Some(&boolean(true)));
    }

    #[test]
    fn skips_rule_when_condition_value_differs() {
        let mut input = plist::Dictionary::from_iter([("EPRO".to_owned(), boolean(false))]);
        let rules = vec![rule(
            plist::Dictionary::from_iter([(
                "ApCurrentProductionMode".to_owned(),
                boolean(false),
            )]),
            plist::Dictionary::from_iter([("EPRO".to_owned(), boolean(true))]),
        )];
        apply_restore_request_rules(&mut input, &production_parameters(), &rules);
        assert_eq!(input.get("EPRO"), Some(&boolean(false)));
    }

    #[test]
    fn skips_rule_on_unknown_condition_key() {
        let mut input = plist::Dictionary::from_iter([("EPRO".to_owned(), boolean(false))]);
        let rules = vec![rule(
            plist::Dictionary::from_iter([("ApUnknownKey".to_owned(), boolean(true))]),
            plist::Dictionary::from_iter([("EPRO".to_owned(), boolean(true))]),
        )];
        apply_restore_request_rules(&mut input, &production_parameters(), &rules);
        assert_eq!(input.get("EPRO"), Some(&boolean(false)));
    }

    #[test]
    fn skips_value_255_action() {
        // 255 is the "ignore" sentinel and must not overwrite the input.
        let mut input = plist::Dictionary::from_iter([("EPRO".to_owned(), boolean(false))]);
        let rules = vec![rule(
            plist::Dictionary::from_iter([(
                "ApCurrentProductionMode".to_owned(),
                boolean(true),
            )]),
            plist::Dictionary::from_iter([(
                "EPRO".to_owned(),
                plist::Value::Integer(255u64.into()),
            )]),
        )];
        apply_restore_request_rules(&mut input, &production_parameters(), &rules);
        assert_eq!(input.get("EPRO"), Some(&boolean(false)));
    }

    #[test]
    fn skips_rules_without_conditions_or_actions() {
        let mut input = plist::Dictionary::from_iter([("EPRO".to_owned(), boolean(false))]);
        let rules = vec![
            plist::Value::Dictionary(plist::Dictionary::new()),
            plist::Value::String("not a dictionary".into()),
        ];
        apply_restore_request_rules(&mut input, &production_parameters(), &rules);
        assert_eq!(input.get("EPRO"), Some(&boolean(false)));
    }

    #[test]
    fn parse_hex_field_accepts_strings_and_integers() {
        assert_eq!(
            parse_hex_field(Some(&plist::Value::String("0x1F".into()))),
            Some(31)
        );
        assert_eq!(
            parse_hex_field(Some(&plist::Value::Integer(42u64.into()))),
            Some(42)
        );
        assert_eq!(parse_hex_field(Some(&plist::Value::Boolean(true))), None);
        assert_eq!(parse_hex_field(Some(&plist::Value::String("zz".into()))), None);
        assert_eq!(parse_hex_field(None), None);
    }

    #[test]
    fn days_from_civil_known_dates() {
        use super::tls_sanity::days_from_civil;
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 1, 1), 20_454);
        assert_eq!(days_from_civil(2050, 1, 1), 29_220);
    }

    #[test]
    fn der_time_to_unix_parses_both_time_forms() {
        use super::tls_sanity::der_time_to_unix;
        // UTCTime 26-01-08 15:30:00 Z
        assert_eq!(
            der_time_to_unix(0x17, b"260108153000Z"),
            Some(1_767_886_200)
        );
        // GeneralizedTime 2050-01-01 00:00:00 Z
        assert_eq!(
            der_time_to_unix(0x18, b"20500101000000Z"),
            Some(2_524_608_000)
        );
        // UTCTime YY >= 50 maps to 19YY: 99-12-31 23:59:60 (leap second allowed)
        assert_eq!(der_time_to_unix(0x17, b"991231235960Z"), Some(946_684_800));
        assert_eq!(der_time_to_unix(0x17, b"short"), None);
        assert_eq!(der_time_to_unix(0x18, b"2050010100000Z"), None);
        assert_eq!(der_time_to_unix(0x16, b"260108153000Z"), None);
        assert_eq!(der_time_to_unix(0x17, b"991332235900Z"), None);
    }

    #[test]
    fn leaf_validity_window_parses_minimal_certificate() {
        use super::tls_sanity::leaf_validity_window;

        fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
            let mut out = vec![tag];
            // Contents here stay far below 128 bytes, so only short form
            // lengths are needed.
            out.push(content.len() as u8);
            out.extend_from_slice(content);
            out
        }

        // A minimal TBS: [0] version, serial, signature alg, issuer, validity.
        let mut tbs = tlv(0xA0, &tlv(0x03, &[0x00]));
        tbs.extend(tlv(0x02, &[0x01]));
        tbs.extend(tlv(0x30, &[0x00]));
        tbs.extend(tlv(0x30, &[0x00]));
        let validity = tlv(0x30, &[tlv(0x17, b"260108153000Z"), tlv(0x18, b"20500101000000Z")].concat());
        tbs.extend(&validity);
        // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { ... } } (the
        // trailing signatureAlgorithm/signatureValue fields are not needed).
        let cert = tlv(0x30, &tlv(0x30, &tbs));

        assert_eq!(
            leaf_validity_window(&cert),
            Some((1_767_886_200, 2_524_608_000))
        );

        // Truncated input must fail cleanly instead of panicking.
        assert_eq!(leaf_validity_window(&cert[..cert.len() - 4]), None);
        assert_eq!(leaf_validity_window(&[]), None);
    }

    #[test]
    fn read_tlv_rejects_oversized_lengths() {
        use tls_sanity::read_tlv;
        // Length prefix claims 4 following length bytes but none exist.
        assert_eq!(read_tlv(&mut &[0x30, 0x84][..]), None);
        // 0x80 = indefinite length, invalid in DER.
        assert_eq!(read_tlv(&mut &[0x30, 0x80, 0x00][..]), None);
        // Well-formed short-form TLV round-trips.
        assert_eq!(
            read_tlv(&mut &[0x30, 0x02, 0xAA, 0xBB, 0xFF][..]),
            Some((0x30, &[0xAA, 0xBB][..]))
        );
    }

    /// Dials the real TSS endpoint with the production TLS config to prove
    /// the leaf sanity check accepts gs.apple.com's private-CA certificate.
    /// Requires network access; run explicitly with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires network access to gs.apple.com"]
    async fn tls_handshake_with_sanity_check_reaches_real_endpoint() {
        let client = tss_http_client().expect("tss http client builds");
        let body = plist_to_xml_bytes(&plist::Dictionary::new());
        // Any HTTP status proves the handshake passed the sanity check; the
        // empty request may legitimately be rejected by Apple afterwards.
        let response = client
            .post(TSS_CONTROLLER_ACTION_URL)
            .header("Cache-Control", "no-cache")
            .header("Content-type", "text/xml; charset=\"utf-8\"")
            .header("User-Agent", "InetURL/1.0")
            .body(body)
            .send()
            .await
            .expect("TLS handshake with gs.apple.com must pass the sanity check");
        debug!("real endpoint answered with HTTP {}", response.status());
    }
}
