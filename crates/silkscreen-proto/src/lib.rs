//! Silkscreen wire protocol.
//!
//! Two channels connect a *host* (the computer whose desktop is extended) and a
//! *display* (the computer acting as a wireless monitor):
//!
//! * **Control** — a TCP connection: a plaintext [`HandshakeMsg`] exchange
//!   (pairing, Noise handshake), then encrypted [`ControlMsg`]s carrying
//!   stream configuration, input events, keyframe requests and latency
//!   probes. See [`control`].
//! * **Video** — UDP datagrams carrying encoded frames split into MTU-sized,
//!   individually encrypted packets. See [`video`].

pub mod control;
pub mod video;

pub use control::*;
pub use video::*;

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u16 = 3;

/// Where to get Silkscreen, quoted when two computers can't talk.
pub const RELEASES_URL: &str = "https://github.com/BenTech2/silkscreen/releases";

/// Explains, in words for either side to show, that a host speaking protocol
/// `host` and a display speaking `display` can't talk, and which to update.
pub fn version_mismatch(host: u16, display: u16) -> String {
    let (older, newer) = if host < display {
        ("host", "display")
    } else {
        ("display", "host")
    };
    format!(
        "Silkscreen versions don't match: the {older} has an older version than the {newer} \
         (protocol {host} on the host, {display} on the display). \
         Update the {older} from {RELEASES_URL}"
    )
}

/// Default TCP port a display listens on for control connections.
pub const DEFAULT_CONTROL_PORT: u16 = 47_810;

/// mDNS service type displays advertise themselves under.
pub const MDNS_SERVICE_TYPE: &str = "_silkscreen._tcp.local.";
