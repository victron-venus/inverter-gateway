#[cfg(feature = "use-rustls-no-provider")]
use tokio_rustls::rustls::{
    self,
    pki_types::{pem::{self, PemObject}, CertificateDer, InvalidDnsNameError, PrivateKeyDer, ServerName},
    ClientConfig, RootCertStore,
};
#[cfg(feature = "use-rustls-no-provider")]
use tokio_rustls::TlsConnector as RustlsConnector;

#[cfg(feature = "use-rustls-no-provider")]
use std::convert::TryFrom;
#[cfg(feature = "use-rustls-no-provider")]
use std::io::Cursor;
#[cfg(feature = "use-rustls-no-provider")]
use std::sync::Arc;

use crate::framed::AsyncReadWrite;
use crate::TlsConfiguration;

#[cfg(feature = "use-native-tls")]
use tokio_native_tls::TlsConnector as NativeTlsConnector;

#[cfg(feature = "use-native-tls")]
use tokio_native_tls::native_tls::{Error as NativeTlsError, Identity};

use std::io;
use std::net::AddrParseError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error parsing IP address
    #[error("Addr")]
    Addr(#[from] AddrParseError),
    /// I/O related error
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[cfg(feature = "use-rustls-no-provider")]
    /// Certificate/Name validation error
    #[error("Web Pki: {0}")]
    WebPki(#[from] webpki::Error),
    /// Invalid DNS name
    #[cfg(feature = "use-rustls-no-provider")]
    #[error("DNS name")]
    DNSName(#[from] InvalidDnsNameError),
    #[cfg(feature = "use-rustls-no-provider")]
    /// Error from rustls module
    #[error("TLS error: {0}")]
    TLS(#[from] rustls::Error),
    #[cfg(feature = "use-rustls-no-provider")]
    /// No valid CA cert found
    #[error("No valid CA certificate provided")]
    NoValidCertInChain,
    #[cfg(feature = "use-rustls-no-provider")]
    /// No valid client cert found
    #[error("No valid certificate for client authentication in chain")]
    NoValidClientCertInChain,
    #[cfg(feature = "use-rustls-no-provider")]
    /// No valid key found
    #[error("No valid key in chain")]
    NoValidKeyInChain,
    #[cfg(feature = "use-native-tls")]
    #[error("Native TLS error {0}")]
    NativeTls(#[from] NativeTlsError),
}

// Preserve the public I/O error variant while using rustls's maintained PEM parser.
#[cfg(feature = "use-rustls-no-provider")]
fn pem_io_error(error: pem::Error) -> io::Error {
    match error {
        pem::Error::Io(error) => error,
        other => io::Error::new(io::ErrorKind::InvalidData, other),
    }
}

#[cfg(feature = "use-rustls-no-provider")]
pub async fn rustls_connector(tls_config: &TlsConfiguration) -> Result<RustlsConnector, Error> {
    let config = match tls_config {
        TlsConfiguration::Simple {
            ca,
            alpn,
            client_auth,
        } => {
            // Add ca to root store if the connection is TLS
            let mut root_cert_store = RootCertStore::empty();
            let certs = CertificateDer::pem_reader_iter(Cursor::new(ca))
                .collect::<Result<Vec<_>, _>>().map_err(pem_io_error)?;

            root_cert_store.add_parsable_certificates(certs);

            if root_cert_store.is_empty() {
                return Err(Error::NoValidCertInChain);
            }

            let config = ClientConfig::builder().with_root_certificates(root_cert_store);

            // Add der encoded client cert and key
            let mut config = if let Some(client) = client_auth.as_ref() {
                let certs =
                    CertificateDer::pem_reader_iter(Cursor::new(&client.0))
                        .collect::<Result<Vec<_>, _>>().map_err(pem_io_error)?;
                if certs.is_empty() {
                    return Err(Error::NoValidClientCertInChain);
                }

                // Preserve first-key behavior for SEC1, PKCS#1 and PKCS#8.
                // A malformed section before the key fails; trailing items are not consumed.
                let key = PrivateKeyDer::pem_reader_iter(Cursor::new(&client.1))
                    .next()
                    .transpose()
                    .map_err(pem_io_error)?
                    .ok_or(Error::NoValidKeyInChain)?;

                config.with_client_auth_cert(certs, key)?
            } else {
                config.with_no_client_auth()
            };

            // Set ALPN
            if let Some(alpn) = alpn.as_ref() {
                config.alpn_protocols.extend_from_slice(alpn);
            }

            Arc::new(config)
        }
        TlsConfiguration::Rustls(tls_client_config) => tls_client_config.clone(),
        #[allow(unreachable_patterns)]
        _ => unreachable!("This cannot be called for other TLS backends than Rustls"),
    };

    Ok(RustlsConnector::from(config))
}

#[cfg(feature = "use-native-tls")]
pub async fn native_tls_connector(
    tls_config: &TlsConfiguration,
) -> Result<NativeTlsConnector, Error> {
    let connector = match tls_config {
        TlsConfiguration::SimpleNative { ca, client_auth } => {
            let cert = native_tls::Certificate::from_pem(ca)?;

            let mut connector_builder = native_tls::TlsConnector::builder();
            connector_builder.add_root_certificate(cert);

            if let Some((der, password)) = client_auth {
                let identity = Identity::from_pkcs12(der, password)?;
                connector_builder.identity(identity);
            }

            connector_builder.build()?
        }
        TlsConfiguration::Native => native_tls::TlsConnector::new()?,
        TlsConfiguration::NativeConnector(connector) => connector.to_owned(),
        #[allow(unreachable_patterns)]
        _ => unreachable!("This cannot be called for other TLS backends than Native TLS"),
    };

    Ok(connector.into())
}

pub async fn tls_connect(
    addr: &str,
    _port: u16,
    tls_config: &TlsConfiguration,
    tcp: Box<dyn AsyncReadWrite>,
) -> Result<Box<dyn AsyncReadWrite>, Error> {
    let tls: Box<dyn AsyncReadWrite> = match tls_config {
        #[cfg(feature = "use-rustls-no-provider")]
        TlsConfiguration::Simple { .. } | TlsConfiguration::Rustls(_) => {
            let connector = rustls_connector(tls_config).await?;
            let domain = ServerName::try_from(addr)?.to_owned();
            Box::new(connector.connect(domain, tcp).await?)
        }
        #[cfg(feature = "use-native-tls")]
        TlsConfiguration::Native
        | TlsConfiguration::NativeConnector(_)
        | TlsConfiguration::SimpleNative { .. } => {
            let connector = native_tls_connector(tls_config).await?;
            Box::new(connector.connect(addr, tcp).await?)
        }
        #[allow(unreachable_patterns)]
        _ => panic!("Unknown or not enabled TLS backend configuration"),
    };
    Ok(tls)
}
