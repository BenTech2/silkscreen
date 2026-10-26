//! Device identity, pairing, and the encrypted control channel.
//!
//! Every device has a long-term X25519 key (its identity). Two devices pair
//! once: the display shows a 6-digit code, the host's user types it, and
//! SPAKE2 turns the code into a strong shared key without exposing it to
//! eavesdroppers or offline guessing. That key seeds a Noise `XXpsk3`
//! handshake which exchanges and authenticates both identities; each side
//! then remembers the other. Later connections use Noise `KK` (both keys
//! already known) — mutual authentication with no prompt.
//!
//! After the handshake, control messages are Noise transport messages and the
//! video key is derived from the handshake hash.

use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use silkscreen_proto::{
    ControlMsg, FrameError, HandshakeMsg, PROTOCOL_VERSION, VideoKey, read_frame, read_msg,
    version_mismatch, write_frame, write_msg,
};
use snow::{Builder, HandshakeState, TransportState};
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use tracing::{info, warn};

const KK: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";
const XX_PSK: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
const NOISE_MAX: usize = 65_535;
/// How long the display waits for the host's user to type the code.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);

static CONFIG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Sets where identity, settings and paired devices live (Android passes
/// its app files directory). Must be called before anything is loaded.
pub fn set_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR.set(dir);
}

/// Where identity, settings and paired devices live. `SILKSCREEN_CONFIG_DIR`
/// overrides it (useful for running two "devices" on one machine).
pub fn config_dir() -> PathBuf {
    if let Some(d) = CONFIG_DIR.get() {
        return d.clone();
    }
    if let Some(d) = std::env::var_os("SILKSCREEN_CONFIG_DIR") {
        return d.into();
    }
    directories::ProjectDirs::from("", "", "silkscreen")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".silkscreen"))
}

/// Short human-checkable form of a public key, e.g. `3f9a-c012-77be`.
pub fn fingerprint(key: &[u8; 32]) -> String {
    let h = Sha256::digest(key);
    format!(
        "{}-{}-{}",
        hex::encode(&h[0..2]),
        hex::encode(&h[2..4]),
        hex::encode(&h[4..6])
    )
}

pub struct Identity {
    private: [u8; 32],
    pub public: [u8; 32],
}

