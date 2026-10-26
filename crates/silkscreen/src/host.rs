//! Host role: the computer whose desktop is extended onto a display.
//!
//! Per session: connect → secure handshake (pairing the first time) →
//! receive the display's caps → create a virtual screen matching it →
//! capture → encode → packetize/encrypt → UDP, while a reader thread handles
//! keyframe requests, input and latency probes.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use silkscreen_codec::{
    Encoder, EncoderConfig, RgbFrame, RgbLayout, compiled_encoders, default_bitrate_kbps,
};
use silkscreen_platform::{Frame, InputSink, PixelLayout, ScreenRequest, SourceKind, open_screen};
use silkscreen_proto::{
    Codec, ControlMsg, DEFAULT_CONTROL_PORT, DisplayCaps, InputEvent, Packetizer, StreamConfig,
    VideoKey,
};
use tracing::{debug, info, warn};

use crate::ctx::Ctx;
use crate::discovery;
use crate::mirror;
use crate::net::{now_us, tune_tcp, udp_socket};
use crate::pacer::Pacer;
use crate::secure::{self, Refused, SecureWriter};
use crate::status::{HostPhase, HostState, Shared};

#[derive(Debug, Clone)]
pub struct HostOptions {
    /// Display name (from `silkscreen list`) or address. `None` picks the only
    /// display on the network.
    pub target: Option<String>,
    pub codec: Option<Codec>,
    pub bitrate_kbps: Option<u32>,
    pub encoder: Option<String>,
    pub source: SourceKind,
    /// Override the display's native resolution.
    pub size: Option<(u32, u32)>,
    pub refresh_hz: Option<f32>,
    pub datagram_len: usize,
    /// Keep reconnecting after the display goes away.
    pub persist: bool,
    pub resolution: Resolution,
    /// Send at most this many frames a second (the screen itself keeps its
    /// refresh rate): fewer frames for a slow link.
    pub max_fps: Option<f32>,
    /// Full-resolution colour (4:4:4): sharper coloured text, more data,
    /// software encoders unless the hardware can (NVENC, PyroWave).
    pub chroma444: bool,
}

impl HostOptions {
    /// Changing these means a new virtual monitor or capture: reconnect.
    fn screen_differs(&self, o: &HostOptions) -> bool {
        (&self.source, self.size, self.resolution, self.refresh_hz)
            != (&o.source, o.size, o.resolution, o.refresh_hz)
    }

    /// Changing these means a new encoder (and stream) on the same screen.
    fn encoder_differs(&self, o: &HostOptions) -> bool {
        (
            self.codec,
            &self.encoder,
            self.bitrate_kbps,
            self.datagram_len,
            self.max_fps,
            self.chroma444,
        ) != (
            o.codec,
            &o.encoder,
            o.bitrate_kbps,
            o.datagram_len,
            o.max_fps,
            o.chroma444,
        )
    }
}

/// How a session ended without an error.
enum Ended {
    /// The display disconnected.
    Disconnected,
    /// New settings need a new screen: connect again straight away.
    Restart,
}

/// How many pixels to stream relative to the display's panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Native up to 4K; half (each axis) above that. Beyond ~8 MP per frame
    /// capture, encode and decode times add up to noticeable latency.
    #[default]
    Auto,
    /// The panel's native resolution: sharpest, most latency on huge panels.
    Full,
    /// Half width and height (a quarter of the pixels): lowest latency.
    Half,
}

/// Pixels above which `Auto` halves the resolution (just over 3840x2160).
const AUTO_HALF_ABOVE: u64 = 3840 * 2160;

/// The virtual monitor's size and scale for a display.
fn stream_mode(caps: &DisplayCaps, res: Resolution) -> (u32, u32, f32) {
    let half = match res {
        Resolution::Full => false,
        Resolution::Half => true,
        Resolution::Auto => caps.width as u64 * caps.height as u64 > AUTO_HALF_ABOVE,
    };
    if half {
        // Same physical size of text and UI: half the pixels, half the scale.
        (
            (caps.width / 2) & !1,
            (caps.height / 2) & !1,
            (caps.scale / 2.0).max(1.0),
        )
    } else {
        (caps.width, caps.height, caps.scale)
    }
}

/// Where the pairing code comes from when a display is not yet paired.
pub enum CodeSource {
    /// Ask on the terminal.
    Stdin,
    /// A code given up front (`--code`, tests).
    Fixed(String),
    /// Typed into the GUI.
    Channel(Receiver<String>),
}

