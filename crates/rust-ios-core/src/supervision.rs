use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use p12_keystore::{Certificate, KeyStore, KeyStoreEntry, PrivateKeyChain};
use rand_08::RngCore;
use rsa::pkcs1::{
    DecodeRsaPrivateKey, DecodeRsaPublicKey, EncodeRsaPrivateKey, EncodeRsaPublicKey,
};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use rsa::signature::{SignatureEncoding, Signer};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha1::{Digest, Sha1};
use sha2::Sha512;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum SupervisionError {
    #[error("invalid supervision identity: {0}")]
    InvalidIdentity(String),
    #[error("supervision cryptography failed: {0}")]
    Crypto(String),
}

pub(crate) struct GeneratedSupervisionIdentity {
    pub certificate_der: Vec<u8>,
    pub certificate_pem: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub private_key_pem: Vec<u8>,
    pub csr_pem: Vec<u8>,
    pub pkcs12_der: Vec<u8>,
}

pub(crate) struct SignedSupervisionChallenge {
    pub signed_request: Vec<u8>,
}

pub(crate) struct GeneratedPairingMaterial {
    pub device_certificate_pem: Vec<u8>,
    pub host_certificate_pem: Vec<u8>,
    pub host_private_key_pem: Vec<u8>,
    pub root_certificate_pem: Vec<u8>,
    pub root_private_key_pem: Vec<u8>,
    pub host_id: String,
    pub system_buid: String,
}

pub(crate) fn generate_identity(
    password: &str,
) -> Result<GeneratedSupervisionIdentity, SupervisionError> {
    let mut rng = rand_08::rngs::OsRng;
    let key = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|error| SupervisionError::Crypto(format!("RSA key generation: {error}")))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let certificate_der = build_supervision_certificate(
        &key.to_public_key(),
        &key,
        now.saturating_sub(600),
        now.saturating_add(10 * 365 * 24 * 60 * 60),
        &mut rng,
    )?;
    let csr_der = build_certificate_request(&key.to_public_key(), &key)?;
    let private_key = key
        .to_pkcs1_der()
        .map_err(|error| SupervisionError::Crypto(format!("encode private key: {error}")))?;
    let private_key_der = private_key.as_bytes().to_vec();
    let private_key_pkcs8 = key
        .to_pkcs8_der()
        .map_err(|error| SupervisionError::Crypto(format!("encode PKCS#8 key: {error}")))?;
    let certificate = Certificate::from_der(&certificate_der).map_err(|error| {
        SupervisionError::Crypto(format!("parse generated certificate: {error}"))
    })?;
    let local_key_id = Sha1::digest(&certificate_der).to_vec();
    let mut store = KeyStore::new();
    store.add_entry(
        "rust-ios supervision",
        KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(
            private_key_pkcs8.as_bytes(),
            local_key_id,
            [certificate],
        )),
    );
    let pkcs12_der = store
        .writer(password)
        .write()
        .map_err(|error| SupervisionError::Crypto(format!("encode PKCS#12: {error}")))?;

    Ok(GeneratedSupervisionIdentity {
        certificate_pem: pem_encode("CERTIFICATE", &certificate_der),
        private_key_pem: pem_encode("RSA PRIVATE KEY", &private_key_der),
        csr_pem: pem_encode("CERTIFICATE REQUEST", &csr_der),
        certificate_der,
        private_key_der,
        pkcs12_der,
    })
}

pub(crate) fn certificate_from_identity(
    bytes: &[u8],
    password: &str,
) -> Result<Vec<u8>, SupervisionError> {
    if Certificate::from_der(bytes).is_ok() {
        return Ok(bytes.to_vec());
    }
    if let Some(der) = decode_pem_block(bytes, "CERTIFICATE")
        && Certificate::from_der(&der).is_ok()
    {
        return Ok(der);
    }
    parse_pkcs12(bytes, password).map(|(certificate, _)| certificate)
}

pub(crate) fn sign_pkcs12_challenge(
    p12_bytes: &[u8],
    password: &str,
    challenge: &[u8],
) -> Result<SignedSupervisionChallenge, SupervisionError> {
    let (certificate_der, key) = parse_pkcs12(p12_bytes, password)?;
    let signed_request = build_pkcs7_signed_data(challenge, &certificate_der, &key)?;
    Ok(SignedSupervisionChallenge { signed_request })
}

