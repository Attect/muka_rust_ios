use idevice::pairing_file::PairingFile;
use ios_device_core::{GetValueRequest, recv_lockdown, send_lockdown};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::supervision::{
    GeneratedPairingMaterial, certificate_from_identity, generate_pairing_material,
    sign_pkcs12_challenge,
};
use crate::{CoreError, CoreResult};

pub(crate) struct SupervisedPairResult {
    pub(crate) pair_record: Vec<u8>,
    pub(crate) host_id: String,
}

pub(crate) async fn pair_supervised<S>(
    stream: &mut S,
    p12_bytes: &[u8],
    password: &str,
    system_buid: &str,
    udid: &str,
) -> CoreResult<SupervisedPairResult>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let supervisor_certificate = certificate_from_identity(p12_bytes, password)
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    let device_public_key = lockdown_value(stream, "DevicePublicKey")
        .await?
        .into_data()
        .ok_or_else(|| {
            CoreError::PairingRequired("Lockdown DevicePublicKey was not data".into())
        })?;
    let wifi_mac = lockdown_value(stream, "WiFiAddress")
        .await?
        .into_string()
        .ok_or_else(|| {
            CoreError::PairingRequired("Lockdown WiFiAddress was not a string".into())
        })?;
    let material = generate_pairing_material(&device_public_key, system_buid)
        .map_err(|error| CoreError::PairingRequired(error.to_string()))?;

    let first_request = pair_request(
        &material,
        plist::Dictionary::from_iter([
            (
                "ExtendedPairingErrors".to_owned(),
                plist::Value::Boolean(true),
            ),
            (
                "SupervisorCertificate".to_owned(),
                plist::Value::Data(supervisor_certificate),
            ),
        ]),
    );
    send_lockdown(stream, &plist::Value::Dictionary(first_request))
        .await
        .map_err(pair_transport_error)?;
    let challenge = pairing_challenge(recv_value(stream).await?)?;
    let challenge_response = sign_pkcs12_challenge(p12_bytes, password, &challenge)
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?
        .signed_request;

    let second_request = pair_request(
        &material,
        plist::Dictionary::from_iter([(
            "ChallengeResponse".to_owned(),
            plist::Value::Data(challenge_response),
        )]),
    );
    send_lockdown(stream, &plist::Value::Dictionary(second_request))
        .await
        .map_err(pair_transport_error)?;
    let escrow_bag = pairing_escrow_bag(recv_value(stream).await?)?;
    let host_id = material.host_id.clone();
    let pair_record = final_pair_record(material, escrow_bag, wifi_mac, udid);
    let pair_record = PairingFile::from_value(&plist::Value::Dictionary(pair_record))
        .map_err(|error| CoreError::PairingRequired(error.to_string()))?
        .serialize()
        .map_err(|error| CoreError::PairingRequired(error.to_string()))?;
    Ok(SupervisedPairResult {
        pair_record,
        host_id,
    })
}

async fn lockdown_value<S>(stream: &mut S, key: &str) -> CoreResult<plist::Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = GetValueRequest {
        label: "rust-ios",
        request: "GetValue",
        domain: None,
        key: Some(key),
    };
    send_lockdown(stream, &request)
        .await
        .map_err(pair_transport_error)?;
    let response = recv_value(stream).await?;
    let response = response.as_dictionary().ok_or_else(|| {
        CoreError::PairingRequired(format!(
            "Lockdown GetValue {key} response was not a dictionary"
        ))
    })?;
    if let Some(error) = response.get("Error").and_then(plist::Value::as_string) {
        return Err(CoreError::PairingRequired(format!(
            "Lockdown GetValue {key} failed: {error}"
        )));
    }
    response.get("Value").cloned().ok_or_else(|| {
        CoreError::PairingRequired(format!("Lockdown GetValue {key} response missing Value"))
    })
}

async fn recv_value<S>(stream: &mut S) -> CoreResult<plist::Value>
where
    S: AsyncRead + Unpin,
{
    recv_lockdown(stream).await.map_err(pair_transport_error)
}

