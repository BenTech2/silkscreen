//! Video packetization over UDP.
//!
//! Each encoded frame (one access unit) is split into datagrams no larger than
//! the configured MTU. Every datagram starts with a fixed 24-byte
//! [`PacketHeader`]. The receiver's [`Reassembler`] rebuilds frames and
//! enforces decode-safe ordering: once any frame is lost, everything up to
//! the next keyframe is discarded (the display briefly holds its last image
//! instead of showing corrupted video) and a keyframe request is signalled.
//!
//! With a [`VideoKey`] (derived from the session's Noise handshake) each
//! payload is sealed with ChaCha20-Poly1305: the header is authenticated as
//! associated data and the nonce is `(stream_id, frame_id, index)`, which is
//! unique per key, so no extra nonce bytes are sent.

use std::collections::BTreeMap;
use std::time::Instant;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};

pub const PACKET_MAGIC: [u8; 2] = *b"SK";
pub const PACKET_VERSION: u8 = 2;
pub const HEADER_LEN: usize = 24;
pub const TAG_LEN: usize = 16;

/// Safe default datagram size for Ethernet/Wi-Fi with some headroom for
/// tunnels: 1500 MTU - 20 IPv4 - 8 UDP - slack.
pub const DEFAULT_DATAGRAM_LEN: usize = 1400;

pub const FLAG_KEYFRAME: u8 = 1 << 0;
pub const FLAG_ENCRYPTED: u8 = 1 << 1;

/// Symmetric key for video packets.
#[derive(Clone)]
pub struct VideoKey(pub [u8; 32]);

impl std::fmt::Debug for VideoKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VideoKey(..)")
    }
}

fn cipher(key: Option<&VideoKey>) -> Option<ChaCha20Poly1305> {
    key.map(|k| ChaCha20Poly1305::new((&k.0).into()))
}

fn nonce(h: &PacketHeader) -> Nonce {
    let mut n = [0u8; 12];
    n[0..4].copy_from_slice(&h.stream_id.to_le_bytes());
    n[4..8].copy_from_slice(&h.frame_id.to_le_bytes());
    n[8..10].copy_from_slice(&h.index.to_le_bytes());
    n.into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub flags: u8,
    pub stream_id: u32,
    pub frame_id: u32,
    /// Host capture timestamp, microseconds since the stream started.
    pub pts_us: u64,
    pub index: u16,
    pub count: u16,
}

impl PacketHeader {
    pub fn encode(&self, out: &mut [u8]) {
        out[0..2].copy_from_slice(&PACKET_MAGIC);
        out[2] = PACKET_VERSION;
        out[3] = self.flags;
        out[4..8].copy_from_slice(&self.stream_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.frame_id.to_le_bytes());
        out[12..20].copy_from_slice(&self.pts_us.to_le_bytes());
        out[20..22].copy_from_slice(&self.index.to_le_bytes());
        out[22..24].copy_from_slice(&self.count.to_le_bytes());
    }

    pub fn decode(buf: &[u8]) -> Option<(Self, &[u8])> {
        if buf.len() < HEADER_LEN || buf[0..2] != PACKET_MAGIC || buf[2] != PACKET_VERSION {
            return None;
        }
        let h = PacketHeader {
            flags: buf[3],
            stream_id: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            frame_id: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            pts_us: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
            index: u16::from_le_bytes(buf[20..22].try_into().unwrap()),
            count: u16::from_le_bytes(buf[22..24].try_into().unwrap()),
        };
        if h.count == 0 || h.index >= h.count {
            return None;
        }
        Some((h, &buf[HEADER_LEN..]))
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
}

/// Splits encoded frames into datagrams.
pub struct Packetizer {
    stream_id: u32,
    datagram_len: usize,
    next_frame_id: u32,
    cipher: Option<ChaCha20Poly1305>,
    buf: Vec<u8>,
}

impl Packetizer {
    pub fn new(stream_id: u32, datagram_len: usize, key: Option<&VideoKey>) -> Self {
        assert!(
            datagram_len > HEADER_LEN + TAG_LEN + 64,
            "datagram too small"
        );
        Packetizer {
            stream_id,
            datagram_len,
            next_frame_id: 0,
            cipher: cipher(key),
            buf: vec![0; datagram_len],
        }
    }