/// Controls a running host session from another thread (the GUI).
pub struct HostHandle {
    pub state: Arc<Shared<HostState>>,
    stop: Arc<AtomicBool>,
    conn: Arc<Mutex<Option<TcpStream>>>,
    code_tx: crossbeam_channel::Sender<String>,
    update: Arc<Mutex<Option<HostOptions>>>,
}

impl HostHandle {
    /// Starts hosting in the background.
    pub fn spawn(opts: HostOptions, ctx: Arc<Ctx>) -> Self {
        let state = Shared::<HostState>::new();
        let stop = Arc::new(AtomicBool::new(false));
        let conn = Arc::new(Mutex::new(None));
        let (code_tx, code_rx) = crossbeam_channel::bounded(1);
        let update = Arc::new(Mutex::new(None));
        let control = Control {
            state: state.clone(),
            stop: stop.clone(),
            conn: conn.clone(),
            update: update.clone(),
        };
        std::thread::Builder::new()
            .name("host".into())
            .spawn(move || run(opts, ctx, CodeSource::Channel(code_rx), control))
            .expect("spawn host thread");
        HostHandle {
            state,
            stop,
            conn,
            code_tx,
            update,
        }
    }

    /// Applies new options to the running session (the target stays):
    /// encoder settings switch in place, screen settings reconnect.
    pub fn update(&self, opts: HostOptions) {
        *self.update.lock().unwrap() = Some(opts);
    }

    /// Answers a pairing request with the code the display shows.
    pub fn submit_code(&self, code: &str) {
        let _ = self.code_tx.try_send(code.to_owned());
    }

    /// Ends the session (unplugging the virtual monitor) and stops retrying.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(c) = self.conn.lock().unwrap().as_ref() {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state.get().phase, HostPhase::Stopped(_))
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Shared between the host thread and its [`HostHandle`].
#[derive(Clone)]
pub struct Control {
    pub state: Arc<Shared<HostState>>,
    pub stop: Arc<AtomicBool>,
    conn: Arc<Mutex<Option<TcpStream>>>,
    /// New options from [`HostHandle::update`], not yet applied.
    update: Arc<Mutex<Option<HostOptions>>>,
}

impl Control {
    pub fn standalone() -> Self {
        Control {
            state: Shared::new(),
            stop: Arc::new(AtomicBool::new(false)),
            conn: Arc::default(),
            update: Arc::default(),
        }
    }