fn pair_request(
    material: &GeneratedPairingMaterial,
    pairing_options: plist::Dictionary,
) -> plist::Dictionary {
    plist::Dictionary::from_iter([
        ("Label".to_owned(), plist::Value::String("rust-ios".into())),
        (
            "ProtocolVersion".to_owned(),
            plist::Value::String("2".into()),
        ),
        ("Request".to_owned(), plist::Value::String("Pair".into())),
        (
            "PairRecord".to_owned(),
            plist::Value::Dictionary(pair_record_payload(material)),
        ),
        (
            "PairingOptions".to_owned(),
            plist::Value::Dictionary(pairing_options),
        ),
    ])
}

fn pair_record_payload(material: &GeneratedPairingMaterial) -> plist::Dictionary {
    plist::Dictionary::from_iter([
        (
            "DeviceCertificate".to_owned(),
            plist::Value::Data(material.device_certificate_pem.clone()),
        ),
        (
            "HostCertificate".to_owned(),
            plist::Value::Data(material.host_certificate_pem.clone()),
        ),
        (
            "RootCertificate".to_owned(),
            plist::Value::Data(material.root_certificate_pem.clone()),
        ),
        (
            "HostID".to_owned(),
            plist::Value::String(material.host_id.clone()),
        ),
        (
            "SystemBUID".to_owned(),
            plist::Value::String(material.system_buid.clone()),
        ),
    ])
}

fn final_pair_record(
    material: GeneratedPairingMaterial,
    escrow_bag: Vec<u8>,
    wifi_mac: String,
    udid: &str,
) -> plist::Dictionary {
    let mut record = pair_record_payload(&material);
    record.insert(
        "HostPrivateKey".into(),
        plist::Value::Data(material.host_private_key_pem),
    );
    record.insert(
        "RootPrivateKey".into(),
        plist::Value::Data(material.root_private_key_pem),
    );
    record.insert("EscrowBag".into(), plist::Value::Data(escrow_bag));
    record.insert("WiFiMACAddress".into(), plist::Value::String(wifi_mac));
    record.insert("UDID".into(), plist::Value::String(udid.into()));
    record
}

fn pairing_challenge(response: plist::Value) -> CoreResult<Vec<u8>> {
    let response = response.as_dictionary().ok_or_else(|| {
        CoreError::PairingRequired("supervised Pair challenge response was not a dictionary".into())
    })?;
    let error = response
        .get("Error")
        .and_then(plist::Value::as_string)
        .ok_or_else(|| {
            CoreError::PairingRequired("supervised Pair challenge response missing Error".into())
        })?;
    if error != "MCChallengeRequired" {
        return Err(CoreError::PairingRequired(format!(
            "supervised Pair expected MCChallengeRequired, got {error}"
        )));
    }
    response
        .get("ExtendedResponse")
        .and_then(plist::Value::as_dictionary)
        .and_then(|value| value.get("PairingChallenge"))
        .and_then(plist::Value::as_data)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            CoreError::PairingRequired(
                "supervised Pair response missing ExtendedResponse.PairingChallenge".into(),
            )
        })
}

fn pairing_escrow_bag(response: plist::Value) -> CoreResult<Vec<u8>> {
    let response = response.as_dictionary().ok_or_else(|| {
        CoreError::PairingRequired("supervised Pair final response was not a dictionary".into())
    })?;
    if let Some(error) = response.get("Error").and_then(plist::Value::as_string) {
        return Err(CoreError::PairingRequired(format!(
            "supervised Pair failed: {error}"
        )));
    }
    response
        .get("EscrowBag")
        .and_then(plist::Value::as_data)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            CoreError::PairingRequired("supervised Pair response missing EscrowBag".into())
        })
}

fn pair_transport_error(error: ios_device_core::LockdownError) -> CoreError {
    CoreError::PairingRequired(error.to_string())
}

#[cfg(test)]
mod tests {
    use rsa::RsaPrivateKey;
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs8::LineEnding;

    use super::*;
    use crate::supervision::generate_identity;