pub(crate) fn generate_pairing_material(
    device_public_key_pem: &[u8],
    system_buid: &str,
) -> Result<GeneratedPairingMaterial, SupervisionError> {
    let device_public_key_text = std::str::from_utf8(device_public_key_pem).map_err(|error| {
        SupervisionError::Crypto(format!("device public key is not PEM UTF-8: {error}"))
    })?;
    let device_public_key = match RsaPublicKey::from_pkcs1_pem(device_public_key_text) {
        Ok(key) => key,
        Err(pkcs1_error) => {
            RsaPublicKey::from_public_key_pem(device_public_key_text).map_err(|pkcs8_error| {
                SupervisionError::Crypto(format!(
                    "cannot parse device RSA public key: PKCS#1 {pkcs1_error}; SPKI {pkcs8_error}"
                ))
            })?
        }
    };

    let mut rng = rand_08::rngs::OsRng;
    let root_key = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|error| SupervisionError::Crypto(format!("generate root RSA key: {error}")))?;
    let host_key = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|error| SupervisionError::Crypto(format!("generate host RSA key: {error}")))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let not_before = now.saturating_sub(60);
    let not_after = now.saturating_add(10 * 365 * 24 * 60 * 60);
    let root_certificate = build_pairing_certificate(
        &root_key.to_public_key(),
        &root_key,
        not_before,
        not_after,
        true,
    )?;
    let host_certificate = build_pairing_certificate(
        &host_key.to_public_key(),
        &root_key,
        not_before,
        not_after,
        false,
    )?;
    let device_certificate =
        build_pairing_certificate(&device_public_key, &root_key, not_before, not_after, false)?;
    let root_private_key = root_key
        .to_pkcs1_der()
        .map_err(|error| SupervisionError::Crypto(format!("encode root key: {error}")))?;
    let host_private_key = host_key
        .to_pkcs1_der()
        .map_err(|error| SupervisionError::Crypto(format!("encode host key: {error}")))?;

    Ok(GeneratedPairingMaterial {
        device_certificate_pem: pem_encode("CERTIFICATE", &device_certificate),
        host_certificate_pem: pem_encode("CERTIFICATE", &host_certificate),
        host_private_key_pem: pem_encode("RSA PRIVATE KEY", host_private_key.as_bytes()),
        root_certificate_pem: pem_encode("CERTIFICATE", &root_certificate),
        root_private_key_pem: pem_encode("RSA PRIVATE KEY", root_private_key.as_bytes()),
        host_id: uuid::Uuid::new_v4().to_string().to_uppercase(),
        system_buid: system_buid.to_owned(),
    })
}

fn parse_pkcs12(
    bytes: &[u8],
    password: &str,
) -> Result<(Vec<u8>, RsaPrivateKey), SupervisionError> {
    let store = KeyStore::from_pkcs12(bytes, password).map_err(|error| {
        SupervisionError::InvalidIdentity(format!("cannot decode PKCS#12: {error}"))
    })?;
    let (_, chain) = store.private_key_chain().ok_or_else(|| {
        SupervisionError::InvalidIdentity("PKCS#12 has no private key chain".into())
    })?;
    let certificate = chain.chain().first().ok_or_else(|| {
        SupervisionError::InvalidIdentity("PKCS#12 private key has no certificate".into())
    })?;
    let key = RsaPrivateKey::from_pkcs8_der(chain.key())
        .or_else(|_| RsaPrivateKey::from_pkcs1_der(chain.key()))
        .map_err(|error| {
            SupervisionError::InvalidIdentity(format!("PKCS#12 key is not RSA: {error}"))
        })?;
    Ok((certificate.as_der().to_vec(), key))
}