    /// Pending new options, keeping the current target.
    fn take_update(&self, current: &HostOptions) -> Option<HostOptions> {
        let mut new = self.update.lock().unwrap().take()?;
        new.target = current.target.clone();
        Some(new)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn phase(&self, phase: HostPhase) {
        self.state.update(|s| s.phase = phase);
    }
}

pub fn run(
    mut opts: HostOptions,
    ctx: Arc<Ctx>,
    mut codes: CodeSource,
    control: Control,
) -> Result<()> {
    let mut last_err = None;
    while !control.stopped() {
        if let Some(new) = control.take_update(&opts) {
            opts = new;
        }
        control.phase(HostPhase::Connecting(
            opts.target.clone().unwrap_or_else(|| "network".into()),
        ));
        let result = resolve_target(opts.target.as_deref())
            .and_then(|addr| run_session(addr, &mut opts, &ctx, &mut codes, &control));
        match result {
            Ok(Ended::Restart) => {
                control.phase(HostPhase::Reconnecting("applying new settings".into()));
                continue;
            }
            Ok(Ended::Disconnected) => info!("display disconnected"),
            Err(e) => {
                // Refusals (wrong code, paused, cancelled) need the user, not a retry.
                if let Some(r) = e.downcast_ref::<Refused>() {
                    warn!("{r}");
                    control.phase(HostPhase::Stopped(Some(r.to_string())));
                    return Err(e);
                }
                warn!("session ended: {e:#}");
                last_err = Some(format!("{e:#}"));
            }
        }
        if !opts.persist || control.stopped() {
            break;
        }
        control.phase(HostPhase::Reconnecting(
            last_err
                .clone()
                .unwrap_or_else(|| "display disconnected".into()),
        ));
        for _ in 0..20 {
            if control.stopped() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        info!("reconnecting…");
    }
    control.phase(HostPhase::Stopped(if control.stopped() {
        None
    } else {
        last_err
    }));
    Ok(())
}

fn read_code(codes: &mut CodeSource, display: &str, control: &Control) -> Result<String> {
    match codes {
        CodeSource::Stdin => secure::prompt_pin_stdin(display),
        CodeSource::Fixed(c) => Ok(c.clone()),
        CodeSource::Channel(rx) => {
            while rx.try_recv().is_ok() {} // drop stale input
            control.phase(HostPhase::NeedCode(display.to_owned()));
            loop {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(code) => return Ok(code),
                    Err(RecvTimeoutError::Timeout) if !control.stopped() => {}
                    _ => return Err(Refused("pairing cancelled".into()).into()),
                }
            }
        }
    }
}

fn resolve_target(target: Option<&str>) -> Result<SocketAddr> {
    if let Some(t) = target {
        if let Ok(addr) = t.parse::<SocketAddr>() {
            return Ok(addr);
        }
        if let Ok(ip) = t.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, DEFAULT_CONTROL_PORT));
        }
    }
    let found = discovery::browse(Duration::from_secs(2))?;
    let matches: Vec<_> = match target {
        Some(t) => {
            let t = t.to_lowercase();
            found
                .iter()
                .filter(|d| d.name.to_lowercase().contains(&t))
                .collect()
        }
        None => found.iter().collect(),
    };
    match matches.as_slice() {
        [one] => {
            info!("found display \"{}\" at {}", one.name, one.addr);
            Ok(one.addr)
        }
        [] => {
            // Not on mDNS: maybe a hostname.
            if let Some(t) = target
                && let Ok(mut addrs) = (t, DEFAULT_CONTROL_PORT).to_socket_addrs()
                && let Some(addr) = addrs.find(SocketAddr::is_ipv4)
            {
                return Ok(addr);
            }
            bail!("no display found on the network (is `silkscreen display` running there?)")
        }
        many => bail!(
            "several displays found, pick one with --to: {}",
            many.iter()
                .map(|d| format!("\"{}\"", d.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Codecs to try, in the display's order of preference, that we can encode.
fn codec_candidates(
    caps: &DisplayCaps,
    forced: Option<Codec>,
    chroma444: bool,
) -> Result<Vec<Codec>> {
    if let Some(c) = forced {
        if !caps.decoders.contains(&c) {
            return Err(Refused(format!("\"{}\" can't decode {}", caps.name, c.name())).into());
        }
        return Ok(vec![c]);
    }
    // PyroWave is experimental and needs Ethernet-class bandwidth: only
    // when picked.
    let mut codecs: Vec<_> = caps
        .decoders
        .iter()
        .copied()
        .filter(|&c| c != Codec::PyroWave && !compiled_encoders(c).is_empty())
        .collect();
    // 4:4:4 mostly means software encoding, where x264 is much faster
    // than x265.
    if chroma444 {
        codecs.sort_by_key(|&c| c != Codec::H264);
    }
    if codecs.is_empty() {
        bail!("no codec in common with the display ({:?})", caps.decoders);
    }
    Ok(codecs)
}

fn run_session(
    addr: SocketAddr,
    opts: &mut HostOptions,
    ctx: &Ctx,
    codes: &mut CodeSource,
    control: &Control,
) -> Result<Ended> {
    info!("connecting to {addr}");
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).context("connecting")?;
    tune_tcp(&tcp)?;
    *control.conn.lock().unwrap() = Some(tcp.try_clone()?);
    let channel = secure::connect(
        tcp,
        &ctx.identity,
        &ctx.paired,
        &ctx.name(),
        &mut |display| read_code(codes, display, control),
    )?;
    info!(
        "secure session with \"{}\"{}",
        channel.peer_name,
        if channel.newly_paired {
            " (newly paired)"
        } else {
            ""
        }
    );
    control.state.update(|s| {
        s.display = Some(channel.peer_name.clone());
        s.phase = HostPhase::Connecting(channel.peer_name.clone());
    });
    let writer = channel.writer()?;
    let mut reader = channel.reader()?;

    channel
        .stream
        .set_read_timeout(Some(Duration::from_secs(10)))?;
    let caps = match reader.recv()? {
        ControlMsg::Caps(c) => c,
        ControlMsg::Reject { reason } => return Err(Refused(reason).into()),
        other => bail!("unexpected first message {other:?}"),
    };
    channel.stream.set_read_timeout(None)?;
    info!(
        "display \"{}\": {}x{} @ {:.2} Hz, scale {}, touch: {} ({} points), decoders {:?}",
        caps.name,
        caps.width,
        caps.height,
        caps.refresh_mhz as f32 / 1000.0,
        caps.scale,
        caps.touch,
        caps.max_touch_points,
        caps.decoders
    );

    let mut codecs = codec_candidates(&caps, opts.codec, opts.chroma444)?;
    let (mode_w, mode_h, scale) = stream_mode(&caps, opts.resolution);
    if (mode_w, mode_h) != (caps.width, caps.height) && opts.size.is_none() {
        info!(
            "streaming at {mode_w}x{mode_h} (half of {}x{}) for low latency; use full resolution for the sharpest picture",
            caps.width, caps.height
        );
    }
    let (width, height) = opts.size.unwrap_or((mode_w, mode_h));
    let refresh_mhz = opts
        .refresh_hz
        .map(|hz| (hz * 1000.0) as u32)
        .unwrap_or(caps.refresh_mhz);

    let req = ScreenRequest {
        name: format!("Silkscreen – {}", caps.name),
        width,
        height,
        refresh_mhz,
        scale,
        physical_size_mm: caps.physical_size_mm,
        kind: opts.source.clone(),
        touch: caps.touch,
    };
    // A virtual monitor belongs to this display alone; an existing screen
    // (or the test pattern) is captured once and mirrored to every display
    // showing it.
    let (mirror, mut capturer, input, description) = if opts.source == SourceKind::Virtual {
        let screen = open_screen(&req).context("opening screen source")?;
        (
            None,
            Some(screen.capturer),
            screen.input,
            screen.description,
        )
    } else {
        let sub = mirror::join(&req, &caps.decoders, mirror_settings(opts))
            .map_err(|e| Refused(format!("{e:#}")))?;
        let (input, description) = (sub.input(), sub.description().to_owned());
        (Some(sub), None, input, description)
    };
    info!("streaming {description}");
    control.state.update(|s| s.source = Some(description));

    let udp = udp_socket(SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 0))?;
    udp.connect(SocketAddr::new(addr.ip(), caps.video_port))?;
    let video_key = channel.video_key.clone();

    // Reader thread: everything the display sends us.
    let stop = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(true));
    let rtt_us = Arc::new(AtomicU64::new(0));
    let ended_by_display = Arc::new(AtomicBool::new(false));
    let heard = Heartbeat::new();
    let reader_thread = {
        let ended_by_display = ended_by_display.clone();
        let heard = heard.clone();
        let (stop, keyframe, rtt_us, writer, state) = (
            stop.clone(),
            keyframe.clone(),
            rtt_us.clone(),
            writer.clone(),
            control.state.clone(),
        );
        std::thread::Builder::new()
            .name("host-control".into())
            .spawn(move || {
                let mut input = KeyTracker::new(input);
                loop {
                    let msg = reader.recv();
                    if msg.is_ok() {
                        heard.beat();
                    }
                    match msg {
                        Ok(ControlMsg::RequestKeyframe) => keyframe.store(true, Ordering::Relaxed),
                        Ok(ControlMsg::Input(ev)) => input.inject(&ev),
                        Ok(ControlMsg::Ping { t_us }) => {
                            let _ = writer.send(&ControlMsg::Pong { t_us });
                        }
                        Ok(ControlMsg::Pong { t_us }) => {
                            rtt_us.store(now_us().saturating_sub(t_us), Ordering::Relaxed);
                        }
                        Ok(ControlMsg::Stats(s)) => {
                            debug!("display stats: {s:?}");
                            state.update(|st| st.receiver = s);
                        }
                        Ok(ControlMsg::Goodbye) => break,
                        Ok(ControlMsg::EndedByDisplay) => {
                            ended_by_display.store(true, Ordering::Relaxed);
                            break;
                        }
                        Ok(other) => debug!("ignoring {other:?}"),
                        Err(e) => {
                            debug!("control read: {e}");
                            break;
                        }
                    }
                }
                stop.store(true, Ordering::Relaxed);
            })?
    };

    let mut stream: Option<ActiveStream> = None;
    let mut pacer = Pacer::new(opts.max_fps);
    let started = Instant::now();
    let mut last_ping = Instant::now();
    let mut stats = Throughput::new();
    let mut last_keyframe_sent = Instant::now() - Duration::from_secs(1);

    let result: Result<Ended> = if let Some(sub) = &mirror {
        mirror_loop(
            sub, opts, control, &writer, &udp, &video_key, &stop, &keyframe, &rtt_us, &heard,
        )
    } else {
        let capturer = capturer.as_mut().unwrap();
        (|| {
            while !stop.load(Ordering::Relaxed) && !control.stopped() {
                if let Some(new) = control.take_update(opts) {
                    if new.screen_differs(opts) {
                        info!("new stream settings: reconnecting with a new screen");
                        *opts = new;
                        return Ok(Ended::Restart);
                    }
                    if new.encoder_differs(opts) {
                        info!("new stream settings: restarting the encoder");
                        codecs = codec_candidates(&caps, new.codec, new.chroma444)?;
                        stream = None;
                        pacer.set_max_fps(new.max_fps);
                    }
                    *opts = new;
                }

                // Rate-limit IDRs: on a lossy link back-to-back keyframes only make it worse.
                let want_key = keyframe.load(Ordering::Relaxed)
                    && last_keyframe_sent.elapsed() > Duration::from_millis(100)
                    && keyframe.swap(false, Ordering::Relaxed);

                // With a frame rate limit the encoder is set up for that rate.
                let enc_refresh = opts
                    .max_fps
                    .map_or(refresh_mhz, |f| refresh_mhz.min((f * 1000.0) as u32));
                let mut encode = |frame: &Frame, key: bool| -> Result<()> {
                    let pts = started.elapsed().as_micros() as u64;
                    let s = ensure_stream(
                        &mut stream,
                        frame,
                        &codecs,
                        enc_refresh,
                        opts,
                        &writer,
                        &video_key,
                        control,
                    )?;
                    let key = key || s.fresh;
                    s.fresh = false;
                    s.encode(frame, pts, key, &udp, &mut stats)
                };
                let mut err = None;
                let got = capturer.next_frame(Duration::from_millis(50), &mut |frame| {
                    if pacer.admit(&frame, want_key)
                        && let Err(e) = encode(&frame, want_key)
                    {
                        err = Some(e);
                    }
                })?;
                if let Some(e) = err {
                    return Err(e);
                }
                if let Some(frame) = pacer.take_due() {
                    encode(&frame, false)?;
                }
                if let Some(s) = stream.as_mut() {
                    s.poll(started.elapsed().as_micros() as u64, &udp, &mut stats)?;
                }
                if want_key {
                    last_keyframe_sent = Instant::now();
                    if !got && let Some(s) = stream.as_mut() {
                        // Static screen: re-send what is already on it as an IDR.
                        let pts = started.elapsed().as_micros() as u64;
                        s.encode_again(pts, &udp, &mut stats)?;
                    }
                }

                if last_ping.elapsed() >= Duration::from_secs(1) {
                    last_ping = Instant::now();
                    heard.check()?;
                    writer.send(&ControlMsg::Ping { t_us: now_us() })?;
                    if let Some(s) = &stream {
                        stats.report(
                            s.encoder.name(),
                            rtt_us.load(Ordering::Relaxed),
                            &control.state,
                        );
                    }
                }
            }
            Ok(Ended::Disconnected)
        })()
    };

    let _ = writer.send(&ControlMsg::Goodbye);
    writer.shutdown();
    let _ = reader_thread.join();
    *control.conn.lock().unwrap() = None;
    control.state.update(|s| s.stream = None);
    if ended_by_display.load(Ordering::Relaxed) {
        return Err(Refused("disconnected from the display".into()).into());
    }
    result
}

/// When the display was last heard from. It answers the once-a-second pings,
/// so a long silence means it is gone (asleep, off the network) even if the
/// TCP connection has not noticed yet — which can take many minutes.
#[derive(Clone)]
struct Heartbeat(Arc<Mutex<Instant>>);

/// How long a display may stay silent before the session is given up and
/// reconnected.
const SILENCE_LIMIT: Duration = Duration::from_secs(8);

impl Heartbeat {
    fn new() -> Self {
        Heartbeat(Arc::new(Mutex::new(Instant::now())))
    }

    fn beat(&self) {
        *self.0.lock().unwrap() = Instant::now();
    }

    fn check(&self) -> Result<()> {
        let silent = self.0.lock().unwrap().elapsed();
        if silent > SILENCE_LIMIT {
            bail!(
                "the display stopped responding ({}s without a reply)",
                silent.as_secs()
            );
        }
        Ok(())
    }
}

fn mirror_settings(opts: &HostOptions) -> mirror::Settings {
    mirror::Settings {
        codec: opts.codec,
        encoder: opts.encoder.clone(),
        bitrate_kbps: opts.bitrate_kbps,
        max_fps: opts.max_fps,
        chroma444: opts.chroma444,
    }
}

/// Sends a shared mirror's stream to this session's display.
#[allow(clippy::too_many_arguments)]
fn mirror_loop(
    sub: &mirror::Subscription,
    opts: &mut HostOptions,
    control: &Control,
    writer: &SecureWriter,
    udp: &std::net::UdpSocket,
    video_key: &VideoKey,
    stop: &AtomicBool,
    keyframe: &AtomicBool,
    rtt_us: &AtomicU64,
    heard: &Heartbeat,
) -> Result<Ended> {
    let mut packetizer: Option<Packetizer> = None;
    let mut stream_id = 0;
    let mut encoder = String::new();
    let mut stats = Throughput::new();
    let mut last_ping = Instant::now();
    while !stop.load(Ordering::Relaxed) && !control.stopped() {
        if let Some(new) = control.take_update(opts) {
            if new.screen_differs(opts) {
                info!("new stream settings: reconnecting with a new screen");
                *opts = new;
                return Ok(Ended::Restart);
            }
            if new.encoder_differs(opts) {
                info!("new stream settings for the mirror");
                sub.update(mirror_settings(&new));
                if new.datagram_len != opts.datagram_len && packetizer.is_some() {
                    packetizer = Some(Packetizer::new(
                        stream_id,
                        new.datagram_len,
                        Some(video_key),
                    ));
                }
            }
            *opts = new;
        }
        if keyframe.swap(false, Ordering::Relaxed) {
            sub.request_keyframe();
        }
        match sub.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(mirror::Event::Start {
                codec,
                width,
                height,
                refresh_mhz,
                bitrate_kbps,
                encoder: name,
            }) => {
                stream_id = rand::random::<u32>();
                writer.send(&ControlMsg::StartStream(StreamConfig {
                    codec,
                    width,
                    height,
                    refresh_mhz,
                    bitrate_kbps,
                    stream_id,
                }))?;
                packetizer = Some(Packetizer::new(
                    stream_id,
                    opts.datagram_len,
                    Some(video_key),
                ));
                let desc = format!(
                    "{width}x{height} @ {:.2} Hz · {name} · {:.1} Mbit/s (mirrored)",
                    refresh_mhz as f32 / 1000.0,
                    bitrate_kbps as f32 / 1000.0
                );
                info!("stream {desc}");
                control.state.update(|s| {
                    s.stream = Some(desc);
                    s.phase = HostPhase::Streaming;
                });
                encoder = name;
            }
            Ok(mirror::Event::Frame { data, key, pts }) => {
                if let Some(p) = packetizer.as_mut() {
                    send_frame(p, udp, &data, key, pts)?;
                    stats.frame(data.len(), key);
                }
            }
            Ok(mirror::Event::Failed(e)) => bail!("{e}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("the mirror stopped"),
        }
        if last_ping.elapsed() >= Duration::from_secs(1) {
            last_ping = Instant::now();
            heard.check()?;
            writer.send(&ControlMsg::Ping { t_us: now_us() })?;
            if packetizer.is_some() {
                stats.set_encode_ms(sub.encode_ms());
                stats.report(&encoder, rtt_us.load(Ordering::Relaxed), &control.state);
            }
        }
    }
    Ok(Ended::Disconnected)
}

/// Wraps the input sink to remember held keys and buttons, and releases them
/// when the session ends — a dropped connection must not leave Shift (or a
/// mouse button) stuck down on the host.
struct KeyTracker {
    sink: Box<dyn InputSink>,
    keys: BTreeSet<u16>,
    buttons: Vec<silkscreen_proto::MouseButton>,
}

impl KeyTracker {
    fn new(sink: Box<dyn InputSink>) -> Self {
        KeyTracker {
            sink,
            keys: BTreeSet::new(),
            buttons: Vec::new(),
        }
    }

    fn inject(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key {
                evdev_code,
                pressed: true,
            } => {
                self.keys.insert(evdev_code);
            }
            InputEvent::Key {
                evdev_code,
                pressed: false,
            } => {
                self.keys.remove(&evdev_code);
            }
            InputEvent::MouseButton {
                button,
                pressed: true,
            } => self.buttons.push(button),
            InputEvent::MouseButton {
                button,
                pressed: false,
            } => self.buttons.retain(|b| *b != button),
            _ => {}
        }
        if let Err(e) = self.sink.inject(ev) {
            debug!("input injection failed: {e:#}");
        }
    }
}