impl Identity {
    /// Loads this device's key, creating it on first run.
    pub fn load_or_create() -> Result<Self> {
        let path = config_dir().join("identity.key");
        if let Ok(bytes) = std::fs::read(&path)
            && bytes.len() == 64
        {
            return Ok(Identity {
                private: bytes[..32].try_into().unwrap(),
                public: bytes[32..].try_into().unwrap(),
            });
        }
        let kp = Builder::new(KK.parse()?).generate_keypair()?;
        let id = Identity {
            private: kp.private.as_slice().try_into()?,
            public: kp.public.as_slice().try_into()?,
        };
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut bytes = id.private.to_vec();
        bytes.extend_from_slice(&id.public);
        write_private(&path, &bytes)?;
        info!("created device identity {}", fingerprint(&id.public));
        Ok(id)
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    Ok(std::fs::write(path, bytes)?)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Peer {
    pub name: String,
    /// Hex-encoded X25519 public key.
    pub key: String,
    pub paired_at: u64,
}

impl Peer {
    pub fn key_bytes(&self) -> Option<[u8; 32]> {
        hex::decode(&self.key).ok()?.try_into().ok()
    }
}

/// Devices this one has paired with (as host or display).
pub struct PairedStore {
    path: PathBuf,
    peers: Mutex<Vec<Peer>>,
}

impl PairedStore {
    pub fn load() -> Self {
        let path = config_dir().join("paired.json");
        let peers = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        PairedStore {
            path,
            peers: Mutex::new(peers),
        }
    }

    pub fn list(&self) -> Vec<Peer> {
        self.peers.lock().unwrap().clone()
    }

    pub fn knows(&self, key: &[u8; 32]) -> bool {
        let k = hex::encode(key);
        self.peers.lock().unwrap().iter().any(|p| p.key == k)
    }

    pub fn add(&self, name: &str, key: &[u8; 32]) {
        let k = hex::encode(key);
        let mut peers = self.peers.lock().unwrap();
        peers.retain(|p| p.key != k);
        peers.push(Peer {
            name: name.to_owned(),
            key: k,
            paired_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        self.save(&peers);
    }

    pub fn remove(&self, key_hex: &str) {
        let mut peers = self.peers.lock().unwrap();
        peers.retain(|p| p.key != key_hex);
        self.save(&peers);
    }

    fn save(&self, peers: &[Peer]) {
        let r = std::fs::create_dir_all(self.path.parent().unwrap()).and_then(|_| {
            std::fs::write(
                &self.path,
                serde_json::to_vec_pretty(peers).unwrap_or_default(),
            )
        });
        if let Err(e) = r {
            warn!("could not save paired devices: {e}");
        }
    }
}

/// An authenticated, encrypted control connection.
pub struct SecureChannel {
    pub stream: TcpStream,
    transport: Arc<Mutex<TransportState>>,
    pub peer_name: String,
    pub video_key: VideoKey,
    /// This connection created a new pairing.
    pub newly_paired: bool,
}

impl std::fmt::Debug for SecureChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureChannel")
            .field("peer_name", &self.peer_name)
            .field("newly_paired", &self.newly_paired)
            .finish_non_exhaustive()
    }
}

impl SecureChannel {
    pub fn writer(&self) -> Result<SecureWriter> {
        Ok(SecureWriter {
            stream: Arc::new(Mutex::new(self.stream.try_clone()?)),
            transport: self.transport.clone(),
        })
    }

    pub fn reader(&self) -> Result<SecureReader> {
        Ok(SecureReader {
            stream: self.stream.try_clone()?,
            transport: self.transport.clone(),
        })
    }
}

/// Sends encrypted control messages; cheap to clone and share across threads.
#[derive(Clone)]
pub struct SecureWriter {
    stream: Arc<Mutex<TcpStream>>,
    transport: Arc<Mutex<TransportState>>,
}

impl SecureWriter {
    pub fn send(&self, msg: &ControlMsg) -> Result<(), FrameError> {
        let plain = postcard::to_stdvec(msg)?;
        let mut out = vec![0u8; plain.len() + 16];
        // Encrypt and write under the stream lock so frames go out in nonce order.
        let mut stream = self.stream.lock().unwrap();
        let n = self
            .transport
            .lock()
            .unwrap()
            .write_message(&plain, &mut out)
            .map_err(|e| FrameError::Io(std::io::Error::other(e)))?;
        write_frame(&mut *stream, &out[..n])
    }

    pub fn shutdown(&self) {
        let _ = self
            .stream
            .lock()
            .unwrap()
            .shutdown(std::net::Shutdown::Both);
    }
}

pub struct SecureReader {
    stream: TcpStream,
    transport: Arc<Mutex<TransportState>>,
}

impl SecureReader {
    pub fn recv(&mut self) -> Result<ControlMsg, FrameError> {
        let frame = read_frame(&mut self.stream)?;
        let mut plain = vec![0u8; frame.len()];
        let n = self
            .transport
            .lock()
            .unwrap()
            .read_message(&frame, &mut plain)
            .map_err(|_| {
                FrameError::Io(std::io::Error::other(
                    "control message failed authentication",
                ))
            })?;
        Ok(postcard::from_bytes(&plain[..n])?)
    }
}

/// The other side deliberately refused (wrong code, paused, not paired…).
/// Unlike network errors, retrying will not help without the user.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// A display turned a host away because their protocol versions differ;
/// the message says which to update.
#[derive(Debug)]
pub struct VersionMismatch(pub String);

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VersionMismatch {}

fn send(s: &mut TcpStream, m: &HandshakeMsg) -> Result<()> {
    write_msg(s, m)?;
    Ok(())
}

fn recv(s: &mut TcpStream) -> Result<HandshakeMsg> {
    match read_msg(s)? {
        HandshakeMsg::Reject { reason } => Err(Refused(reason).into()),
        m => Ok(m),
    }
}

fn noise_send(s: &mut TcpStream, hs: &mut HandshakeState) -> Result<()> {
    let mut buf = vec![0u8; NOISE_MAX];
    let n = hs.write_message(&[], &mut buf)?;
    send(s, &HandshakeMsg::Noise(buf[..n].to_vec()))
}

fn noise_recv(s: &mut TcpStream, hs: &mut HandshakeState) -> Result<()> {
    match recv(s)? {
        HandshakeMsg::Noise(m) => {
            let mut buf = vec![0u8; NOISE_MAX];
            hs.read_message(&m, &mut buf)
                .map(|_| ())
                .map_err(|_| anyhow!("handshake failed authentication (wrong device?)"))
        }
        other => bail!("unexpected {other:?}"),
    }
}

/// Binds the plaintext hello (both identities) into the Noise transcript.
fn prologue(host: &[u8; 32], display: &[u8; 32]) -> Vec<u8> {
    let mut p = b"silkscreen/2".to_vec();
    p.extend_from_slice(host);
    p.extend_from_slice(display);
    p
}

fn finish(
    hs: HandshakeState,
    stream: TcpStream,
    peer_name: String,
    peer_key: [u8; 32],
    newly_paired: bool,
) -> Result<SecureChannel> {
    // XX transmits the static key inside the handshake: it must match the hello.
    if hs.get_remote_static() != Some(&peer_key[..]) {
        bail!("peer identity changed during the handshake");
    }
    let video_key = VideoKey(
        Sha256::new_with_prefix(b"silkscreen video key")
            .chain_update(hs.get_handshake_hash())
            .finalize()
            .into(),
    );
    stream.set_read_timeout(None)?;
    Ok(SecureChannel {
        stream,
        transport: Arc::new(Mutex::new(hs.into_transport_mode()?)),
        peer_name,
        video_key,
        newly_paired,
    })
}

fn spake_ids(host: &[u8; 32], display: &[u8; 32]) -> (SpakeId, SpakeId) {
    (SpakeId::new(host), SpakeId::new(display))
}

fn psk(spake_key: &[u8]) -> [u8; 32] {
    Sha256::new_with_prefix(b"silkscreen pairing psk")
        .chain_update(spake_key)
        .finalize()
        .into()
}

/// Host side. `ask_pin` is called (with the display's name) only when the two
/// devices are not yet paired; it should return the code the display shows.
pub fn connect(
    mut stream: TcpStream,
    id: &Identity,
    store: &PairedStore,
    name: &str,
    ask_pin: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<SecureChannel> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    send(
        &mut stream,
        &HandshakeMsg::ClientHello {
            protocol: PROTOCOL_VERSION,
            name: name.into(),
            key: id.public,
        },
    )?;
    let (display_name, display_key, knows_me) = match recv(&mut stream)? {
        HandshakeMsg::ServerHello {
            protocol,
            name,
            key,
            knows_you,
        } => {
            if protocol != PROTOCOL_VERSION {
                return Err(Refused(version_mismatch(PROTOCOL_VERSION, protocol)).into());
            }
            (name, key, knows_you)
        }
        other => bail!("unexpected handshake reply {other:?}"),
    };
    let pair = !(knows_me && store.knows(&display_key));
    send(&mut stream, &HandshakeMsg::Begin { pair })?;
    let prologue = prologue(&id.public, &display_key);

    let mut hs = if pair {
        info!(
            "pairing with \"{display_name}\" ({})",
            fingerprint(&display_key)
        );
        let pin = ask_pin(&display_name)?;
        if pin.trim().is_empty() {
            return Err(Refused("pairing cancelled".into()).into());
        }
        let pin: String = pin.chars().filter(char::is_ascii_digit).collect();
        let (a, b) = spake_ids(&id.public, &display_key);
        let (spake, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(pin.as_bytes()), &a, &b);
        send(&mut stream, &HandshakeMsg::PairStart { spake: msg })?;
        let reply = match recv(&mut stream)? {
            HandshakeMsg::PairReply { spake } => spake,
            other => bail!("unexpected {other:?}"),
        };
        let key = spake
            .finish(&reply)
            .map_err(|e| anyhow!("pairing exchange failed: {e:?}"))?;
        let psk = psk(&key);
        Builder::new(XX_PSK.parse()?)
            .local_private_key(&id.private)?
            .psk(3, &psk)?
            .prologue(&prologue)?
            .build_initiator()?
    } else {
        Builder::new(KK.parse()?)
            .local_private_key(&id.private)?
            .remote_public_key(&display_key)?
            .prologue(&prologue)?
            .build_initiator()?
    };

    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    noise_send(&mut stream, &mut hs)?;
    noise_recv(&mut stream, &mut hs)?;
    if pair {
        noise_send(&mut stream, &mut hs)?;
    }
    match recv(&mut stream)? {
        HandshakeMsg::Established => {}
        other => bail!("unexpected {other:?}"),
    }
    if pair {
        store.add(&display_name, &display_key);
        info!("paired with \"{display_name}\"");
    }
    finish(hs, stream, display_name, display_key, pair)
}

/// What the display's UI needs to know while a host connects.
pub enum PairingEvent<'a> {
    /// Show this code until the host enters it (or gives up).
    ShowCode {
        host: &'a str,
        code: &'a str,
    },
    Done,
}

/// Display side. `admit` may refuse the host (busy, paused) with a reason.
pub fn accept(
    mut stream: TcpStream,
    id: &Identity,
    store: &PairedStore,
    name: &str,
    fixed_pin: Option<&str>,
    admit: &dyn Fn(&str) -> std::result::Result<(), String>,
    on_pairing: &dyn Fn(PairingEvent<'_>),
) -> Result<SecureChannel> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (host_name, host_key) = match read_msg(&mut stream)? {
        HandshakeMsg::ClientHello {
            protocol,
            name,
            key,
        } => {
            if protocol != PROTOCOL_VERSION {
                let reason = version_mismatch(protocol, PROTOCOL_VERSION);
                let _ = send(
                    &mut stream,
                    &HandshakeMsg::Reject {
                        reason: reason.clone(),
                    },
                );
                return Err(VersionMismatch(reason).into());
            }
            (name, key)
        }
        other => bail!("expected ClientHello, got {other:?}"),
    };
    if let Err(reason) = admit(&host_name) {
        let _ = send(
            &mut stream,
            &HandshakeMsg::Reject {
                reason: reason.clone(),
            },
        );
        bail!("refused \"{host_name}\": {reason}");
    }
    send(
        &mut stream,
        &HandshakeMsg::ServerHello {
            protocol: PROTOCOL_VERSION,
            name: name.into(),
            key: id.public,
            knows_you: store.knows(&host_key),
        },
    )?;
    let pair = match recv(&mut stream)? {
        HandshakeMsg::Begin { pair } => pair,
        other => bail!("unexpected {other:?}"),
    };
    if !pair && !store.knows(&host_key) {
        let _ = send(
            &mut stream,
            &HandshakeMsg::Reject {
                reason: "not paired with this display".into(),
            },
        );
        bail!("\"{host_name}\" skipped pairing but is not paired");
    }
    let prologue = prologue(&host_key, &id.public);

    let mut hs = if pair {
        let code = match fixed_pin {
            Some(p) => p.to_owned(),
            None => format!("{:06}", rand::random_range(0..1_000_000u32)),
        };
        info!("pairing request from \"{host_name}\" — enter code {code} on the host");
        on_pairing(PairingEvent::ShowCode {
            host: &host_name,
            code: &code,
        });
        let _clear = scopeguard(|| on_pairing(PairingEvent::Done));

        stream.set_read_timeout(Some(PAIRING_TIMEOUT))?;
        let start = match recv(&mut stream) {
            Ok(HandshakeMsg::PairStart { spake }) => spake,
            Ok(other) => bail!("unexpected {other:?}"),
            Err(e) => bail!("pairing cancelled: {e:#}"),
        };
        let (a, b) = spake_ids(&host_key, &id.public);
        let (spake, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(code.as_bytes()), &a, &b);
        send(&mut stream, &HandshakeMsg::PairReply { spake: msg })?;
        let key = spake
            .finish(&start)
            .map_err(|e| anyhow!("pairing exchange failed: {e:?}"))?;
        let psk = psk(&key);
        Builder::new(XX_PSK.parse()?)
            .local_private_key(&id.private)?
            .psk(3, &psk)?
            .prologue(&prologue)?
            .build_responder()?
    } else {
        Builder::new(KK.parse()?)
            .local_private_key(&id.private)?
            .remote_public_key(&host_key)?
            .prologue(&prologue)?
            .build_responder()?
    };

    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let reject = |stream: &mut TcpStream, reason: &str| -> anyhow::Error {
        let _ = send(
            stream,
            &HandshakeMsg::Reject {
                reason: reason.into(),
            },
        );
        anyhow!("\"{host_name}\": {reason}")
    };
    if noise_recv(&mut stream, &mut hs).is_err() {
        return Err(reject(
            &mut stream,
            "not paired with this display (pair again)",
        ));
    }
    noise_send(&mut stream, &mut hs)?;
    if pair && noise_recv(&mut stream, &mut hs).is_err() {
        // The PSK only enters in the last message: this is a wrong code.
        std::thread::sleep(Duration::from_secs(1)); // slow down guessing
        return Err(reject(&mut stream, "wrong pairing code"));
    }
    send(&mut stream, &HandshakeMsg::Established)?;
    if pair {
        store.add(&host_name, &host_key);
        info!("paired with \"{host_name}\"");
    }
    finish(hs, stream, host_name, host_key, pair)
}

/// Runs `f` when dropped.
fn scopeguard<F: FnOnce()>(f: F) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}

