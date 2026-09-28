//! Internet bearer: QUIC between stations.
//!
//! Each station authenticates with its own Ed25519 station key, carried in a
//! self-signed certificate that also names its callsign. Dialers always verify
//! the listener against their trust list. Listeners do the same by default, or
//! with [`NetConfig::open`] accept any dialer that presents a valid station
//! certificate (a public core hub). There is no certificate authority.
//!
//! One endpoint both listens and dials; configured peers are redialled when
//! their connection drops. In TLS 1.3 the dialer finishes its handshake before
//! the listener has checked the dialer's certificate, so the listener, once it
//! has accepted the dialer, confirms it on a unidirectional stream (`"HMOK"`);
//! only then does the dialer count the link as up.
//!
//! A bundle travels on its own bidirectional stream:
//!
//! ```text
//! sender   -> "HMD0" | length u32 BE | object bytes
//! receiver -> 0x00 | receipt (64 B)             stored or already held
//!           | 0x01 | reason length u16 | reason  rejected
//!
//! Authenticated pairwise control messages use the same streams:
//! sender   -> "HMC0" | length u32 BE | control bytes
//! receiver -> 0x00
//! ```
//!
//! The receipt is the same statement as on radio (see `hm-xfer`), with base
//! callsigns and session 0: the receiver's signature proving it holds the object.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use hm_ident::{Identity, PublicKey};
use hm_wire::Callsign;
use hm_xfer::{object_id, receipt_statement};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

/// Application protocol name negotiated in the TLS handshake.
pub const ALPN: &[u8] = b"hm-net/1";
const OBJECT_MAGIC: &[u8; 4] = b"HMD0";
const CONTROL_MAGIC: &[u8; 4] = b"HMC0";
/// The listener's confirmation that it accepted the dialer.
const ACCEPTED: &[u8; 4] = b"HMOK";
/// How long a dialer waits for the listener to confirm it.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest object accepted over the internet.
pub const MAX_OBJECT: usize = 1024 * 1024;
pub const MAX_CONTROL: usize = 4096;
const REDIAL_EVERY: Duration = Duration::from_secs(3);
const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// What the node did with an object that arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Stored,
    /// Already held; still acknowledged with a receipt.
    Duplicate,
    /// Capacity is temporarily exhausted.
    Busy {
        retry_after: u16,
        reason: String,
    },
    Rejected(String),
}

/// Called for every object that arrives: `(sender's base callsign, object)`.
pub type Accept = Arc<dyn Fn(Callsign, Vec<u8>) -> Verdict + Send + Sync>;
/// Called for authenticated pairwise control data.
pub type Control = Arc<dyn Fn(Callsign, Vec<u8>) + Send + Sync>;

#[derive(Debug)]
pub enum NetError {
    NotConnected,
    Io(String),
    Busy { retry_after: u16, reason: String },
    Rejected(String),
    BadReceipt,
    Timeout,
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::NotConnected => f.write_str("no internet connection to that station"),
            NetError::Io(e) => write!(f, "internet link: {e}"),
            NetError::Busy { retry_after, reason } => {
                write!(f, "receiver busy for {retry_after} s: {reason}")
            }
            NetError::Rejected(r) => write!(f, "rejected by the receiver: {r}"),
            NetError::BadReceipt => f.write_str("receipt did not verify"),
            NetError::Timeout => f.write_str("no answer in time"),
        }
    }
}

impl std::error::Error for NetError {}

pub struct NetConfig {
    /// Our callsign; the base call is used on this bearer.
    pub me: Callsign,
    /// Our station key (32 secret bytes).
    pub secret: [u8; 32],
    /// Stations allowed to connect, and whose receipts we check.
    pub trust: Vec<(Callsign, PublicKey)>,
    pub listen: SocketAddr,
    /// Stations to keep a link to, as `host:port`; looked up again at every
    /// dial, so a changed address is followed. See also [`Net::set_dial`].
    pub dial: Vec<(Callsign, String)>,
    /// When true, accept inbound links from any station with a valid certificate
    /// (core hub). Outbound dials still require a trust entry for the peer.
    pub open: bool,
}

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
struct TrustTable {
    keys: BTreeMap<Callsign, PublicKey>,
    names: BTreeMap<[u8; 32], Callsign>,
}