impl Drop for KeyTracker {
    fn drop(&mut self) {
        for code in std::mem::take(&mut self.keys) {
            let _ = self.sink.inject(&InputEvent::Key {
                evdev_code: code,
                pressed: false,
            });
        }
        for button in std::mem::take(&mut self.buttons) {
            let _ = self.sink.inject(&InputEvent::MouseButton {
                button,
                pressed: false,
            });
        }
    }
}

struct ActiveStream {
    encoder: Encoder,
    packetizer: Packetizer,
    width: u32,
    height: u32,
    /// Next frame must be a keyframe (stream just (re)started).
    fresh: bool,
}

/// (Re)creates the encoder whenever the captured size changes — e.g. the
/// user picked a different resolution for the virtual monitor.
#[allow(clippy::too_many_arguments)]
fn ensure_stream<'a>(
    stream: &'a mut Option<ActiveStream>,
    frame: &Frame,
    codecs: &[Codec],
    refresh_mhz: u32,
    opts: &HostOptions,
    writer: &SecureWriter,
    video_key: &VideoKey,
    control: &Control,
) -> Result<&'a mut ActiveStream> {
    // Encoders need even dimensions; drop a trailing odd row/column.
    let (w, h) = (frame.width & !1, frame.height & !1);
    if stream
        .as_ref()
        .is_some_and(|s| (s.width, s.height) == (w, h))
    {
        return Ok(stream.as_mut().unwrap());
    }
    // Fall through codecs: e.g. most H.264 hardware encoders stop at 4096
    // pixels wide, while HEVC goes to 8192.
    let mut errors = Vec::new();
    let opened = codecs.iter().find_map(|&codec| {
        let bitrate_kbps = opts
            .bitrate_kbps
            .unwrap_or_else(|| default_bitrate_kbps(codec, w, h, refresh_mhz));
        let cfg = EncoderConfig {
            codec,
            width: w,
            height: h,
            refresh_mhz,
            bitrate_kbps,
            encoder: opts.encoder.clone(),
            chroma444: opts.chroma444,
        };
        Encoder::open(&cfg)
            .map_err(|e| errors.push(format!("{e:#}")))
            .ok()
            .map(|e| (e, codec, bitrate_kbps))
    });
    let Some((encoder, codec, bitrate_kbps)) = opened else {
        bail!("cannot encode {w}x{h}:\n{}", errors.join("\n"));
    };
    let stream_id = rand::random::<u32>();
    writer.send(&ControlMsg::StartStream(StreamConfig {
        codec,
        width: w,
        height: h,
        refresh_mhz,
        bitrate_kbps,
        stream_id,
    }))?;
    let desc = format!(
        "{w}x{h} @ {:.2} Hz · {} · {:.1} Mbit/s",
        refresh_mhz as f32 / 1000.0,
        encoder.name(),
        bitrate_kbps as f32 / 1000.0
    );
    info!("stream {desc}");
    control.state.update(|s| {
        s.stream = Some(desc);
        s.phase = HostPhase::Streaming;
    });
    Ok(stream.insert(ActiveStream {
        encoder,
        packetizer: Packetizer::new(stream_id, opts.datagram_len, Some(video_key)),
        width: w,
        height: h,
        fresh: true,
    }))
}

