//! Control channel messages and framing.
//!
//! A connection starts with a short plaintext [`HandshakeMsg`] exchange
//! (identities, optional PIN pairing, Noise handshake). After that every
//! frame carries an encrypted, postcard-encoded [`ControlMsg`]. On the wire a
//! frame is a little-endian `u32` byte length followed by the body.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Upper bound on a single control message, to reject garbage early.
pub const MAX_CONTROL_MSG_LEN: u32 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    /// PyroWave: an intra-only wavelet codec on the GPU (Vulkan or Metal).
    /// Every frame stands alone; needs a few hundred Mbit/s (Ethernet).
    /// Experimental.
    PyroWave,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
            Codec::PyroWave => "pyrowave",
        }
    }
}

/// What a display can show and sense. Sent by the display right after the
/// session is established.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayCaps {
    /// Human readable name, e.g. "Surface Studio".
    pub name: String,
    /// Native panel resolution in physical pixels.
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (59_940 = 59.94 Hz).
    pub refresh_mhz: u32,
    /// OS scale factor of the panel (1.0, 1.5, 2.0...). The host uses this to
    /// pick a sensible scale for the virtual monitor.
    pub scale: f32,
    /// Physical size in millimetres if known, so the host can report a
    /// correct DPI for the virtual monitor.
    pub physical_size_mm: Option<(u32, u32)>,
    pub touch: bool,
    pub max_touch_points: u8,
    pub pen: bool,
    /// Codecs the display can decode, most preferred first.
    pub decoders: Vec<Codec>,
    /// Whether the display decodes 10-bit / HDR streams. Reserved.
    pub hdr: bool,
    /// UDP port the display receives video on.
    pub video_port: u16,
}

/// Stream parameters chosen by the host, sent before the first video packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub bitrate_kbps: u32,
    /// Random id stamped on every video packet of this stream, so packets
    /// from a previous stream are ignored.
    pub stream_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Input coming from the display back to the host.
///
/// Positions are normalised to `0.0..=1.0` across the streamed surface, so
/// they are independent of either side's scaling.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    Touch {
        /// Stable id for the lifetime of one contact.
        id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        /// `0.0..=1.0` if the digitiser reports it.
        pressure: Option<f32>,
    },
    Pen {
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
        /// Tilt in degrees, `-90..=90`.
        tilt_x: f32,
        tilt_y: f32,
        /// Barrel button held.
        barrel: bool,
        /// Eraser end in use.
        eraser: bool,
        /// Hovering in range without contact.
        hover: bool,
    },
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Scroll in "lines" (wheel notches) or pixels when `pixels` is set.
    Scroll {
        dx: f32,
        dy: f32,
        pixels: bool,
    },
    /// Keyboard key using Linux evdev keycodes, which every platform module
    /// translates from/to.
    Key {
        evdev_code: u16,
        pressed: bool,
    },
}

/// Plaintext messages that set up a secure session.
///
/// 1. host → `ClientHello`, display → `ServerHello` (or `Reject`)
/// 2. host → `Begin { pair }`; pairing is needed unless both sides already
///    know each other's key
/// 3. pairing only: the display shows a 6-digit code, the host's user types
///    it; `PairStart`/`PairReply` run SPAKE2 on it, yielding a shared key
/// 4. `Noise` messages: `KK` for paired devices, `XXpsk3` (keyed with the
///    SPAKE2 result) when pairing
/// 5. display → `Established` (or `Reject`, e.g. wrong code)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HandshakeMsg {
    ClientHello {
        protocol: u16,
        name: String,
        /// X25519 static public key identifying the host.
        key: [u8; 32],
    },
    ServerHello {
        protocol: u16,
        name: String,
        key: [u8; 32],
        /// The display has paired with this host before.
        knows_you: bool,
    },
    Begin {
        pair: bool,
    },
    PairStart {
        spake: Vec<u8>,
    },
    PairReply {
        spake: Vec<u8>,
    },
    Noise(Vec<u8>),
    Established,
    Reject {
        reason: String,
    },
}

/// Messages on the encrypted control channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Display → host, first message once the session is established.
    Caps(DisplayCaps),
    /// Display → host. The display cannot continue (e.g. cannot decode).
    Reject {
        reason: String,
    },
    /// Host → display. A new stream starts; reset the decoder.
    StartStream(StreamConfig),
    /// Host → display. The stream stopped (e.g. host is sleeping).
    StopStream,
    /// Display → host. Video was lost; send an IDR frame as soon as possible.
    RequestKeyframe,
    /// Display → host.
    Input(InputEvent),
    /// Either direction. Answered with `Pong` carrying the same value.
    Ping {
        t_us: u64,
    },
    Pong {
        t_us: u64,
    },
    /// Display → host. Periodic receive statistics for adaptive bitrate.
    Stats(ReceiverStats),
    /// Either direction. Orderly shutdown.
    Goodbye,
    /// Display → host. Someone at the display ended the session (hotkey,
    /// leaving display mode): the host should not reconnect on its own.
    EndedByDisplay,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ReceiverStats {
    pub frames_received: u32,
    pub frames_dropped: u32,
    pub packets_lost: u32,
    /// Average time from first packet received to frame presented, µs.
    pub decode_present_us: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("control message too large ({0} bytes)")]
    TooLarge(u32),
    #[error("malformed control message: {0}")]
    Decode(#[from] postcard::Error),
}

/// Write one length-prefixed frame. Callers should disable Nagle
/// (`TCP_NODELAY`) so input events go out immediately.
pub fn write_frame<W: Write>(w: &mut W, body: &[u8]) -> Result<(), FrameError> {
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed frame, blocking until it is complete.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_CONTROL_MSG_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Write one framed, postcard-encoded message.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    write_frame(w, &postcard::to_stdvec(msg)?)
}

/// Read one framed, postcard-encoded message.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(&read_frame(r)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = [
            ControlMsg::Input(InputEvent::Touch {
                id: 3,
                phase: TouchPhase::Move,
                x: 0.25,
                y: 0.75,
                pressure: Some(0.5),
            }),
            ControlMsg::Caps(DisplayCaps {
                name: "Surface Studio".into(),
                width: 4500,
                height: 3000,
                refresh_mhz: 60_000,
                scale: 2.0,
                physical_size_mm: Some((637, 425)),
                touch: true,
                max_touch_points: 10,
                pen: true,
                decoders: vec![Codec::Hevc, Codec::H264],
                hdr: false,
                video_port: 47_811,
            }),
            ControlMsg::RequestKeyframe,
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            write_msg(&mut wire, m).unwrap();
        }
        let mut r = wire.as_slice();
        for m in &msgs {
            assert_eq!(&read_msg::<_, ControlMsg>(&mut r).unwrap(), m);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn rejects_huge_length() {
        let wire = (MAX_CONTROL_MSG_LEN + 1).to_le_bytes();
        assert!(matches!(
            read_frame(&mut wire.as_slice()),
            Err(FrameError::TooLarge(_))
        ));
    }
}
