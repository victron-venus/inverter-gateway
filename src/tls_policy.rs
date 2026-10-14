//! Additional exact key-strength checks; rustls still verifies trust, names and signatures.
use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, Error, SignatureScheme,
};
use std::sync::Arc;
use x509_parser::{prelude::*, public_key::PublicKey};

fn invalid() -> Error {
    Error::General("TLS certificate requires RSA >=2048 bits, P-256/P-384/P-521, or Ed25519".into())
}

/// Do not round RSA moduli to bytes: a 2047-bit modulus is below the policy floor.
pub(crate) fn strong_certificate(der: &CertificateDer<'_>) -> Result<(), Error> {
    if der.len() > 65536 {
        return Err(invalid());
    }
    let (remaining, certificate) =
        X509Certificate::from_der(der.as_ref()).map_err(|_| invalid())?;
    if !remaining.is_empty() {
        return Err(invalid());
    }
    let spki = certificate.public_key();
    let strong = match spki.parsed().map_err(|_| invalid())? {
        PublicKey::RSA(key) => {
            let modulus = key
                .modulus
                .iter()
                .skip_while(|&&b| b == 0)
                .copied()
                .collect::<Vec<_>>();
            let bits = modulus.first().map_or(0, |first| {
                modulus.len() * 8 - first.leading_zeros() as usize
            });
            bits >= 2048 && key.try_exponent().is_ok_and(|e| e >= 3 && e % 2 == 1)
        }
        PublicKey::EC(_) => spki
            .algorithm
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.as_oid().ok())
            .is_some_and(|oid| {
                matches!(
                    oid.to_id_string().as_str(),
                    "1.2.840.10045.3.1.7" | "1.3.132.0.34" | "1.3.132.0.35"
                )
            }),
        PublicKey::Unknown(_) => {
            spki.algorithm.algorithm.to_id_string() == "1.3.101.112"
                && spki.subject_public_key.data.len() == 32
        }
        _ => false,
    };
    if strong {
        Ok(())
    } else {
        Err(invalid())
    }
}

pub(crate) fn strong_pem_chain(pem: &[u8]) -> Result<(), Error> {
    let mut count = 0;
    for certificate in CertificateDer::pem_slice_iter(pem) {
        count += 1;
        if count > 16 {
            return Err(invalid());
        }
        strong_certificate(&certificate.map_err(|_| invalid())?)?;
    }
    if count == 0 {
        Err(invalid())
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct StrongServerVerifier(pub Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for StrongServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // Obtain the ordinary verifier's proof first; never manufacture a trust assertion.
        let verified = self.0.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        if intermediates.len() > 15 {
            return Err(invalid());
        }
        strong_certificate(end_entity)?;
        for certificate in intermediates {
            strong_certificate(certificate)?;
        }
        Ok(verified)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_trust_success_does_not_override_exact_strength_floor() {
        let root =
            CertificateDer::from_pem_slice(include_bytes!("../tests/fixtures/rsa-2048-cert.pem"))
                .unwrap();
        let leaf =
            CertificateDer::from_pem_slice(include_bytes!("../tests/fixtures/rsa-2047-leaf.pem"))
                .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(root).unwrap();
        let normal = WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        let now = UnixTime::now();
        assert!(normal
            .verify_server_cert(&leaf, &[], &name, &[], now)
            .is_ok());
        assert!(StrongServerVerifier(normal)
            .verify_server_cert(&leaf, &[], &name, &[], now)
            .is_err());
    }

    #[test]
    fn exact_rsa_boundary_and_malformed_certificates() {
        for (pem, valid) in [
            (
                include_bytes!("../tests/fixtures/rsa-1024-cert.pem").as_slice(),
                false,
            ),
            (
                include_bytes!("../tests/fixtures/rsa-2047-cert.pem").as_slice(),
                false,
            ),
            (
                include_bytes!("../tests/fixtures/rsa-2048-cert.pem").as_slice(),
                true,
            ),
        ] {
            assert_eq!(strong_pem_chain(pem).is_ok(), valid);
        }
        assert!(strong_pem_chain(b"not a certificate").is_err());
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        assert!(strong_certificate(certificate.cert.der()).is_ok());
        let mut trailing = certificate.cert.der().to_vec();
        trailing.push(0);
        assert!(strong_certificate(&CertificateDer::from(trailing)).is_err());
        assert!(strong_pem_chain(certificate.cert.pem().repeat(17).as_bytes()).is_err());
    }
}