impl ActiveStream {
    fn encode(
        &mut self,
        frame: &Frame,
        pts: u64,
        key: bool,
        udp: &std::net::UdpSocket,
        stats: &mut Throughput,
    ) -> Result<()> {
        let rgb = RgbFrame {
            width: self.width,
            height: self.height,
            stride: frame.stride,
            layout: match frame.layout {
                PixelLayout::Bgrx => RgbLayout::Bgrx,
                PixelLayout::Rgbx => RgbLayout::Rgbx,
            },
            data: frame.data,
        };
        let t = Instant::now();
        let packetizer = &mut self.packetizer;
        let mut send_err = None;
        self.encoder.encode(&rgb, pts, key, |data, is_key| {
            if let Err(e) = send_frame(packetizer, udp, data, is_key, pts) {
                send_err = Some(e);
            }
            stats.frame(data.len(), is_key);
        })?;
        stats.encode_time(t.elapsed());
        send_err.map_or(Ok(()), Err)
    }

    /// Sends packets an asynchronous encoder finished after `encode` returned.
    fn poll(&mut self, pts: u64, udp: &std::net::UdpSocket, stats: &mut Throughput) -> Result<()> {
        let packetizer = &mut self.packetizer;
        let mut send_err = None;
        self.encoder.poll(|data, is_key| {
            if let Err(e) = send_frame(packetizer, udp, data, is_key, pts) {
                send_err = Some(e);
            }
            stats.frame(data.len(), is_key);
        })?;
        send_err.map_or(Ok(()), Err)
    }

