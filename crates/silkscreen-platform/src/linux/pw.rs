//! PipeWire video capture.
//!
//! Frames are handed to the encoding thread *zero-copy*: the PipeWire thread
//! dequeues the newest buffer, passes a view of its mapped memory over a
//! rendezvous channel, and only requeues the buffer once the encoder says it
//! is done. Older queued buffers are skipped, so latency never builds up.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::Pod;
use tracing::{debug, info, warn};

use crate::{Capturer, Frame, PixelLayout};

/// Where to find the stream.
pub enum Remote {
    /// A PipeWire connection handed out by the ScreenCast portal.
    Fd(OwnedFd),
    /// The user's default PipeWire daemon.
    Default,
}

pub struct CaptureParams {
    pub remote: Remote,
    pub node_id: u32,
    /// Preferred size; compositors that size virtual monitors from format
    /// negotiation (GNOME) use it, others ignore it.
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

/// A borrowed view of a mapped PipeWire buffer.
struct RawFrame {
    ptr: *const u8,
    len: usize,
    width: u32,
    height: u32,
    stride: usize,
    layout: PixelLayout,
}

// Valid until the PipeWire thread receives the matching "done".
unsafe impl Send for RawFrame {}

pub struct PwCapturer {
    frames: Receiver<RawFrame>,
    done: Sender<()>,
    ended: Arc<AtomicBool>,
    quit: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Keeps the portal/KWin session (and with it the virtual monitor) alive.
    _session: Box<dyn Send>,
}

impl PwCapturer {
    pub fn start(params: CaptureParams, session: Box<dyn Send>) -> Result<Self> {
        let (frame_tx, frame_rx) = bounded::<RawFrame>(0);
        let (done_tx, done_rx) = bounded::<()>(1);
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = bounded::<Result<()>>(1);
        let ended = Arc::new(AtomicBool::new(false));

        let thread = {
            let ended = ended.clone();
            std::thread::Builder::new()
                .name("pipewire".into())
                .spawn(move || {
                    let r = run(params, frame_tx, done_rx, quit_rx, ended.clone(), &ready_tx);
                    if let Err(e) = r {
                        let _ = ready_tx.try_send(Err(anyhow!("{e:#}")));
                        warn!("pipewire: {e:#}");
                    }
                    ended.store(true, Ordering::Relaxed);
                })?
        };
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .context("PipeWire stream did not start")??;
        Ok(PwCapturer {
            frames: frame_rx,
            done: done_tx,
            ended,
            quit: quit_tx,
            thread: Some(thread),
            _session: session,
        })
    }
}

impl Capturer for PwCapturer {
    fn next_frame(&mut self, timeout: Duration, f: &mut dyn FnMut(Frame<'_>)) -> Result<bool> {
        match self.frames.recv_timeout(timeout) {
            Ok(raw) => {
                let data = unsafe { std::slice::from_raw_parts(raw.ptr, raw.len) };
                f(Frame {
                    width: raw.width,
                    height: raw.height,
                    stride: raw.stride,
                    layout: raw.layout,
                    data,
                });
                let _ = self.done.send(());
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) if !self.ended.load(Ordering::Relaxed) => Ok(false),
            Err(_) => {
                bail!("screen capture ended (sharing stopped or the virtual monitor was removed)")
            }
        }
    }
}

impl Drop for PwCapturer {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct State {
    format: VideoInfoRaw,
    layout: Option<PixelLayout>,
    frames: Sender<RawFrame>,
    done: Receiver<()>,
}

fn run(
    params: CaptureParams,
    frames: Sender<RawFrame>,
    done: Receiver<()>,
    quit: pw::channel::Receiver<()>,
    ended: Arc<AtomicBool>,
    ready: &Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = match params.remote {
        Remote::Fd(fd) => context.connect_fd_rc(fd, None)?,
        Remote::Default => context.connect_rc(None)?,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "silkscreen-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state = State {
        format: Default::default(),
        layout: None,
        frames,
        done,
    };
    let ready_state = ready.clone();
    let ended_state = ended.clone();
    let mainloop_state = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |_, _, old, new| {
            debug!("pipewire stream {old:?} -> {new:?}");
            match new {
                pw::stream::StreamState::Streaming => {
                    let _ = ready_state.try_send(Ok(()));
                }
                pw::stream::StreamState::Error(e) => {
                    let _ = ready_state.try_send(Err(anyhow!("stream error: {e}")));
                    ended_state.store(true, Ordering::Relaxed);
                    mainloop_state.quit();
                }
                pw::stream::StreamState::Unconnected => {
                    ended_state.store(true, Ordering::Relaxed);
                    mainloop_state.quit();
                }
                _ => {}
            }
        })
        .param_changed(|_, s, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            if s.format.parse(param).is_err() {
                return;
            }
            s.layout = match s.format.format() {
                VideoFormat::BGRx | VideoFormat::BGRA => Some(PixelLayout::Bgrx),
                VideoFormat::RGBx | VideoFormat::RGBA => Some(PixelLayout::Rgbx),
                other => {
                    warn!("unsupported capture format {other:?}");
                    None
                }
            };
            let size = s.format.size();
            info!(
                "capturing {}x{} {:?}",
                size.width,
                size.height,
                s.format.format()
            );
        })
        .process(|stream, s| {
            // Keep only the newest buffer; dropping older ones requeues them.
            let mut newest = None;
            while let Some(b) = stream.dequeue_buffer() {
                newest = Some(b);
            }
            let (Some(mut buffer), Some(layout)) = (newest, s.layout) else {
                return;
            };
            let size = s.format.size();
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let chunk = d.chunk();
            let (offset, chunk_size, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride(),
            );
            if chunk_size == 0 {
                return; // cursor-only or empty update
            }
            let stride = if stride > 0 {
                stride as usize
            } else {
                size.width as usize * 4
            };
            let Some(mem) = d.data() else { return };
            let needed = stride * size.height as usize;
            if mem.len() < offset + needed {
                return;
            }
            let raw = RawFrame {
                ptr: mem[offset..].as_ptr(),
                len: needed,
                width: size.width,
                height: size.height,
                stride,
                layout,
            };
            // Wait for the encoder to take this frame and finish with it. If
            // nobody is consuming (session tearing down) give up quickly.
            if s.frames
                .send_timeout(raw, Duration::from_millis(500))
                .is_ok()
            {
                let _ = s.done.recv_timeout(Duration::from_secs(2));
            }
        })
        .register()?;

    let format = format_pod(params.width, params.height, params.refresh_mhz)?;
    let mut pods = [Pod::from_bytes(&format).context("format pod")?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(params.node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut pods,
    )?;

    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| ml.quit());
    mainloop.run();
    Ok(())
}

/// EnumFormat: 32-bit RGB in system memory (no DMA-BUF modifiers, so the
/// compositor hands us mappable SHM buffers), preferring the display's size.
fn format_pod(width: u32, height: u32, refresh_mhz: u32) -> Result<Vec<u8>> {
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pw::spa::pod::{Value, object, property, serialize::PodSerializer};
    use pw::spa::utils::{Fraction, Rectangle, SpaTypes};

    let fps = (refresh_mhz / 1000).max(1);
    let obj = object!(
        SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width, height },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    Ok(
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
            .map_err(|e| anyhow!("serialising format: {e:?}"))?
            .0
            .into_inner(),
    )
}
