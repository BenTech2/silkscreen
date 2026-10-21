//! Display-side networking: accepts one host at a time, receives video on a
//! dedicated thread and hands complete frames to a [`FrameSink`] (FFmpeg on
//! desktops, MediaCodec on Android), and forwards input events to the host.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use silkscreen_proto::{
    ControlMsg, DisplayCaps, EncodedFrame, InputEvent, Reassembler, ReceiverStats, StreamConfig,
    VideoKey,
};
use tracing::{debug, info, warn};

use crate::ctx::Ctx;
use crate::net::{tune_tcp, udp_socket};
use crate::secure::{self, PairingEvent, SecureWriter};
use crate::status::{DisplayPhase, DisplayState, Shared};

/// Decodes and shows a display's video. All calls come from one thread, in
/// order: `start`, any number of `frame`s, `stop`.
pub trait FrameSink: Send + Sync {
    /// A new stream begins (from `host`); set up a decoder for it. Returns a
    /// description of the decoder for the debug screen.
    fn start(&self, cfg: &StreamConfig, host: &str) -> Result<String>;
    /// One complete, decodable access unit. An error makes the display
    /// request a keyframe.
    fn frame(&self, frame: &EncodedFrame) -> Result<()>;
    /// The stream ended.
    fn stop(&self);
}

pub struct ServiceOptions {
    pub port: u16,
    /// Use this pairing code instead of a random one (tests, scripted setups).
    pub fixed_code: Option<String>,
}

/// Handle the UI uses to talk to the display service. Dropping it stops the
/// service and frees its ports.
pub struct ServiceHandle {
    input: Sender<InputEvent>,
    shared: Arc<ServiceShared>,
    pub state: Arc<Shared<DisplayState>>,
}

struct ServiceShared {
    stop: AtomicBool,
    /// Skip ahead when the picture is this many ms behind; 0 = never.
    catch_up_ms: AtomicU32,
    /// A host is connected (or pairing).
    busy: AtomicBool,
    current: Mutex<Option<SecureWriter>>,
}

impl ServiceHandle {
    /// Forwards input to the connected host (dropped when none is).
    pub fn send_input(&self, ev: InputEvent) {
        let _ = self.input.send(ev);
    }

    /// Lets the display skip queued frames (to the next keyframe) whenever
    /// the picture falls more than `behind` behind the host, e.g. on a slow
    /// Wi-Fi link; `None` shows every frame however late. Takes effect
    /// immediately.
    pub fn set_catch_up(&self, behind: Option<Duration>) {
        let ms = behind.map_or(0, |d| d.as_millis().clamp(1, u32::MAX as u128) as u32);
        self.shared.catch_up_ms.store(ms, Ordering::Relaxed);
    }

    /// Ends the current session, if any. The host is told the display's
    /// user ended it, so it waits to be reconnected instead of retrying; the
    /// display stays available.
    pub fn disconnect(&self) {
        if let Some(w) = self.shared.current.lock().unwrap().as_ref() {
            let _ = w.send(&ControlMsg::EndedByDisplay);
            w.shutdown();
        }
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.disconnect();
        self.state.update(|s| *s = DisplayState::default());
    }
}

enum VideoCtl {
    Start {
        cfg: StreamConfig,
        host: String,
        peer: IpAddr,
        writer: SecureWriter,
        key: VideoKey,
    },
    Stop,
}