    fn encode_again(
        &mut self,
        pts: u64,
        udp: &std::net::UdpSocket,
        stats: &mut Throughput,
    ) -> Result<()> {
        let packetizer = &mut self.packetizer;
        let mut send_err = None;
        self.encoder.encode_again(pts, true, |data, is_key| {
            if let Err(e) = send_frame(packetizer, udp, data, is_key, pts) {
                send_err = Some(e);
            }
            stats.frame(data.len(), is_key);
        })?;
        send_err.map_or(Ok(()), Err)
    }
}

/// ENOBUFS (macOS, Linux), WSAENOBUFS, or a would-block.
fn send_queue_full(e: &std::io::Error) -> bool {
    let enobufs = if cfg!(windows) {
        10055
    } else if cfg!(target_os = "macos") {
        55
    } else {
        105
    };
    e.raw_os_error() == Some(enobufs) || e.kind() == std::io::ErrorKind::WouldBlock
}

fn send_frame(
    p: &mut Packetizer,
    udp: &std::net::UdpSocket,
    data: &[u8],
    key: bool,
    pts: u64,
) -> Result<()> {
    p.packetize(data, key, pts, |d| match udp.send(d) {
        Ok(_) => Ok(()),
        // The display's socket is momentarily gone (restarting); not fatal.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(()),
        // The link (Wi-Fi) can't keep up and the outgoing queue is full:
        // drop the packet like the network would; the display asks for a
        // keyframe. Ending the session would be worse.
        Err(e) if send_queue_full(&e) => {
            debug!("send queue full, dropped a packet");
            Ok(())
        }
        Err(e) => Err(e),
    })?;
    Ok(())
}