impl TrustTable {
    fn new(list: &[(Callsign, PublicKey)]) -> TrustTable {
        let keys: BTreeMap<Callsign, PublicKey> = list.iter().copied().collect();
        let names = keys.iter().map(|(c, k)| (k.0, *c)).collect();
        TrustTable { keys, names }
    }

    /// A station's key: its own, else its base callsign's.
    fn key_for(&self, call: Callsign) -> Option<PublicKey> {
        self.keys
            .get(&call)
            .or_else(|| self.keys.get(&call.base()))
            .copied()
    }
}

/// Verifies peer certificates. Dialed servers must be trusted; inbound clients
/// must be trusted unless [`NetConfig::open`] is set.
#[derive(Debug)]
struct TrustVerifier {
    trust: Arc<RwLock<TrustTable>>,
    provider: Arc<CryptoProvider>,
    /// Accept any inbound client with a valid station certificate.
    open: bool,
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

fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().expect("in range")));
    Arc::new(t)
}

/// The internet bearer of one station.
pub struct Net {
    endpoint: Endpoint,
    me: Callsign,
    identity: Identity,
    /// Trusted stations, shared with the TLS verifier; see [`Net::set_trust`].
    trust: Arc<RwLock<TrustTable>>,
    /// Keys learned from open-hub inbound links (for receipt checks).
    session_keys: Mutex<BTreeMap<Callsign, PublicKey>>,
    conns: Mutex<BTreeMap<Callsign, Connection>>,
    /// Stations to keep a link to.
    dial: Mutex<Vec<(Callsign, String)>>,
    /// Accept any inbound station certificate.
    open: bool,
    accept: Accept,
    control: Control,
}

impl Net {
    /// Bind, start accepting connections, and keep dialling `cfg.dial`.
    /// Must be called inside a Tokio runtime.
    pub fn start(cfg: NetConfig, accept: Accept) -> io::Result<Arc<Net>> {
        Self::start_with_control(cfg, accept, Arc::new(|_, _| {}))
    }

    /// Start with a handler for authenticated control-plane messages.
    pub fn start_with_control(cfg: NetConfig, accept: Accept, control: Control) -> io::Result<Arc<Net>> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let trust = Arc::new(RwLock::new(TrustTable::new(&cfg.trust)));
        let verifier = Arc::new(TrustVerifier {
            trust: trust.clone(),
            provider: provider.clone(),
            open: cfg.open,
        });
        let (cert, key) = station_certificate(cfg.secret, cfg.me)?;
        let tls13 = &[&rustls::version::TLS13];

        let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(tls13)
            .map_err(io::Error::other)?
            .with_client_cert_verifier(verifier.clone())
            .with_single_cert(vec![cert.clone()], key.clone_key())
            .map_err(io::Error::other)?;
        server.alpn_protocols = vec![ALPN.to_vec()];
        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(tls13)
            .map_err(io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![cert], key)
            .map_err(io::Error::other)?;
        client.alpn_protocols = vec![ALPN.to_vec()];