fn build_supervision_certificate(
    public_key: &RsaPublicKey,
    signing_key: &RsaPrivateKey,
    not_before: u64,
    not_after: u64,
    rng: &mut impl RngCore,
) -> Result<Vec<u8>, SupervisionError> {
    let spki = public_key
        .to_public_key_der()
        .map_err(|error| SupervisionError::Crypto(format!("encode public key: {error}")))?;
    let mut serial = [0_u8; 20];
    rng.fill_bytes(&mut serial);
    serial[0] &= 0x7f;
    if serial.iter().all(|byte| *byte == 0) {
        serial[19] = 1;
    }
    let subject_key_id: [u8; 20] = Sha1::digest(spki.as_bytes()).into();
    let extensions = der_sequence(&[
        der_sequence(&[
            der_oid(&[2, 5, 29, 14]),
            der_tag(0x04, &der_tag(0x04, &subject_key_id)),
        ]),
        der_sequence(&[
            der_oid(&[2, 5, 29, 19]),
            der_bool(true),
            der_tag(0x04, &der_sequence(&[der_bool(true)])),
        ]),
        der_sequence(&[
            der_oid(&[2, 5, 29, 15]),
            der_bool(true),
            der_tag(0x04, &der_tag(0x03, &[0x02, 0x84])),
        ]),
        der_sequence(&[
            der_oid(&[2, 5, 29, 37]),
            der_tag(
                0x04,
                &der_sequence(&[
                    der_oid(&[1, 3, 6, 1, 5, 5, 7, 3, 1]),
                    der_oid(&[1, 3, 6, 1, 5, 5, 7, 3, 2]),
                ]),
            ),
        ]),
    ]);
    let tbs = der_sequence(&[
        der_tag(0xa0, &der_integer(&[2])),
        der_integer(&serial),
        sha512_with_rsa_algorithm(),
        der_sequence(&[]),
        der_sequence(&[der_utc_time(not_before), der_utc_time(not_after)]),
        der_sequence(&[]),
        spki.as_bytes().to_vec(),
        der_tag(0xa3, &extensions),
    ]);
    let signing = rsa::pkcs1v15::SigningKey::<Sha512>::new(signing_key.clone());
    let signature = signing.sign(&tbs).to_bytes();
    let mut bit_string = Vec::with_capacity(signature.len() + 1);
    bit_string.push(0);
    bit_string.extend_from_slice(&signature);
    Ok(der_sequence(&[
        tbs,
        sha512_with_rsa_algorithm(),
        der_tag(0x03, &bit_string),
    ]))
}

fn build_pairing_certificate(
    public_key: &RsaPublicKey,
    signing_key: &RsaPrivateKey,
    not_before: u64,
    not_after: u64,
    is_ca: bool,
) -> Result<Vec<u8>, SupervisionError> {
    let spki = public_key
        .to_public_key_der()
        .map_err(|error| SupervisionError::Crypto(format!("encode pairing public key: {error}")))?;
    let public_key_pkcs1 = public_key.to_pkcs1_der().map_err(|error| {
        SupervisionError::Crypto(format!("encode pairing PKCS#1 public key: {error}"))
    })?;
    let subject_key_id: [u8; 20] = Sha1::digest(public_key_pkcs1.as_bytes()).into();
    let subject_key_identifier = der_sequence(&[
        der_oid(&[2, 5, 29, 14]),
        der_tag(0x04, &der_tag(0x04, &subject_key_id)),
    ]);
    let basic_constraints = der_sequence(&[
        der_oid(&[2, 5, 29, 19]),
        der_bool(true),
        der_tag(
            0x04,
            &if is_ca {
                der_sequence(&[der_bool(true)])
            } else {
                der_sequence(&[])
            },
        ),
    ]);
    let mut extension_items = vec![basic_constraints, subject_key_identifier];
    if !is_ca {
        extension_items.push(der_sequence(&[
            der_oid(&[2, 5, 29, 15]),
            der_bool(true),
            der_tag(0x04, &der_tag(0x03, &[0x02, 0x84])),
        ]));
    }
    let extensions = der_sequence(&extension_items);
    let tbs = der_sequence(&[
        der_tag(0xa0, &der_integer(&[2])),
        der_integer(&[0]),
        sha1_with_rsa_algorithm(),
        der_sequence(&[]),
        der_sequence(&[der_utc_time(not_before), der_utc_time(not_after)]),
        der_sequence(&[]),
        spki.as_bytes().to_vec(),
        der_tag(0xa3, &extensions),
    ]);
    let signing = rsa::pkcs1v15::SigningKey::<Sha1>::new(signing_key.clone());
    let signature = signing.sign(&tbs).to_bytes();
    let mut bit_string = Vec::with_capacity(signature.len() + 1);
    bit_string.push(0);
    bit_string.extend_from_slice(&signature);
    Ok(der_sequence(&[
        tbs,
        sha1_with_rsa_algorithm(),
        der_tag(0x03, &bit_string),
    ]))
}

