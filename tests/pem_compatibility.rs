//! Exercise the patched MQTT dependency through its public connection API.
use std::{sync::Arc, time::Duration};

use rumqttc::{AsyncClient, ConnectionError, MqttOptions, TlsConfiguration, TlsError, Transport};
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::net::TcpListener;

async fn rejected_pem(ca: Vec<u8>, cert: Vec<u8>, key: Vec<u8>) -> ConnectionError {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut options = MqttOptions::new(
        "pem-rejection",
        "127.0.0.1",
        listener.local_addr().unwrap().port(),
    );
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca,
        alpn: None,
        client_auth: Some((cert, key)),
    }));
    let (_client, mut eventloop) = AsyncClient::new(options, 1);
    tokio::time::timeout(Duration::from_secs(5), eventloop.poll())
        .await
        .unwrap()
        .unwrap_err()
}

#[tokio::test]
async fn mqtt_pem_rejects_missing_and_malformed_certificates_and_keys() {
    let identity = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let cert = identity.cert.pem().into_bytes();
    let key = identity.signing_key.serialize_pem().into_bytes();
    let malformed = b"-----BEGIN CERTIFICATE-----\ninvalid!\n-----END CERTIFICATE-----\n";
    let error = rejected_pem(Vec::new(), cert.clone(), key.clone()).await;
    assert!(matches!(
        error,
        ConnectionError::Tls(TlsError::NoValidCertInChain)
    ));
    let error = rejected_pem(cert.clone(), Vec::new(), key.clone()).await;
    assert!(matches!(
        error,
        ConnectionError::Tls(TlsError::NoValidClientCertInChain)
    ));
    let error = rejected_pem(cert.clone(), cert.clone(), Vec::new()).await;
    assert!(matches!(
        error,
        ConnectionError::Tls(TlsError::NoValidKeyInChain)
    ));
    for (ca, client, key) in [
        (malformed.to_vec(), cert.clone(), key.clone()),
        (
            cert.clone(),
            [cert.as_slice(), malformed].concat(),
            key.clone(),
        ),
        (
            cert.clone(),
            cert.clone(),
            [malformed, key.as_slice()].concat(),
        ),
    ] {
        let error = rejected_pem(ca, client, key).await;
        assert!(
            matches!(error, ConnectionError::Tls(TlsError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidData),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn mqtt_pem_recognizes_each_private_key_container_before_der_validation() {
    let identity = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let cert = identity.cert.pem().into_bytes();
    // Synthetic DER is deliberately invalid: reaching Rustls validation proves
    // the PEM reader recognized the container instead of dropping the key.
    for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"] {
        let key = format!("-----BEGIN {label}-----\r\nAQID\r\n-----END {label}-----\r\n");
        let error = rejected_pem(cert.clone(), cert.clone(), key.into_bytes()).await;
        assert!(
            matches!(error, ConnectionError::Tls(TlsError::TLS(_))),
            "{label}: {error:?}"
        );
    }
}

#[tokio::test]
async fn mqtt_pem_uses_first_valid_key_and_ignores_trailing_key_sections() {
    let identity = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let cert = identity.cert.pem();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![identity.cert.der().clone()],
            PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut options = MqttOptions::new(
        "pem-first-key",
        "127.0.0.1",
        listener.local_addr().unwrap().port(),
    );
    let key = format!("-----BEGIN PUBLIC KEY-----\nAQID\n-----END PUBLIC KEY-----\n{}-----BEGIN PRIVATE KEY-----\ninvalid!\n", identity.signing_key.serialize_pem()).replace('\n', "\r\n");
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: cert.as_bytes().to_vec(),
        alpn: None,
        client_auth: Some((cert.into_bytes(), key.into_bytes())),
    }));
    let (_client, mut eventloop) = AsyncClient::new(options, 1);
    let server = async {
        let (socket, _) = listener.accept().await.unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(server_config))
            .accept(socket)
            .await
            .unwrap();
    };
    let client = async {
        let _ = eventloop.poll().await;
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, client);
    })
    .await
    .unwrap();
}