        let mut server = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(server).map_err(io::Error::other)?,
        ));
        server.transport_config(transport());
        let mut client = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(client).map_err(io::Error::other)?,
        ));
        client.transport_config(transport());
        let mut endpoint = Endpoint::server(server, cfg.listen)?;
        endpoint.set_default_client_config(client);

        let net = Arc::new(Net {
            endpoint,
            me: cfg.me.base(),
            identity: Identity::from_secret(cfg.secret),
            trust,
            session_keys: Mutex::new(BTreeMap::new()),
            conns: Mutex::new(BTreeMap::new()),
            dial: Mutex::new(Vec::new()),
            open: cfg.open,
            accept,
            control,
        });
        tokio::spawn(accept_loop(net.clone()));
        *net.dial.lock().expect("lock") = cfg.dial;
        tokio::spawn(dial_loop(Arc::downgrade(&net)));
        Ok(net)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// The live connection to `peer`: named by its own callsign, or by its base
    /// callsign when its key is trusted without an SSID.
    fn conn_for(&self, peer: Callsign) -> Option<Connection> {
        let conns = self.conns.lock().expect("lock");
        [peer, peer.base()]
            .iter()
            .find_map(|c| conns.get(c).filter(|c| c.close_reason().is_none()).cloned())
    }

    /// Whether a live connection to `peer` exists.
    pub fn is_connected(&self, peer: Callsign) -> bool {
        self.conn_for(peer).is_some()
    }

    pub fn connected(&self) -> Vec<Callsign> {
        let conns = self.conns.lock().expect("lock");
        conns
            .iter()
            .filter(|(_, c)| c.close_reason().is_none())
            .map(|(k, _)| *k)
            .collect()
    }

    /// Public key bound to a live peer (trust list or an open-hub session).
    pub fn public_key_of(&self, peer: Callsign) -> Option<PublicKey> {
        self.trust
            .read()
            .expect("lock")
            .key_for(peer)
            .or_else(|| self.session_keys.lock().expect("lock").get(&peer).copied())
            .or_else(|| self.session_keys.lock().expect("lock").get(&peer.base()).copied())
    }

    /// Send `object` to `to` and wait for its verified receipt.
    pub async fn deliver(&self, to: Callsign, object: &[u8]) -> Result<(), NetError> {
        let conn = self.conn_for(to).ok_or(NetError::NotConnected)?;
        let key = self.public_key_of(to).ok_or(NetError::NotConnected)?;
        let exchange = async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| NetError::Io(e.to_string()))?;
            let mut msg = Vec::with_capacity(8 + object.len());
            msg.extend_from_slice(OBJECT_MAGIC);
            msg.extend_from_slice(&(object.len() as u32).to_be_bytes());
            msg.extend_from_slice(object);
            send.write_all(&msg)
                .await
                .map_err(|e| NetError::Io(e.to_string()))?;
            send.finish().map_err(|e| NetError::Io(e.to_string()))?;
            let reply = recv
                .read_to_end(4096)
                .await
                .map_err(|e| NetError::Io(e.to_string()))?;
            match reply.split_first() {
                Some((0, sig)) if sig.len() == 64 => {
                    let sig: [u8; 64] = sig.try_into().expect("length checked");
                    let statement = receipt_statement(to.base(), self.me, 0, &object_id(object));
                    key.verify(&statement, &sig).map_err(|_| NetError::BadReceipt)
                }
                Some((1, rest)) if rest.len() >= 2 => {
                    let n = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                    let reason = String::from_utf8_lossy(rest.get(2..2 + n).unwrap_or(&[])).into_owned();
                    Err(NetError::Rejected(reason))
                }
                Some((2, rest)) if rest.len() >= 4 => {
                    let retry_after = u16::from_be_bytes([rest[0], rest[1]]);
                    let n = u16::from_be_bytes([rest[2], rest[3]]) as usize;
                    let reason = String::from_utf8_lossy(rest.get(4..4 + n).unwrap_or(&[])).into_owned();
                    Err(NetError::Busy { retry_after, reason })
                }
                _ => Err(NetError::Io("malformed reply".into())),
            }
        };
        tokio::time::timeout(DELIVER_TIMEOUT, exchange)
            .await
            .map_err(|_| NetError::Timeout)?
    }

    /// Send one authenticated pairwise control message.
    pub async fn send_control(&self, to: Callsign, payload: &[u8]) -> Result<(), NetError> {
        if payload.len() > MAX_CONTROL {
            return Err(NetError::Io(format!(
                "control message of {} bytes exceeds {MAX_CONTROL}",
                payload.len()
            )));
        }
        let conn = self.conn_for(to).ok_or(NetError::NotConnected)?;
        let exchange = async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|error| NetError::Io(error.to_string()))?;
            let mut message = Vec::with_capacity(8 + payload.len());
            message.extend_from_slice(CONTROL_MAGIC);
            message.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            message.extend_from_slice(payload);
            send.write_all(&message)
                .await
                .map_err(|error| NetError::Io(error.to_string()))?;
            send.finish().map_err(|error| NetError::Io(error.to_string()))?;
            let reply = recv
                .read_to_end(512)
                .await
                .map_err(|error| NetError::Io(error.to_string()))?;
            match reply.as_slice() {
                [0] => Ok(()),
                [1, rest @ ..] if rest.len() >= 2 => {
                    let length = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                    let reason = String::from_utf8_lossy(rest.get(2..2 + length).unwrap_or(&[])).into_owned();
                    Err(NetError::Rejected(reason))
                }
                _ => Err(NetError::Io("malformed control reply".into())),
            }
        };
        tokio::time::timeout(DELIVER_TIMEOUT, exchange)
            .await
            .map_err(|_| NetError::Timeout)?
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }

    fn peer_of(&self, conn: &Connection) -> Option<Callsign> {
        let certs = conn
            .peer_identity()?
            .downcast::<Vec<CertificateDer<'static>>>()
            .ok()?;
        let cert = certs.first()?;
        let key = ed25519_key_in_cert(cert)?;
        if let Some(call) = self.trust.read().expect("lock").names.get(&key).copied() {
            return Some(call);
        }
        if self.open {
            return callsign_in_cert(cert);
        }
        None
    }

    /// Replace the stations to keep a link to. Links to stations dropped from
    /// the list stay until they break; they are not redialled.
    pub fn set_dial(&self, peers: Vec<(Callsign, String)>) {
        *self.dial.lock().expect("lock") = peers;
    }

    /// Replace the trusted stations. New connections are checked against the
    /// new list at once; a link whose key is no longer trusted, or now belongs
    /// to another callsign, is closed (and redialled if configured). Open hubs
    /// keep inbound links; only the trust list used for dialling changes.
    pub fn set_trust(&self, trust: &[(Callsign, PublicKey)]) {
        *self.trust.write().expect("lock") = TrustTable::new(trust);
        if self.open {
            return;
        }
        let mut conns = self.conns.lock().expect("lock");
        conns.retain(|name, conn| {
            let still = self.peer_of(conn) == Some(*name);
            if !still {
                conn.close(1u32.into(), b"no longer trusted");
            }
            still
        });
    }

    fn register(self: &Arc<Self>, conn: Connection) {
        let certs = conn
            .peer_identity()
            .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok());
        let key = certs
            .as_ref()
            .and_then(|c| c.first())
            .and_then(|c| ed25519_key_in_cert(c));
        let Some(peer) = self.peer_of(&conn) else {
            conn.close(1u32.into(), b"unknown station");
            return;
        };
        if let Some(key) = key {
            self.session_keys
                .lock()
                .expect("lock")
                .insert(peer, PublicKey(key));
        }
        self.conns.lock().expect("lock").insert(peer, conn.clone());
        tokio::spawn(serve_connection(self.clone(), peer, conn));
    }
}