fn build_certificate_request(
    public_key: &RsaPublicKey,
    signing_key: &RsaPrivateKey,
) -> Result<Vec<u8>, SupervisionError> {
    let spki = public_key
        .to_public_key_der()
        .map_err(|error| SupervisionError::Crypto(format!("encode CSR public key: {error}")))?;
    let request_info = der_sequence(&[
        der_integer(&[0]),
        der_sequence(&[]),
        spki.as_bytes().to_vec(),
        der_tag(0xa0, &[]),
    ]);
    let signing = rsa::pkcs1v15::SigningKey::<Sha512>::new(signing_key.clone());
    let signature = signing.sign(&request_info).to_bytes();
    let mut bit_string = Vec::with_capacity(signature.len() + 1);
    bit_string.push(0);
    bit_string.extend_from_slice(&signature);
    Ok(der_sequence(&[
        request_info,
        sha512_with_rsa_algorithm(),
        der_tag(0x03, &bit_string),
    ]))
}

fn build_pkcs7_signed_data(
    content: &[u8],
    certificate_der: &[u8],
    key: &RsaPrivateKey,
) -> Result<Vec<u8>, SupervisionError> {
    let (issuer, serial) = extract_issuer_and_serial(certificate_der)?;
    let digest: [u8; 20] = Sha1::digest(content).into();
    let content_type = der_sequence(&[
        der_oid(&[1, 2, 840, 113549, 1, 9, 3]),
        der_tag(0x31, &der_oid(&[1, 2, 840, 113549, 1, 7, 1])),
    ]);
    let message_digest = der_sequence(&[
        der_oid(&[1, 2, 840, 113549, 1, 9, 4]),
        der_tag(0x31, &der_tag(0x04, &digest)),
    ]);
    let mut attributes = content_type;
    attributes.extend_from_slice(&message_digest);
    let signing = rsa::pkcs1v15::SigningKey::<Sha1>::new(key.clone());
    let signature = signing.sign(&der_tag(0x31, &attributes)).to_bytes();
    let signer_info = der_sequence(&[
        der_integer(&[1]),
        der_sequence(&[issuer, serial]),
        sha1_algorithm(),
        der_tag(0xa0, &attributes),
        sha1_with_rsa_algorithm(),
        der_tag(0x04, &signature),
    ]);
    let signed_data = der_sequence(&[
        der_integer(&[1]),
        der_tag(0x31, &sha1_algorithm()),
        der_sequence(&[
            der_oid(&[1, 2, 840, 113549, 1, 7, 1]),
            der_tag(0xa0, &der_tag(0x04, content)),
        ]),
        der_tag(0xa0, certificate_der),
        der_tag(0x31, &signer_info),
    ]);
    Ok(der_sequence(&[
        der_oid(&[1, 2, 840, 113549, 1, 7, 2]),
        der_tag(0xa0, &signed_data),
    ]))
}

fn extract_issuer_and_serial(
    certificate_der: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), SupervisionError> {
    let (_, certificate, _) = take_tlv(certificate_der, Some(0x30)).ok_or_else(|| {
        SupervisionError::InvalidIdentity("certificate outer sequence is invalid".into())
    })?;
    let (_, tbs, _) = take_tlv(certificate, Some(0x30)).ok_or_else(|| {
        SupervisionError::InvalidIdentity("certificate TBSCertificate is invalid".into())
    })?;
    let mut remaining = tbs;
    if remaining.first() == Some(&0xa0) {
        let (_, _, rest) = take_tlv(remaining, Some(0xa0)).ok_or_else(|| {
            SupervisionError::InvalidIdentity("certificate version is invalid".into())
        })?;
        remaining = rest;
    }
    let (serial, _, rest) = take_tlv(remaining, Some(0x02))
        .ok_or_else(|| SupervisionError::InvalidIdentity("certificate serial is invalid".into()))?;
    let (_, _, rest) = take_tlv(rest, Some(0x30)).ok_or_else(|| {
        SupervisionError::InvalidIdentity("certificate signature algorithm is invalid".into())
    })?;
    let (issuer, _, _) = take_tlv(rest, Some(0x30))
        .ok_or_else(|| SupervisionError::InvalidIdentity("certificate issuer is invalid".into()))?;
    Ok((issuer.to_vec(), serial.to_vec()))
}

