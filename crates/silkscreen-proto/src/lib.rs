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

/// Default TCP port a display listens on for control connections.
pub const DEFAULT_CONTROL_PORT: u16 = 47_810;

/// mDNS service type displays advertise themselves under.
pub const MDNS_SERVICE_TYPE: &str = "_silkscreen._tcp.local.";