/// Reads a pairing code from the terminal (CLI host).
pub fn prompt_pin_stdin(display: &str) -> Result<String> {
    eprint!("Enter the pairing code shown on \"{display}\": ");
    let mut line = String::new();
    let mut byte = [0u8; 1];
    let mut stdin = std::io::stdin();
    while stdin.read(&mut byte).context("reading pairing code")? == 1 && byte[0] != b'\n' {
        line.push(byte[0] as char);
    }
    Ok(line.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn id() -> Identity {
        let kp = Builder::new(KK.parse().unwrap())
            .generate_keypair()
            .unwrap();
        Identity {
            private: kp.private.try_into().unwrap(),
            public: kp.public.try_into().unwrap(),
        }
    }

    fn store() -> PairedStore {
        let dir = std::env::temp_dir().join(format!("silkscreen-test-{}", rand::random::<u64>()));
        PairedStore {
            path: dir.join("paired.json"),
            peers: Mutex::new(Vec::new()),
        }
    }

    /// Runs one host/display handshake over loopback.
    fn run(
        host: &Identity,
        hs: &PairedStore,
        disp: &Identity,
        ds: &PairedStore,
        typed: &str,
    ) -> (Result<SecureChannel>, Result<SecureChannel>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::scope(|s| {
            let d = s.spawn(|| {
                let (c, _) = l.accept().unwrap();
                accept(c, disp, ds, "display", Some("123456"), &|_| Ok(()), &|_| {})
            });
            let c = TcpStream::connect(addr).unwrap();
            let h = connect(c, host, hs, "host", &mut |_| Ok(typed.to_owned()));
            (h, d.join().unwrap())
        })
    }

    #[test]
    fn pair_then_reconnect_and_talk() {
        let (host, disp) = (id(), id());
        let (hs, ds) = (store(), store());

        let (h, d) = run(&host, &hs, &disp, &ds, "123 456");
        let (h, d) = (h.unwrap(), d.unwrap());
        assert!(h.newly_paired && d.newly_paired);
        assert_eq!(h.video_key.0, d.video_key.0);
        assert!(hs.knows(&disp.public) && ds.knows(&host.public));

        // Encrypted control messages flow both ways.
        h.writer()
            .unwrap()
            .send(&ControlMsg::Ping { t_us: 42 })
            .unwrap();
        assert_eq!(
            d.reader().unwrap().recv().unwrap(),
            ControlMsg::Ping { t_us: 42 }
        );
        d.writer().unwrap().send(&ControlMsg::Goodbye).unwrap();
        assert_eq!(h.reader().unwrap().recv().unwrap(), ControlMsg::Goodbye);

        // Second connection: KK, no code needed (a wrong one is never asked for).
        let (h2, d2) = run(&host, &hs, &disp, &ds, "000000");
        let (h2, d2) = (h2.unwrap(), d2.unwrap());
        assert!(!h2.newly_paired && !d2.newly_paired);
        assert_ne!(h2.video_key.0, h.video_key.0, "fresh session keys");
    }

    #[test]
    fn wrong_code_is_rejected() {
        let (host, disp) = (id(), id());
        let (hs, ds) = (store(), store());
        let (h, d) = run(&host, &hs, &disp, &ds, "654321");
        assert!(h.unwrap_err().to_string().contains("wrong pairing code"));
        assert!(d.is_err());
        assert!(!hs.knows(&disp.public) && !ds.knows(&host.public));
    }

    #[test]
    fn one_sided_pairing_falls_back_to_pairing() {
        let (host, disp) = (id(), id());
        let (hs, ds) = (store(), store());
        // The display remembers the host, but the host forgot the display.
        ds.add("host", &host.public);
        let (h, d) = run(&host, &hs, &disp, &ds, "123456");
        assert!(h.unwrap().newly_paired && d.unwrap().newly_paired);
    }

    #[test]
    fn older_host_is_told_to_update() {
        let (disp, ds) = (id(), store());
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::scope(|s| {
            let d = s.spawn(|| {
                let (c, _) = l.accept().unwrap();
                accept(c, &disp, &ds, "display", None, &|_| Ok(()), &|_| {})
            });
            // What an older host sends.
            let mut c = TcpStream::connect(addr).unwrap();
            let hello = HandshakeMsg::ClientHello {
                protocol: PROTOCOL_VERSION - 1,
                name: "old host".into(),
                key: id().public,
            };
            send(&mut c, &hello).unwrap();
            let reason = recv(&mut c).unwrap_err().to_string();
            assert!(reason.contains("Update the host"), "{reason}");
            let err = d.join().unwrap().unwrap_err();
            assert!(err.downcast_ref::<VersionMismatch>().is_some());
        });
    }

    #[test]
    fn older_display_is_named_by_the_host() {
        let (host, hs) = (id(), store());
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                // What an older display replies.
                let (mut c, _) = l.accept().unwrap();
                read_msg::<_, HandshakeMsg>(&mut c).unwrap();
                let hello = HandshakeMsg::ServerHello {
                    protocol: PROTOCOL_VERSION - 1,
                    name: "old display".into(),
                    key: id().public,
                    knows_you: false,
                };
                send(&mut c, &hello).unwrap();
            });
            let c = TcpStream::connect(addr).unwrap();
            let err = connect(c, &host, &hs, "host", &mut |_| Ok(String::new())).unwrap_err();
            let r = err
                .downcast_ref::<Refused>()
                .expect("a refusal, not retried");
            assert!(r.0.contains("Update the display"), "{}", r.0);
        });
    }
}