/// Once-a-second log line with frame rate, bitrate and encode time.
struct Throughput {
    since: Instant,
    frames: u32,
    keyframes: u32,
    bytes: usize,
    encode_total: Duration,
}

impl Throughput {
    fn new() -> Self {
        Throughput {
            since: Instant::now(),
            frames: 0,
            keyframes: 0,
            bytes: 0,
            encode_total: Duration::ZERO,
        }
    }

    fn frame(&mut self, bytes: usize, key: bool) {
        self.frames += 1;
        self.keyframes += key as u32;
        self.bytes += bytes;
    }

    fn encode_time(&mut self, d: Duration) {
        self.encode_total += d;
    }

    /// For a mirror, which encodes elsewhere: the average per frame.
    fn set_encode_ms(&mut self, ms: f32) {
        self.encode_total = Duration::from_secs_f64(ms as f64 / 1000.0 * self.frames as f64);
    }

    fn report(&mut self, encoder: &str, rtt_us: u64, state: &Shared<HostState>) {
        let secs = self.since.elapsed().as_secs_f64();
        let enc_ms = if self.frames > 0 {
            self.encode_total.as_secs_f64() * 1000.0 / self.frames as f64
        } else {
            0.0
        };
        state.update(|s| {
            s.fps = (self.frames as f64 / secs) as f32;
            s.mbps = (self.bytes as f64 * 8.0 / secs / 1e6) as f32;
            s.encode_ms = enc_ms as f32;
            s.rtt_ms = rtt_us as f32 / 1000.0;
            s.keyframes = self.keyframes;
        });
        info!(
            "{:5.1} fps  {:6.1} Mbit/s  {} IDR  encode {:4.1} ms ({encoder})  rtt {:.1} ms",
            self.frames as f64 / secs,
            self.bytes as f64 * 8.0 / secs / 1e6,
            self.keyframes,
            enc_ms,
            rtt_us as f64 / 1000.0
        );
        *self = Throughput::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(w: u32, h: u32, scale: f32) -> DisplayCaps {
        DisplayCaps {
            name: "d".into(),
            width: w,
            height: h,
            refresh_mhz: 60_000,
            scale,
            physical_size_mm: None,
            touch: false,
            max_touch_points: 0,
            pen: false,
            decoders: vec![Codec::Hevc],
            hdr: false,
            video_port: 1,
        }
    }

    #[test]
    fn auto_halves_only_above_4k() {
        assert_eq!(
            stream_mode(&caps(4500, 3000, 2.0), Resolution::Auto),
            (2250, 1500, 1.0)
        );
        assert_eq!(
            stream_mode(&caps(3840, 2160, 2.0), Resolution::Auto),
            (3840, 2160, 2.0)
        );
        assert_eq!(
            stream_mode(&caps(4500, 3000, 2.0), Resolution::Full),
            (4500, 3000, 2.0)
        );
        assert_eq!(
            stream_mode(&caps(2560, 1600, 1.0), Resolution::Half),
            (1280, 800, 1.0)
        );
    }
}
