//! Mirroring: one capture and one encode of a screen, shared by every host
//! session showing that screen. Each session only packetizes, encrypts (with
//! its own display's key) and sends.
//!
//! Sessions whose source is an existing monitor or the test pattern join the
//! mirror for that source, starting it if it isn't running; the mirror stops
//! once its last session leaves. Size and refresh come from the session that
//! started it (other displays scale the picture to fit); the codec is one
//! every joined display can decode.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};
use silkscreen_codec::{
    Encoder, EncoderConfig, RgbFrame, RgbLayout, compiled_encoders, default_bitrate_kbps,
};
use silkscreen_platform::{Frame, InputSink, PixelLayout, ScreenRequest, SourceKind, open_screen};
use silkscreen_proto::{Codec, InputEvent};
use tracing::{info, warn};

use crate::pacer::Pacer;

/// What a session sends its display.
#[derive(Clone)]
pub enum Event {
    /// A new stream begins (first, and whenever the encoder is reopened).
    Start {
        codec: Codec,
        width: u32,
        height: u32,
        refresh_mhz: u32,
        bitrate_kbps: u32,
        encoder: String,
    },
    Frame {
        data: Arc<[u8]>,
        key: bool,
        pts: u64,
    },
    /// The mirror cannot continue (capture or every encoder failed).
    Failed(String),
}

/// Encoder settings, from the sessions' stream options.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub codec: Option<Codec>,
    pub encoder: Option<String>,
    pub bitrate_kbps: Option<u32>,
    /// Frame rate limit for the shared stream.
    pub max_fps: Option<f32>,
}

struct Sub {
    id: u64,
    tx: Sender<Event>,
    decoders: Vec<Codec>,
}

struct State {
    subs: Vec<Sub>,
    next_id: u64,
    settings: Settings,
    /// Bumped when the settings or the set of displays change.
    generation: u64,
    /// The stream being sent, for sessions that join mid-way.
    current: Option<Event>,
    /// Set by the capture thread when it has stopped for good.
    closed: bool,
}

struct Shared {
    kind: SourceKind,
    description: String,
    state: Mutex<State>,
    keyframe: AtomicBool,
    /// Average encode time in µs, for the sessions' stats.
    encode_us: AtomicU32,
    input: Mutex<Box<dyn InputSink>>,
}

/// Running mirrors, by source.
static MIRRORS: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

/// A session's membership of a mirror; leaves it when dropped.
pub struct Subscription {
    shared: Arc<Shared>,
    id: u64,
    pub rx: Receiver<Event>,
}

/// Joins the mirror of `req.kind`, starting it (with `req`'s size) if
/// needed. `decoders` are what this session's display can decode.
pub fn join(req: &ScreenRequest, decoders: &[Codec], settings: Settings) -> Result<Subscription> {
    if let Some(c) = settings.codec
        && !decoders.contains(&c)
    {
        bail!("this display can't decode {}", c.name());
    }
    let mut mirrors = MIRRORS.lock().unwrap();
    mirrors.retain(|w| w.strong_count() > 0);
    let running = mirrors
        .iter()
        .filter_map(Weak::upgrade)
        .find(|m| m.kind == req.kind && !m.state.lock().unwrap().closed);
    let shared = match running {
        Some(m) => m,
        None => {
            let shared = start(req, settings.clone())?;
            mirrors.push(Arc::downgrade(&shared));
            shared
        }
    };
    drop(mirrors);

    let (tx, rx) = crossbeam_channel::unbounded();
    let mut st = shared.state.lock().unwrap();
    let id = st.next_id;
    st.next_id += 1;
    // The newest session's options win, as they do for live changes.
    if st.settings != settings {
        st.settings = settings;
    }
    st.generation += 1;
    if let Some(cur) = &st.current {
        let _ = tx.send(cur.clone());
    }
    st.subs.push(Sub {
        id,
        tx,
        decoders: decoders.to_vec(),
    });
    let n = st.subs.len();
    drop(st);
    shared.keyframe.store(true, Ordering::Relaxed);
    if n > 1 {
        info!("mirroring {} to {n} displays", shared.description);
    }
    Ok(Subscription { shared, id, rx })
}

impl Subscription {
    pub fn description(&self) -> &str {
        &self.shared.description
    }

    pub fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    /// Changes the encoder settings for every display of this mirror.
    pub fn update(&self, settings: Settings) {
        let mut st = self.shared.state.lock().unwrap();
        if st.settings != settings {
            st.settings = settings;
            st.generation += 1;
        }
    }