pub fn spawn(
    opts: ServiceOptions,
    caps: DisplayCaps,
    ctx: Arc<Ctx>,
    sink: Arc<dyn FrameSink>,
) -> Result<ServiceHandle> {
    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), opts.port))
        .with_context(|| {
            format!(
                "listening on TCP port {} (is another display running?)",
                opts.port
            )
        })?;
    listener.set_nonblocking(true)?;
    let udp = udp_socket(SocketAddr::new(
        Ipv4Addr::UNSPECIFIED.into(),
        caps.video_port,
    ))
    .with_context(|| format!("binding UDP port {}", caps.video_port))?;
    udp.set_read_timeout(Some(Duration::from_millis(20)))?;
    info!(
        "waiting for a host on port {} (video on {})",
        opts.port, caps.video_port
    );

    let state = Shared::<DisplayState>::new();
    state.update(|s| s.phase = DisplayPhase::Waiting);
    let shared = Arc::new(ServiceShared {
        stop: AtomicBool::new(false),
        catch_up_ms: AtomicU32::new(0),
        busy: AtomicBool::new(false),
        current: Mutex::new(None),
    });
    let (video_tx, video_rx) = crossbeam_channel::unbounded();
    let (input_tx, input_rx) = crossbeam_channel::unbounded::<InputEvent>();

    {
        let (sink, state, shared) = (sink.clone(), state.clone(), shared.clone());
        std::thread::Builder::new()
            .name("display-video".into())
            .spawn(move || video_thread(udp, video_rx, sink, state, &shared))?;
    }
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("display-input".into())
            .spawn(move || {
                for ev in input_rx {
                    if let Some(w) = shared.current.lock().unwrap().as_ref() {
                        let _ = w.send(&ControlMsg::Input(ev));
                    }
                }
            })?;
    }
    {
        let (shared, state) = (shared.clone(), state.clone());
        let fixed_code = opts.fixed_code.clone();
        std::thread::Builder::new()
            .name("display-accept".into())
            .spawn(move || {
                while !shared.stop.load(Ordering::Relaxed) {
                    let conn = match listener.accept() {
                        Ok((c, _)) => c,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                        Err(_) => continue,
                    };
                    let _ = conn.set_nonblocking(false);
                    let (caps, ctx, shared, state, video_tx, code) = (
                        caps.clone(),
                        ctx.clone(),
                        shared.clone(),
                        state.clone(),
                        video_tx.clone(),
                        fixed_code.clone(),
                    );
                    std::thread::spawn(move || {
                        let peer = conn.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                        let r = session(
                            conn,
                            &caps,
                            &ctx,
                            &shared,
                            &state,
                            &video_tx,
                            code.as_deref(),
                        );
                        match r {
                            Ok(Some(())) => info!("host {peer} disconnected"),
                            Ok(None) => {} // refused before the session started
                            Err(e) if is_disconnect(&e) => info!("host {peer} went away"),
                            Err(e) => warn!("host {peer}: {e:#}"),
                        }
                    });
                }
            })?;
    }

    Ok(ServiceHandle {
        input: input_tx,
        shared,
        state,
    })
}

fn is_disconnect(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.downcast_ref::<silkscreen_proto::FrameError>(),
        Some(silkscreen_proto::FrameError::Io(io)) if matches!(io.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted)
    )
}

/// Runs one host connection. `Ok(None)` means the host was refused up front.
fn session(
    conn: TcpStream,
    caps: &DisplayCaps,
    ctx: &Ctx,
    shared: &ServiceShared,
    state: &Shared<DisplayState>,
    video_tx: &Sender<VideoCtl>,
    fixed_code: Option<&str>,
) -> Result<Option<()>> {
    tune_tcp(&conn)?;
    let peer = conn.peer_addr()?.ip();
    let admitted = AtomicBool::new(false);
    let admit = |host: &str| -> Result<(), String> {
        if shared
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return Err("the display is in use by another host".into());
        }
        admitted.store(true, Ordering::Relaxed);
        info!("host \"{host}\" connecting from {peer}");
        Ok(())
    };
    let on_pairing = |ev: PairingEvent<'_>| {
        state.update(|s| {
            s.phase = match ev {
                PairingEvent::ShowCode { host, code } => DisplayPhase::Pairing {
                    host: host.to_owned(),
                    code: code.to_owned(),
                },
                PairingEvent::Done => DisplayPhase::Waiting,
            }
        })
    };
    let result = (|| {
        let channel = secure::accept(
            conn,
            &ctx.identity,
            &ctx.paired,
            &ctx.name(),
            fixed_code,
            &admit,
            &on_pairing,
        )?;
        let host_name = channel.peer_name.clone();
        let writer = channel.writer()?;
        let mut reader = channel.reader()?;
        *shared.current.lock().unwrap() = Some(writer.clone());
        writer.send(&ControlMsg::Caps(caps.clone()))?;
        info!("host \"{host_name}\" connected from {peer}");

        loop {
            match reader.recv()? {
                ControlMsg::StartStream(cfg) => {
                    info!(
                        "stream {}x{} {} @ {:.2} Hz, {:.1} Mbit/s",
                        cfg.width,
                        cfg.height,
                        cfg.codec.name(),
                        cfg.refresh_mhz as f32 / 1000.0,
                        cfg.bitrate_kbps as f32 / 1000.0
                    );
                    state.update(|s| {
                        s.phase = DisplayPhase::Streaming {
                            host: host_name.clone(),
                        };
                        s.stream = Some(format!(
                            "{}x{} {} @ {:.0} Hz, {:.1} Mbit/s",
                            cfg.width,
                            cfg.height,
                            cfg.codec.name(),
                            cfg.refresh_mhz as f32 / 1000.0,
                            cfg.bitrate_kbps as f32 / 1000.0
                        ));
                    });
                    let _ = video_tx.send(VideoCtl::Start {
                        cfg,
                        host: host_name.clone(),
                        peer,
                        writer: writer.clone(),
                        key: channel.video_key.clone(),
                    });
                }
                ControlMsg::StopStream => {
                    let _ = video_tx.send(VideoCtl::Stop);
                }
                ControlMsg::Ping { t_us } => writer.send(&ControlMsg::Pong { t_us })?,
                ControlMsg::Goodbye => return Ok(()),
                other => debug!("ignoring {other:?}"),
            }
        }
    })();
    if !admitted.load(Ordering::Relaxed) {
        return result.map(|_| None).or_else(|e| {
            debug!("refused connection: {e:#}");
            Ok(None)
        });
    }
    *shared.current.lock().unwrap() = None;
    shared.busy.store(false, Ordering::Release);
    let _ = video_tx.send(VideoCtl::Stop);
    state.update(|s| {
        s.phase = DisplayPhase::Waiting;
        s.stream = None;
        s.decoder = None;
        s.fps = 0.0;
        s.latency_ms = 0.0;
    });
    result.map(Some)
}