    #[tokio::test]
    async fn supervised_pair_signs_challenge_without_sending_private_keys() {
        let identity = generate_identity("secret").unwrap();
        let mut rng = rand_08::rngs::OsRng;
        let device_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let device_public_pem = device_key
            .to_public_key()
            .to_pkcs1_pem(LineEnding::LF)
            .unwrap()
            .into_bytes();
        let (mut client, mut device) = tokio::io::duplex(256 * 1024);
        let server = tokio::spawn(async move {
            let public_key_request: plist::Value = recv_lockdown(&mut device).await.unwrap();
            assert_eq!(
                public_key_request
                    .as_dictionary()
                    .and_then(|value| value.get("Key"))
                    .and_then(plist::Value::as_string),
                Some("DevicePublicKey")
            );
            send_lockdown(
                &mut device,
                &plist::Value::Dictionary(plist::Dictionary::from_iter([(
                    "Value".to_owned(),
                    plist::Value::Data(device_public_pem),
                )])),
            )
            .await
            .unwrap();

            let wifi_request: plist::Value = recv_lockdown(&mut device).await.unwrap();
            assert_eq!(
                wifi_request
                    .as_dictionary()
                    .and_then(|value| value.get("Key"))
                    .and_then(plist::Value::as_string),
                Some("WiFiAddress")
            );
            send_lockdown(
                &mut device,
                &plist::Value::Dictionary(plist::Dictionary::from_iter([(
                    "Value".to_owned(),
                    plist::Value::String("02:00:00:00:00:01".into()),
                )])),
            )
            .await
            .unwrap();

            let first_pair: plist::Value = recv_lockdown(&mut device).await.unwrap();
            assert_pair_request_has_no_private_keys(&first_pair);
            let first_options = first_pair
                .as_dictionary()
                .and_then(|value| value.get("PairingOptions"))
                .and_then(plist::Value::as_dictionary)
                .unwrap();
            assert_eq!(
                first_options
                    .get("ExtendedPairingErrors")
                    .and_then(plist::Value::as_boolean),
                Some(true)
            );
            assert!(
                first_options
                    .get("SupervisorCertificate")
                    .and_then(plist::Value::as_data)
                    .is_some_and(|value| !value.is_empty())
            );
            send_lockdown(
                &mut device,
                &plist::Value::Dictionary(plist::Dictionary::from_iter([
                    (
                        "Error".to_owned(),
                        plist::Value::String("MCChallengeRequired".into()),
                    ),
                    (
                        "ExtendedResponse".to_owned(),
                        plist::Value::Dictionary(plist::Dictionary::from_iter([(
                            "PairingChallenge".to_owned(),
                            plist::Value::Data(b"device challenge".to_vec()),
                        )])),
                    ),
                ])),
            )
            .await
            .unwrap();

            let second_pair: plist::Value = recv_lockdown(&mut device).await.unwrap();
            assert_pair_request_has_no_private_keys(&second_pair);
            assert!(
                second_pair
                    .as_dictionary()
                    .and_then(|value| value.get("PairingOptions"))
                    .and_then(plist::Value::as_dictionary)
                    .and_then(|value| value.get("ChallengeResponse"))
                    .and_then(plist::Value::as_data)
                    .is_some_and(|value| value.first() == Some(&0x30))
            );
            send_lockdown(
                &mut device,
                &plist::Value::Dictionary(plist::Dictionary::from_iter([(
                    "EscrowBag".to_owned(),
                    plist::Value::Data(b"escrow".to_vec()),
                )])),
            )
            .await
            .unwrap();
        });

        let result = pair_supervised(
            &mut client,
            &identity.pkcs12_der,
            "secret",
            "TEST-BUID",
            "test-device",
        )
        .await
        .unwrap();
        server.await.unwrap();
        let record = PairingFile::from_bytes(&result.pair_record).unwrap();
        assert_eq!(record.host_id, result.host_id);
        assert_eq!(record.system_buid, "TEST-BUID");
        assert_eq!(record.wifi_mac_address, "02:00:00:00:00:01");
        assert_eq!(record.udid.as_deref(), Some("test-device"));
        assert_eq!(record.escrow_bag.as_deref(), Some(b"escrow".as_slice()));
    }

    fn assert_pair_request_has_no_private_keys(request: &plist::Value) {
        let request = request.as_dictionary().unwrap();
        assert_eq!(
            request.get("Request").and_then(plist::Value::as_string),
            Some("Pair")
        );
        let pair_record = request
            .get("PairRecord")
            .and_then(plist::Value::as_dictionary)
            .unwrap();
        assert!(!pair_record.contains_key("HostPrivateKey"));
        assert!(!pair_record.contains_key("RootPrivateKey"));
        assert_eq!(pair_record.len(), 5);
    }
}