    pub fn encode_ms(&self) -> f32 {
        self.shared.encode_us.load(Ordering::Relaxed) as f32 / 1000.0
    }

    /// Input from this display goes to the mirrored screen.
    pub fn input(&self) -> Box<dyn InputSink> {
        Box::new(MirrorInput(self.shared.clone()))
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock().unwrap();
        st.subs.retain(|s| s.id != self.id);
        st.generation += 1;
    }
}

struct MirrorInput(Arc<Shared>);

impl InputSink for MirrorInput {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        self.0.input.lock().unwrap().inject(ev)
    }
}

fn start(req: &ScreenRequest, settings: Settings) -> Result<Arc<Shared>> {
    let screen = open_screen(req).context("opening screen source")?;
    info!("mirroring {}", screen.description);
    let shared = Arc::new(Shared {
        kind: req.kind.clone(),
        description: screen.description,
        state: Mutex::new(State {
            subs: Vec::new(),
            next_id: 0,
            settings,
            generation: 0,
            current: None,
            closed: false,
        }),
        keyframe: AtomicBool::new(true),
        encode_us: AtomicU32::new(0),
        input: Mutex::new(screen.input),
    });
    let refresh_mhz = req.refresh_mhz;
    let s = shared.clone();
    let mut capturer = screen.capturer;
    std::thread::Builder::new()
        .name("mirror".into())
        .spawn(move || {
            if let Err(e) = run(&s, capturer.as_mut(), refresh_mhz) {
                warn!("mirror stopped: {e:#}");
                broadcast(&s, &Event::Failed(format!("{e:#}")));
            }
            let mut st = s.state.lock().unwrap();
            st.closed = true;
            st.current = None;
        })?;
    Ok(shared)
}

fn broadcast(shared: &Shared, ev: &Event) {
    let st = shared.state.lock().unwrap();
    for sub in &st.subs {
        // A session that has fallen behind skips frames; it asks for a
        // keyframe once its display notices the gap.
        if matches!(ev, Event::Frame { .. }) && sub.tx.len() > 8 {
            continue;
        }
        let _ = sub.tx.send(ev.clone());
    }
}

/// Codecs every display of the mirror can decode, best first.
fn common_codecs(st: &State) -> Vec<Codec> {
    let candidates = match st.settings.codec {
        Some(c) => vec![c],
        None => vec![Codec::Hevc, Codec::H264, Codec::Av1],
    };
    candidates
        .into_iter()
        .filter(|c| st.subs.iter().all(|s| s.decoders.contains(c)))
        .filter(|&c| !compiled_encoders(c).is_empty())
        .collect()
}

struct Active {
    encoder: Encoder,
    codec: Codec,
    settings: Settings,
    width: u32,
    height: u32,
    generation: u64,
}