    /// Calls `emit` once per datagram of `frame`, in order. Returns the frame id.
    pub fn packetize<E>(
        &mut self,
        frame: &[u8],
        keyframe: bool,
        pts_us: u64,
        mut emit: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<u32, E> {
        let tag_len = if self.cipher.is_some() { TAG_LEN } else { 0 };
        let payload_len = self.datagram_len - HEADER_LEN - tag_len;
        let count = frame.len().div_ceil(payload_len).max(1);
        assert!(count <= u16::MAX as usize, "frame too large to packetize");
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        for index in 0..count {
            let chunk = &frame[(index * payload_len).min(frame.len())
                ..((index + 1) * payload_len).min(frame.len())];
            let mut flags = if keyframe { FLAG_KEYFRAME } else { 0 };
            if self.cipher.is_some() {
                flags |= FLAG_ENCRYPTED;
            }
            let header = PacketHeader {
                flags,
                stream_id: self.stream_id,
                frame_id,
                pts_us,
                index: index as u16,
                count: count as u16,
            };
            let (head, body) = self.buf.split_at_mut(HEADER_LEN);
            header.encode(head);
            body[..chunk.len()].copy_from_slice(chunk);
            if let Some(c) = &self.cipher {
                let tag = c
                    .encrypt_in_place_detached(&nonce(&header), head, &mut body[..chunk.len()])
                    .expect("chunk within ChaCha20 limits");
                body[chunk.len()..chunk.len() + TAG_LEN].copy_from_slice(&tag);
            }
            emit(&self.buf[..HEADER_LEN + chunk.len() + tag_len])?;
        }
        Ok(frame_id)
    }
}

/// A fully reassembled encoded frame.
#[derive(Debug)]
pub struct EncodedFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub pts_us: u64,
    pub data: Vec<u8>,
    /// When the first datagram of this frame arrived.
    pub first_packet_at: Instant,
}

struct Partial {
    keyframe: bool,
    pts_us: u64,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    first_packet_at: Instant,
}

#[derive(Debug, Default)]
pub struct PushResult {
    pub frame: Option<EncodedFrame>,
    /// The decoder cannot continue without a keyframe. The caller should send
    /// a (rate limited) keyframe request to the host.
    pub need_keyframe: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReassemblyStats {
    pub frames_delivered: u32,
    pub frames_dropped: u32,
    pub packets_lost: u32,
}

/// Rebuilds frames from datagrams for a single stream.
pub struct Reassembler {
    stream_id: u32,
    cipher: Option<ChaCha20Poly1305>,
    partial: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
    waiting_for_key: bool,
    pub stats: ReassemblyStats,
}

impl Reassembler {
    pub fn new(stream_id: u32, key: Option<&VideoKey>) -> Self {
        Reassembler {
            stream_id,
            cipher: cipher(key),
            partial: BTreeMap::new(),
            last_delivered: None,
            waiting_for_key: true,
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub fn push(&mut self, datagram: &[u8], now: Instant) -> PushResult {
        let Some((h, payload)) = PacketHeader::decode(datagram) else {
            return PushResult::default();
        };
        if h.stream_id != self.stream_id {
            return PushResult::default();
        }
        if self.last_delivered.is_some_and(|last| h.frame_id <= last) {
            return PushResult::default(); // late or duplicate
        }
        // Never accept plaintext on an encrypted session (or vice versa).
        if (h.flags & FLAG_ENCRYPTED != 0) != self.cipher.is_some() {
            return PushResult::default();
        }
        let payload = match &self.cipher {
            None => payload.to_vec(),
            Some(c) => {
                let Some(split) = payload.len().checked_sub(TAG_LEN) else {
                    return PushResult::default();
                };
                let (body, tag) = payload.split_at(split);
                let mut body = body.to_vec();
                let aad = &datagram[..HEADER_LEN];
                if c.decrypt_in_place_detached(&nonce(&h), aad, &mut body, Tag::from_slice(tag))
                    .is_err()
                {
                    return PushResult::default(); // forged or corrupted
                }
                body
            }
        };

        let p = self.partial.entry(h.frame_id).or_insert_with(|| Partial {
            keyframe: h.is_keyframe(),
            pts_us: h.pts_us,
            chunks: vec![None; h.count as usize],
            received: 0,
            first_packet_at: now,
        });
        if p.chunks.len() != h.count as usize {
            return PushResult::default(); // inconsistent header; ignore
        }
        let slot = &mut p.chunks[h.index as usize];
        if slot.is_none() {
            *slot = Some(payload);
            p.received += 1;
        }
        if p.received < p.chunks.len() {
            return PushResult::default();
        }

        let p = self.partial.remove(&h.frame_id).unwrap();
        self.evict_older_than(h.frame_id);

        let contiguous = self
            .last_delivered
            .is_some_and(|last| h.frame_id == last.wrapping_add(1));
        if !contiguous && !p.keyframe {
            self.waiting_for_key = true;
        }
        if self.waiting_for_key && !p.keyframe {
            self.stats.frames_dropped += 1;
            // Remember how far we got so stale packets are still rejected.
            self.last_delivered = Some(h.frame_id);
            return PushResult {
                frame: None,
                need_keyframe: true,
            };
        }
        self.waiting_for_key = false;
        self.last_delivered = Some(h.frame_id);
        self.stats.frames_delivered += 1;

        let mut data = Vec::with_capacity(p.chunks.iter().flatten().map(Vec::len).sum());
        for c in p.chunks.into_iter().flatten() {
            data.extend_from_slice(&c);
        }
        PushResult {
            frame: Some(EncodedFrame {
                frame_id: h.frame_id,
                keyframe: p.keyframe,
                pts_us: p.pts_us,
                data,
                first_packet_at: p.first_packet_at,
            }),
            need_keyframe: false,
        }
    }

    /// Frames older than a completed one will never be decodable in order.
    fn evict_older_than(&mut self, frame_id: u32) {
        let newer = self.partial.split_off(&frame_id);
        for (_, p) in std::mem::replace(&mut self.partial, newer) {
            self.stats.frames_dropped += 1;
            self.stats.packets_lost += (p.chunks.len() - p.received) as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(p: &mut Packetizer, data: &[u8], key: bool) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        p.packetize::<()>(data, key, 0, |d| {
            out.push(d.to_vec());
            Ok(())
        })
        .unwrap();
        out
    }

    fn frame(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn roundtrip_out_of_order() {
        let mut p = Packetizer::new(7, 200, None);
        let mut r = Reassembler::new(7, None);
        let now = Instant::now();
        let data = frame(5000, 1);
        let mut pk = packets(&mut p, &data, true);
        pk.reverse();
        let mut got = None;
        for d in &pk {
            let res = r.push(d, now);
            if let Some(f) = res.frame {
                got = Some(f);
            }
        }
        let f = got.expect("frame");
        assert_eq!(f.data, data);
        assert!(f.keyframe);
    }

    #[test]
    fn empty_frame() {
        let mut p = Packetizer::new(1, 200, None);
        let mut r = Reassembler::new(1, None);
        let pk = packets(&mut p, &[], true);
        assert_eq!(pk.len(), 1);
        assert_eq!(r.push(&pk[0], Instant::now()).frame.unwrap().data.len(), 0);
    }

    #[test]
    fn loss_waits_for_keyframe() {
        let mut p = Packetizer::new(1, 200, None);
        let mut r = Reassembler::new(1, None);
        let now = Instant::now();

        // Before the first keyframe nothing is delivered.
        let pf = packets(&mut p, &frame(300, 0), false);
        let res = pf.iter().map(|d| r.push(d, now)).last().unwrap();
        assert!(res.frame.is_none() && res.need_keyframe);

        let k = packets(&mut p, &frame(300, 1), true);
        assert!(
            k.iter()
                .filter_map(|d| r.push(d, now).frame)
                .next()
                .is_some()
        );

        // Frame 2 loses a packet, frame 3 arrives complete -> dropped, need key.
        let lost = packets(&mut p, &frame(600, 2), false);
        for d in &lost[1..] {
            assert!(r.push(d, now).frame.is_none());
        }
        let f3 = packets(&mut p, &frame(300, 3), false);
        let res = f3.iter().map(|d| r.push(d, now)).last().unwrap();
        assert!(res.frame.is_none() && res.need_keyframe);
        assert_eq!(r.stats.packets_lost, 1);

        // Late packet of the evicted frame is ignored.
        assert!(r.push(&lost[0], now).frame.is_none());

        // Next keyframe recovers, and following P-frames flow again.
        let k2 = packets(&mut p, &frame(300, 4), true);
        assert!(
            k2.iter()
                .filter_map(|d| r.push(d, now).frame)
                .next()
                .unwrap()
                .keyframe
        );
        let f5 = packets(&mut p, &frame(300, 5), false);
        assert!(
            f5.iter()
                .filter_map(|d| r.push(d, now).frame)
                .next()
                .is_some()
        );
    }

    #[test]
    fn ignores_other_streams_and_garbage() {
        let mut p = Packetizer::new(1, 200, None);
        let mut r = Reassembler::new(2, None);
        for d in packets(&mut p, &frame(300, 0), true) {
            assert!(r.push(&d, Instant::now()).frame.is_none());
        }
        assert!(r.push(b"nonsense", Instant::now()).frame.is_none());
    }

    #[test]
    fn encrypted_roundtrip_and_tamper() {
        let key = VideoKey([7; 32]);
        let mut p = Packetizer::new(3, 200, Some(&key));
        let data = frame(1000, 9);
        let pk = packets(&mut p, &data, true);
        assert!(pk.iter().all(|d| d.len() <= 200));

        // Wrong key: nothing decodes.
        let mut wrong = Reassembler::new(3, Some(&VideoKey([8; 32])));
        assert!(
            pk.iter()
                .all(|d| wrong.push(d, Instant::now()).frame.is_none())
        );
        // Plaintext receiver refuses encrypted packets and vice versa.
        let mut plain = Reassembler::new(3, None);
        assert!(
            pk.iter()
                .all(|d| plain.push(d, Instant::now()).frame.is_none())
        );

        // A flipped bit anywhere (header or payload) is rejected.
        let mut r = Reassembler::new(3, Some(&key));
        let mut bad = pk[0].clone();
        bad[30] ^= 1;
        assert!(r.push(&bad, Instant::now()).frame.is_none());
        let mut bad_header = pk[0].clone();
        bad_header[14] ^= 1; // pts
        assert!(r.push(&bad_header, Instant::now()).frame.is_none());

        let f = pk
            .iter()
            .filter_map(|d| r.push(d, Instant::now()).frame)
            .next()
            .unwrap();
        assert_eq!(f.data, data);
    }
}