struct Active {
    peer: IpAddr,
    writer: SecureWriter,
    reassembler: Reassembler,
    last_key_request: Option<Instant>,
}

/// How far behind the host the display is. A frame's arrival time minus
/// its capture time is the one-way delay plus the (unknown, near constant)
/// offset between the two clocks; the smallest such gap seen is the
/// baseline, and anything above it is time the frame spent queued — on the
/// network or in this display's receive buffer.
struct Lag {
    origin: Instant,
    /// Smallest arrival−capture gap (µs) and when it was last updated.
    baseline: Option<(i64, Instant)>,
}

impl Lag {
    fn new() -> Self {
        Lag {
            origin: Instant::now(),
            baseline: None,
        }
    }

    /// A new stream (its capture times start again).
    fn reset(&mut self) {
        self.baseline = None;
    }

    /// Milliseconds this frame arrived behind the best case.
    fn behind_ms(&mut self, arrived: Instant, pts_us: u64) -> f32 {
        let gap = arrived.saturating_duration_since(self.origin).as_micros() as i64 - pts_us as i64;
        let base = match self.baseline {
            // Creep up by 100 ppm to follow the clocks drifting apart; a
            // real backlog grows thousands of times faster.
            Some((b, at)) => (b + at.elapsed().as_micros() as i64 / 10_000).min(gap),
            None => gap,
        };
        self.baseline = Some((base, Instant::now()));
        (gap - base) as f32 / 1000.0
    }
}

/// Throws away every datagram already waiting in the socket.
fn drain(udp: &UdpSocket, buf: &mut [u8]) -> usize {
    let mut n = 0;
    if udp.set_nonblocking(true).is_ok() {
        while udp.recv_from(buf).is_ok() {
            n += 1;
        }
        let _ = udp.set_nonblocking(false);
    }
    n
}