/// The capture thread: capture → encode → every session, until the last
/// session has been gone for a moment.
fn run(
    shared: &Shared,
    capturer: &mut dyn silkscreen_platform::Capturer,
    refresh_mhz: u32,
) -> Result<()> {
    let started = Instant::now();
    let mut active: Option<Active> = None;
    let mut empty_since: Option<Instant> = None;
    let mut last_key = Instant::now() - Duration::from_secs(1);
    let mut encode_total = Duration::ZERO;
    let mut encoded = 0u32;
    let mut pacer = Pacer::new(None);
    loop {
        let (generation, max_fps) = {
            let mut st = shared.state.lock().unwrap();
            if st.subs.is_empty() {
                let since = *empty_since.get_or_insert_with(Instant::now);
                // A session reconnecting (or restarting for new settings)
                // rejoins within this time and keeps the capture.
                if since.elapsed() > Duration::from_secs(3) {
                    st.closed = true;
                    info!("stopped mirroring {}", shared.description);
                    return Ok(());
                }
            } else {
                empty_since = None;
            }
            (st.generation, st.settings.max_fps)
        };
        pacer.set_max_fps(max_fps);
        // With a frame rate limit the encoder is set up for that rate.
        let enc_refresh = max_fps.map_or(refresh_mhz, |f| refresh_mhz.min((f * 1000.0) as u32));
        let want_key = last_key.elapsed() > Duration::from_millis(100)
            && shared.keyframe.swap(false, Ordering::Relaxed);
        let mut encode = |frame: &Frame, key: bool| -> Result<()> {
            let pts = started.elapsed().as_micros() as u64;
            let a = ensure_encoder(shared, &mut active, frame, generation, enc_refresh)?;
            let rgb = rgb_frame(frame, a.width, a.height);
            let t = Instant::now();
            a.encoder.encode(&rgb, pts, key, |data, key| {
                broadcast(
                    shared,
                    &Event::Frame {
                        data: data.into(),
                        key,
                        pts,
                    },
                );
            })?;
            encode_total += t.elapsed();
            encoded += 1;
            Ok(())
        };
        let mut err = None;
        let got = capturer.next_frame(Duration::from_millis(50), &mut |frame| {
            if empty_since.is_none()
                && pacer.admit(&frame, want_key)
                && let Err(e) = encode(&frame, want_key)
            {
                err = Some(e);
            }
        })?;
        if let Some(e) = err {
            return Err(e);
        }
        if empty_since.is_none()
            && let Some(frame) = pacer.take_due()
        {
            encode(&frame, false)?;
        }
        if let Some(a) = active.as_mut() {
            let pts = started.elapsed().as_micros() as u64;
            a.encoder.poll(|data, key| {
                broadcast(
                    shared,
                    &Event::Frame {
                        data: data.into(),
                        key,
                        pts,
                    },
                );
            })?;
            if want_key && !got {
                // Static screen: re-send what is on it as a keyframe.
                a.encoder.encode_again(pts, true, |data, key| {
                    broadcast(
                        shared,
                        &Event::Frame {
                            data: data.into(),
                            key,
                            pts,
                        },
                    );
                })?;
            }
        }
        if want_key {
            last_key = Instant::now();
        }
        if encoded >= 15 {
            let us = encode_total.as_micros() as u32 / encoded;
            shared.encode_us.store(us, Ordering::Relaxed);
            (encode_total, encoded) = (Duration::ZERO, 0);
        }
    }
}

fn rgb_frame<'a>(frame: &Frame<'a>, width: u32, height: u32) -> RgbFrame<'a> {
    RgbFrame {
        width,
        height,
        stride: frame.stride,
        layout: match frame.layout {
            PixelLayout::Bgrx => RgbLayout::Bgrx,
            PixelLayout::Rgbx => RgbLayout::Rgbx,
        },
        data: frame.data,
    }
}

/// (Re)opens the encoder when the captured size, the settings or the set of
/// displays changed, and tells every session about the new stream.
fn ensure_encoder<'a>(
    shared: &Shared,
    active: &'a mut Option<Active>,
    frame: &Frame,
    generation: u64,
    refresh_mhz: u32,
) -> Result<&'a mut Active> {
    let (w, h) = (frame.width & !1, frame.height & !1);
    if let Some(a) = active
        && (a.width, a.height) == (w, h)
        && a.generation == generation
    {
        return Ok(active.as_mut().unwrap());
    }
    let (codecs, settings) = {
        let st = shared.state.lock().unwrap();
        (common_codecs(&st), st.settings.clone())
    };
    // A display joined or left, or settings were re-sent unchanged: keep
    // the encoder if every display can still decode it.
    if let Some(a) = active
        && (a.width, a.height) == (w, h)
        && a.settings == settings
        && codecs.contains(&a.codec)
    {
        a.generation = generation;
        return Ok(active.as_mut().unwrap());
    }
    if codecs.is_empty() {
        bail!("no codec that every mirrored display can decode");
    }
    let mut errors = Vec::new();
    for codec in codecs {
        let bitrate_kbps = settings
            .bitrate_kbps
            .unwrap_or_else(|| default_bitrate_kbps(codec, w, h, refresh_mhz));
        let cfg = EncoderConfig {
            codec,
            width: w,
            height: h,
            refresh_mhz,
            bitrate_kbps,
            encoder: settings.encoder.clone(),
        };
        match Encoder::open(&cfg) {
            Ok(encoder) => {
                let start = Event::Start {
                    codec,
                    width: w,
                    height: h,
                    refresh_mhz,
                    bitrate_kbps,
                    encoder: encoder.name().to_owned(),
                };
                info!(
                    "mirror stream {w}x{h} · {} · {:.1} Mbit/s",
                    encoder.name(),
                    bitrate_kbps as f32 / 1000.0
                );
                shared.state.lock().unwrap().current = Some(start.clone());
                broadcast(shared, &start);
                shared.keyframe.store(true, Ordering::Relaxed);
                return Ok(active.insert(Active {
                    encoder,
                    codec,
                    settings,
                    width: w,
                    height: h,
                    generation,
                }));
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    bail!("cannot encode {w}x{h}:\n{}", errors.join("\n"))
}