fn der_tag(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(content.len() + 8);
    result.push(tag);
    if content.len() < 0x80 {
        result.push(content.len() as u8);
    } else {
        let length_bytes = content.len().to_be_bytes();
        let first = length_bytes
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(length_bytes.len() - 1);
        let significant = &length_bytes[first..];
        result.push(0x80 | significant.len() as u8);
        result.extend_from_slice(significant);
    }
    result.extend_from_slice(content);
    result
}

fn der_sequence(items: &[Vec<u8>]) -> Vec<u8> {
    let body = items.iter().flatten().copied().collect::<Vec<_>>();
    der_tag(0x30, &body)
}

fn der_integer(value: &[u8]) -> Vec<u8> {
    let first_nonzero = value
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(value.len().saturating_sub(1));
    let value = &value[first_nonzero..];
    let mut body = Vec::with_capacity(value.len() + 1);
    if value.first().is_some_and(|byte| byte & 0x80 != 0) {
        body.push(0);
    }
    body.extend_from_slice(value);
    der_tag(0x02, &body)
}

fn der_bool(value: bool) -> Vec<u8> {
    der_tag(0x01, &[if value { 0xff } else { 0 }])
}

fn der_oid(arcs: &[u32]) -> Vec<u8> {
    let mut body = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for arc in &arcs[2..] {
        let mut encoded = [0_u8; 5];
        let mut index = encoded.len() - 1;
        let mut value = *arc;
        encoded[index] = (value & 0x7f) as u8;
        value >>= 7;
        while value != 0 {
            index -= 1;
            encoded[index] = ((value & 0x7f) as u8) | 0x80;
            value >>= 7;
        }
        body.extend_from_slice(&encoded[index..]);
    }
    der_tag(0x06, &body)
}