async fn accept_loop(net: Arc<Net>) {
    while let Some(incoming) = net.endpoint.accept().await {
        let net = net.clone();
        tokio::spawn(async move {
            if let Ok(conn) = incoming.await {
                // The handshake is done on our side, so the dialer's key is
                // one we trust: tell it so before it counts the link as up.
                if net.peer_of(&conn).is_some() && confirm(&conn).await.is_err() {
                    return;
                }
                net.register(conn);
            }
        });
    }
}

/// Keeps the dial list's links up. Holds the station only while dialling, so
/// a station that is dropped releases its socket, and the loop ends.
async fn dial_loop(net: std::sync::Weak<Net>) {
    loop {
        let Some(net) = net.upgrade() else { return };
        let peers = net.dial.lock().expect("lock").clone();
        for (call, address) in &peers {
            if net.is_connected(*call) {
                continue;
            }
            let Ok(Ok(addrs)) =
                tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host(address)).await
            else {
                continue;
            };
            // A name can resolve to IPv6 and IPv4 addresses (localhost to ::1
            // and 127.0.0.1): try each, those our socket can reach first.
            let mut addrs: Vec<SocketAddr> = addrs.collect();
            let v4 = net.endpoint.local_addr().is_ok_and(|a| a.is_ipv4());
            addrs.sort_by_key(|a| a.is_ipv4() != v4);
            for addr in addrs {
                let Ok(connecting) = net.endpoint.connect(addr, "hm-net") else {
                    continue;
                };
                let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_secs(5), connecting).await else {
                    continue;
                };
                let named = net.peer_of(&conn);
                if named != Some(*call) && named != Some(call.base()) {
                    conn.close(1u32.into(), b"unexpected station");
                } else if confirmed(&conn).await {
                    net.register(conn);
                } else {
                    conn.close(1u32.into(), b"not accepted");
                }
                break;
            }
        }
        drop(net);
        tokio::time::sleep(REDIAL_EVERY).await;
    }
}