fn video_thread(
    udp: UdpSocket,
    ctl: Receiver<VideoCtl>,
    sink: Arc<dyn FrameSink>,
    state: Arc<Shared<DisplayState>>,
    shared: &ServiceShared,
) {
    let mut buf = vec![0u8; 65_536];
    let mut active: Option<Active> = None;
    let mut last_stats = Instant::now();
    let mut present_total = Duration::ZERO;
    let mut lag = Lag::new();
    let mut worst_behind = 0f32;
    // Catching up: frames are dropped until the next keyframe.
    let mut skipping = false;
    let mut last_catch_up: Option<Instant> = None;
    let mut stream_id = None;

    loop {
        if !apply_ctl(&ctl, &mut active, &*sink, &state) {
            return;
        }
        let id = active.as_ref().map(|a| a.reassembler.stream_id());
        if id != stream_id {
            stream_id = id;
            lag.reset();
            skipping = false;
        }

        let (n, from) = match udp.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => {
                debug!("udp recv: {e}");
                continue;
            }
        };
        // The first packets of a new stream can overtake its StartStream
        // (TCP vs UDP); pick up pending control messages before judging them.
        if active
            .as_ref()
            .is_none_or(|a| a.reassembler.stream_id() != packet_stream_id(&buf[..n]))
            && !apply_ctl(&ctl, &mut active, &*sink, &state)
        {
            return;
        }
        let Some(a) = active.as_mut() else { continue };
        if from.ip() != a.peer {
            continue;
        }

        let res = a.reassembler.push(&buf[..n], Instant::now());
        if res.need_keyframe {
            a.request_keyframe();
        }
        if let Some(frame) = res.frame {
            let behind = lag.behind_ms(frame.first_packet_at, frame.pts_us);
            worst_behind = worst_behind.max(behind);
            let limit = shared.catch_up_ms.load(Ordering::Relaxed) as f32;
            if skipping && frame.keyframe {
                skipping = false;
            }
            if !skipping
                && limit > 0.0
                && behind > limit
                && last_catch_up.is_none_or(|t| t.elapsed() > Duration::from_secs(1))
            {
                // Showing these late frames only keeps the picture behind:
                // drop everything queued and start again from a keyframe.
                let dropped = drain(&udp, &mut buf);
                info!(
                    "{behind:.0} ms behind the host: skipped {dropped} queued packets, waiting for a keyframe"
                );
                skipping = true;
                last_catch_up = Some(Instant::now());
                a.request_keyframe();
                state.update(|s| s.catch_ups += 1);
            }
            if !skipping {
                match sink.frame(&frame) {
                    Ok(()) => present_total += frame.first_packet_at.elapsed(),
                    Err(e) => {
                        warn!("decode error: {e:#}");
                        a.request_keyframe();
                    }
                }
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(1) {
            let s = a.reassembler.stats;
            let _ = a.writer.send(&ControlMsg::Stats(ReceiverStats {
                frames_received: s.frames_delivered,
                frames_dropped: s.frames_dropped,
                packets_lost: s.packets_lost,
                decode_present_us: if s.frames_delivered > 0 {
                    (present_total.as_micros() / s.frames_delivered as u128) as u32
                } else {
                    0
                },
            }));
            debug!("receive stats {s:?}, {worst_behind:.0} ms behind");
            let secs = last_stats.elapsed().as_secs_f32();
            state.update(|st| {
                st.fps = s.frames_delivered as f32 / secs;
                st.latency_ms = if s.frames_delivered > 0 {
                    present_total.as_secs_f32() * 1000.0 / s.frames_delivered as f32
                } else {
                    0.0
                };
                st.frames_dropped += s.frames_dropped;
                st.packets_lost += s.packets_lost;
                st.behind_ms = worst_behind;
            });
            worst_behind = 0.0;
            a.reassembler.stats = Default::default();
            present_total = Duration::ZERO;
            last_stats = Instant::now();
        }
    }
}

fn packet_stream_id(datagram: &[u8]) -> u32 {
    silkscreen_proto::PacketHeader::decode(datagram).map_or(0, |(h, _)| h.stream_id)
}

/// Applies pending control messages. Returns `false` once the service is gone.
fn apply_ctl(
    ctl: &Receiver<VideoCtl>,
    active: &mut Option<Active>,
    sink: &dyn FrameSink,
    state: &Shared<DisplayState>,
) -> bool {
    loop {
        match ctl.try_recv() {
            Ok(VideoCtl::Start {
                cfg,
                host,
                peer,
                writer,
                key,
            }) => match sink.start(&cfg, &host) {
                Ok(decoder) => {
                    info!("decoding with {decoder}");
                    state.update(|s| s.decoder = Some(decoder));
                    *active = Some(Active {
                        reassembler: Reassembler::new(cfg.stream_id, Some(&key)),
                        peer,
                        writer,
                        last_key_request: None,
                    });
                }
                Err(e) => {
                    warn!("cannot decode {}: {e:#}", cfg.codec.name());
                    let _ = writer.send(&ControlMsg::Reject {
                        reason: format!("display cannot decode {}", cfg.codec.name()),
                    });
                    writer.shutdown();
                    *active = None;
                }
            },
            Ok(VideoCtl::Stop) => {
                *active = None;
                sink.stop();
            }
            Err(TryRecvError::Disconnected) => return false,
            Err(TryRecvError::Empty) => return true,
        }
    }
}

impl Active {
    fn request_keyframe(&mut self) {
        // One request per ~RTT is plenty; the host also rate-limits.
        if self
            .last_key_request
            .is_none_or(|t| t.elapsed() > Duration::from_millis(100))
        {
            self.last_key_request = Some(Instant::now());
            let _ = self.writer.send(&ControlMsg::RequestKeyframe);
        }
    }
}
