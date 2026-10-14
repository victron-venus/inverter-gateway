//! Gateway services and pure input validators shared by the binary and tests.
pub mod alarms;
pub mod config;
pub mod energy;
pub mod http;
pub mod mqtt_bridge;
pub mod state;
mod tls_policy;
pub mod transport;
pub mod whitelist;

/// Offline test entry points. These check key strength, never certificate trust.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    /// Parse one complete DER certificate and apply the additional strength policy.
    pub fn strong_der(der: &[u8]) -> bool {
        crate::tls_policy::strong_certificate(&rustls::pki_types::CertificateDer::from(der)).is_ok()
    }

    /// Parse a PEM chain and apply the additional strength and chain-length policy.
    pub fn strong_pem(pem: &[u8]) -> bool {
        crate::tls_policy::strong_pem_chain(pem).is_ok()
    }
}