/// Listener side: confirm to the dialer that it was accepted.
async fn confirm(conn: &Connection) -> Result<(), String> {
    let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
    s.write_all(ACCEPTED).await.map_err(|e| e.to_string())?;
    s.finish().map_err(|e| e.to_string())
}

/// Dialer side: whether the listener confirmed it accepted us. A listener that
/// refuses our key closes the connection instead.
async fn confirmed(conn: &Connection) -> bool {
    let wait = async {
        let mut s = conn.accept_uni().await.ok()?;
        s.read_to_end(ACCEPTED.len()).await.ok()
    };
    matches!(
        tokio::time::timeout(CONFIRM_TIMEOUT, wait).await,
        Ok(Some(msg)) if msg == ACCEPTED
    )
}

async fn serve_connection(net: Arc<Net>, peer: Callsign, conn: Connection) {
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let net = net.clone();
        tokio::spawn(async move {
            let reply = match read_message(&mut recv).await {
                Ok(Incoming::Object(object)) => {
                    let id = object_id(&object);
                    let accept = net.accept.clone();
                    let verdict = tokio::task::spawn_blocking(move || accept(peer, object))
                        .await
                        .unwrap_or_else(|_| Verdict::Rejected("internal error".into()));
                    match verdict {
                        Verdict::Stored | Verdict::Duplicate => {
                            let sig = net.identity.sign(&receipt_statement(net.me, peer.base(), 0, &id));
                            let mut r = vec![0u8];
                            r.extend_from_slice(&sig);
                            r
                        }
                        Verdict::Busy { retry_after, reason } => busy(retry_after, &reason),
                        Verdict::Rejected(reason) => rejection(&reason),
                    }
                }
                Ok(Incoming::Control(payload)) => {
                    (net.control)(peer, payload);
                    vec![0]
                }
                Err(reason) => rejection(&reason),
            };
            let _ = send.write_all(&reply).await;
            let _ = send.finish();
        });
    }
    let mut conns = net.conns.lock().expect("lock");
    if conns
        .get(&peer)
        .is_some_and(|c| c.stable_id() == conn.stable_id())
    {
        conns.remove(&peer);
    }
}

fn rejection(reason: &str) -> Vec<u8> {
    let bytes = reason.as_bytes();
    let n = bytes.len().min(512);
    let mut r = vec![1u8];
    r.extend_from_slice(&(n as u16).to_be_bytes());
    r.extend_from_slice(&bytes[..n]);
    r
}

fn busy(retry_after: u16, reason: &str) -> Vec<u8> {
    let bytes = reason.as_bytes();
    let n = bytes.len().min(512);
    let mut reply = vec![2_u8];
    reply.extend_from_slice(&retry_after.to_be_bytes());
    reply.extend_from_slice(&(n as u16).to_be_bytes());
    reply.extend_from_slice(&bytes[..n]);
    reply
}

enum Incoming {
    Object(Vec<u8>),
    Control(Vec<u8>),
}

async fn read_message(recv: &mut quinn::RecvStream) -> Result<Incoming, String> {
    let mut head = [0u8; 8];
    recv.read_exact(&mut head).await.map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let (limit, control) = match &head[..4] {
        magic if magic == OBJECT_MAGIC => (MAX_OBJECT, false),
        magic if magic == CONTROL_MAGIC => (MAX_CONTROL, true),
        _ => return Err("unknown message type".into()),
    };
    if len > limit {
        return Err(format!("message of {len} bytes exceeds {limit}"));
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await.map_err(|e| e.to_string())?;
    Ok(if control {
        Incoming::Control(payload)
    } else {
        Incoming::Object(payload)
    })
}