fn der_utc_time(timestamp: u64) -> Vec<u8> {
    let days = timestamp / 86_400;
    let seconds = timestamp % 86_400;
    let (year, month, day) = days_to_ymd(days);
    let value = format!(
        "{:02}{month:02}{day:02}{:02}{:02}{:02}Z",
        year % 100,
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    );
    der_tag(0x17, value.as_bytes())
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970;
    loop {
        let length = if is_leap_year(year) { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let month_lengths = [
        31,
        if is_leap_year(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for length in month_lengths {
        if days < length {
            break;
        }
        days -= length;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap_year(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn sha1_algorithm() -> Vec<u8> {
    der_sequence(&[der_oid(&[1, 3, 14, 3, 2, 26]), vec![0x05, 0x00]])
}

fn sha1_with_rsa_algorithm() -> Vec<u8> {
    der_sequence(&[der_oid(&[1, 2, 840, 113549, 1, 1, 5]), vec![0x05, 0x00]])
}

fn sha512_with_rsa_algorithm() -> Vec<u8> {
    der_sequence(&[der_oid(&[1, 2, 840, 113549, 1, 1, 13]), vec![0x05, 0x00]])
}

fn take_tlv(input: &[u8], expected_tag: Option<u8>) -> Option<(&[u8], &[u8], &[u8])> {
    let tag = *input.first()?;
    if expected_tag.is_some_and(|expected| expected != tag) {
        return None;
    }
    let first_length = *input.get(1)?;
    let (length, header_length) = if first_length & 0x80 == 0 {
        (first_length as usize, 2)
    } else {
        let count = (first_length & 0x7f) as usize;
        if count == 0 || count > std::mem::size_of::<usize>() || input.len() < count + 2 {
            return None;
        }
        let mut length = 0_usize;
        for byte in &input[2..2 + count] {
            length = length.checked_shl(8)?.checked_add(*byte as usize)?;
        }
        (length, count + 2)
    };
    let end = header_length.checked_add(length)?;
    if end > input.len() {
        return None;
    }
    Some((&input[..end], &input[header_length..end], &input[end..]))
}

fn pem_encode(label: &str, bytes: &[u8]) -> Vec<u8> {
    let encoded = BASE64.encode(bytes);
    let mut output = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        output.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        output.push('\n');
    }
    output.push_str(&format!("-----END {label}-----\n"));
    output.into_bytes()
}

fn decode_pem_block(bytes: &[u8], label: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let start_marker = format!("-----BEGIN {label}-----");
    let end_marker = format!("-----END {label}-----");
    let start = text.find(&start_marker)? + start_marker.len();
    let end = text[start..].find(&end_marker)? + start;
    let encoded = text[start..end]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    BASE64.decode(encoded).ok()
}

#[cfg(test)]
mod tests {
    use rsa::signature::Verifier;

    use super::*;

    #[test]
    fn generated_identity_round_trips_and_signs_challenge() {
        let identity = generate_identity("test-password").unwrap();
        let signed =
            sign_pkcs12_challenge(&identity.pkcs12_der, "test-password", b"test challenge")
                .unwrap();
        assert_eq!(signed.signed_request.first(), Some(&0x30));
        assert!(signed.signed_request.len() > identity.certificate_der.len());
        assert!(
            identity
                .certificate_pem
                .starts_with(b"-----BEGIN CERTIFICATE-----")
        );
        assert!(
            identity
                .private_key_pem
                .starts_with(b"-----BEGIN RSA PRIVATE KEY-----")
        );
        assert!(
            identity
                .csr_pem
                .starts_with(b"-----BEGIN CERTIFICATE REQUEST-----")
        );
    }

    #[test]
    fn certificate_loader_accepts_der_pem_and_pkcs12() {
        let identity = generate_identity("secret").unwrap();
        assert_eq!(
            certificate_from_identity(&identity.certificate_der, "").unwrap(),
            identity.certificate_der
        );
        assert_eq!(
            certificate_from_identity(&identity.certificate_pem, "").unwrap(),
            identity.certificate_der
        );
        assert_eq!(
            certificate_from_identity(&identity.pkcs12_der, "secret").unwrap(),
            identity.certificate_der
        );
    }

    #[test]
    fn generated_pairing_material_contains_parseable_certs_and_distinct_keys() {
        let mut rng = rand_08::rngs::OsRng;
        let device_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let device_public_der = device_key.to_public_key().to_pkcs1_der().unwrap();
        let device_public_pem = pem_encode("RSA PUBLIC KEY", device_public_der.as_bytes());
        let material = generate_pairing_material(&device_public_pem, "TEST-BUID").unwrap();

        for certificate in [
            &material.device_certificate_pem,
            &material.host_certificate_pem,
            &material.root_certificate_pem,
        ] {
            let der = decode_pem_block(certificate, "CERTIFICATE").unwrap();
            x509_certificate::CapturedX509Certificate::from_der(der).unwrap();
        }
        let host_key = RsaPrivateKey::from_pkcs1_pem(
            std::str::from_utf8(&material.host_private_key_pem).unwrap(),
        )
        .unwrap();
        let root_key = RsaPrivateKey::from_pkcs1_pem(
            std::str::from_utf8(&material.root_private_key_pem).unwrap(),
        )
        .unwrap();
        assert_ne!(host_key.to_public_key(), root_key.to_public_key());
        for certificate in [
            &material.root_certificate_pem,
            &material.host_certificate_pem,
            &material.device_certificate_pem,
        ] {
            assert_certificate_signed_by(certificate, &root_key);
        }
        assert_eq!(material.system_buid, "TEST-BUID");
        assert_eq!(material.host_id.len(), 36);
    }

    fn assert_certificate_signed_by(certificate_pem: &[u8], key: &RsaPrivateKey) {
        let certificate = decode_pem_block(certificate_pem, "CERTIFICATE").unwrap();
        let (_, certificate, rest) = take_tlv(&certificate, Some(0x30)).unwrap();
        assert!(rest.is_empty());
        let (signed_data, _, remaining) = take_tlv(certificate, Some(0x30)).unwrap();
        let (_, _, remaining) = take_tlv(remaining, Some(0x30)).unwrap();
        let (_, signature, remaining) = take_tlv(remaining, Some(0x03)).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(signature.first(), Some(&0));
        let signature = rsa::pkcs1v15::Signature::try_from(&signature[1..]).unwrap();
        rsa::pkcs1v15::VerifyingKey::<Sha1>::new(key.to_public_key())
            .verify(signed_data, &signature)
            .unwrap();
    }
}
