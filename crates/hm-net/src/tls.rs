//! Station certificates: a self-signed certificate carrying the station's
//! Ed25519 key and callsign, and the verifiers that check the other side's
//! against the trust list (or, on an open hub, against itself).

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use hm_ident::PublicKey;
use hm_wire::Callsign;
use quinn::TransportConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::{MAX_OBJECT, MAX_STREAMS};

const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// The Ed25519 public key in a certificate. The SubjectPublicKeyInfo of an
/// Ed25519 key has one fixed DER encoding, so it can be found without a full
/// X.509 parser.
pub fn ed25519_key_in_cert(der: &[u8]) -> Option<[u8; 32]> {
    let at = der
        .windows(ED25519_SPKI_PREFIX.len())
        .position(|w| w == ED25519_SPKI_PREFIX)?;
    der.get(at + 12..at + 44)?.try_into().ok()
}

/// A self-signed certificate carrying the station key and callsign (CN + SAN).
pub fn station_certificate(
    secret: [u8; 32],
    call: Callsign,
) -> io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let mut pkcs8 = ED25519_PKCS8_PREFIX.to_vec();
    pkcs8.extend_from_slice(&secret);
    let der = PrivatePkcs8KeyDer::from(pkcs8.clone());
    let kp =
        rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&der, &rcgen::PKCS_ED25519).map_err(io::Error::other)?;
    let name = call.to_string();
    let mut params = rcgen::CertificateParams::new(vec![name.clone()]).map_err(io::Error::other)?;
    params.distinguished_name.push(rcgen::DnType::CommonName, &name);
    let cert = params.self_signed(&kp).map_err(io::Error::other)?;
    Ok((
        cert.der().clone(),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8)),
    ))
}

/// Callsign from the certificate CN (written by [`station_certificate`]).
pub fn callsign_in_cert(der: &[u8]) -> Option<Callsign> {
    const CN_OID: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
    let mut i = 0;
    while i + CN_OID.len() < der.len() {
        let Some(rel) = der[i..].windows(CN_OID.len()).position(|window| window == CN_OID) else {
            break;
        };
        let at = i + rel + CN_OID.len();
        let tag = *der.get(at)?;
        // UTF8String / PrintableString / IA5String
        if matches!(tag, 0x0c | 0x13 | 0x16) {
            let len = usize::from(*der.get(at + 1)?);
            if len < 128 {
                let bytes = der.get(at + 2..at + 2 + len)?;
                if let Ok(text) = std::str::from_utf8(bytes) {
                    if let Ok(call) = Callsign::parse(text) {
                        return Some(call);
                    }
                }
            }
        }
        i = at;
    }
    None
}

/// Trusted stations: callsign -> key, and key -> callsign to name peers.
#[derive(Debug, Default)]
pub(crate) struct TrustTable {
    keys: BTreeMap<Callsign, PublicKey>,
    pub(crate) names: BTreeMap<[u8; 32], Callsign>,
}

impl TrustTable {
    pub(crate) fn new(list: &[(Callsign, PublicKey)]) -> TrustTable {
        let keys: BTreeMap<Callsign, PublicKey> = list.iter().copied().collect();
        let names = keys.iter().map(|(c, k)| (k.0, *c)).collect();
        TrustTable { keys, names }
    }

    /// A station's key: its own, else its base callsign's.
    pub(crate) fn key_for(&self, call: Callsign) -> Option<PublicKey> {
        self.keys
            .get(&call)
            .or_else(|| self.keys.get(&call.base()))
            .copied()
    }
}

/// Verifies peer certificates. Dialed servers must be trusted; inbound clients
/// must be trusted unless [`NetConfig::open`] is set.
#[derive(Debug)]
pub(crate) struct TrustVerifier {
    pub(crate) trust: Arc<RwLock<TrustTable>>,
    pub(crate) provider: Arc<CryptoProvider>,
    /// Accept any inbound client with a valid station certificate.
    pub(crate) open: bool,
}

impl TrustVerifier {
    fn check_trusted(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let key = ed25519_key_in_cert(cert)
            .ok_or_else(|| rustls::Error::General("not an Ed25519 station key".into()))?;
        if self.trust.read().expect("lock").names.contains_key(&key) {
            Ok(())
        } else {
            Err(rustls::Error::General("station key not trusted".into()))
        }
    }

    fn check_client(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let key = ed25519_key_in_cert(cert)
            .ok_or_else(|| rustls::Error::General("not an Ed25519 station key".into()))?;
        if self.trust.read().expect("lock").names.contains_key(&key) {
            return Ok(());
        }
        if self.open {
            callsign_in_cert(cert)
                .ok_or_else(|| rustls::Error::General("station certificate has no callsign".into()))?;
            return Ok(());
        }
        Err(rustls::Error::General("station key not trusted".into()))
    }

    fn tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
}

impl ServerCertVerifier for TrustVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check_trusted(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for TrustVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check_client(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

pub(crate) fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().expect("in range")));
    // One bidirectional stream per message; the only unidirectional one is the
    // listener's confirmation. The link's receive window bounds what a peer
    // can make us buffer, whatever its streams claim.
    t.max_concurrent_bidi_streams(MAX_STREAMS.into());
    t.max_concurrent_uni_streams(2u32.into());
    t.receive_window((4 * (MAX_OBJECT as u32 + 8)).into());
    Arc::new(t)
}
