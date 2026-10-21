//! ScreenCaptureKit capture of one display, handed to the encoder zero-copy
//! (same rendezvous scheme as the PipeWire backend: the sample buffer stays
//! locked until the encoder is done with it).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};
use tracing::info;

use super::virtual_display::VirtualDisplay;
use crate::{Capturer, Frame, PixelLayout, ScreenRequest};

struct RawFrame {
    ptr: *const u8,
    len: usize,
    width: u32,
    height: u32,
    stride: usize,
}
unsafe impl Send for RawFrame {}

struct OutputIvars {
    frames: Sender<RawFrame>,
    done: Receiver<()>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SilkscreenStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle/unchanged frames carry no image buffer.
            let Some(pb) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            unsafe {
                if CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
                    return;
                }
                let (w, h) = (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb));
                let stride = CVPixelBufferGetBytesPerRow(&pb);
                let raw = RawFrame {
                    ptr: CVPixelBufferGetBaseAddress(&pb) as *const u8,
                    len: stride * h,
                    width: w as u32,
                    height: h as u32,
                    stride,
                };
                let iv = self.ivars();
                if iv
                    .frames
                    .send_timeout(raw, Duration::from_millis(500))
                    .is_ok()
                {
                    let _ = iv.done.recv_timeout(Duration::from_secs(2));
                }
                CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
);

impl StreamOutput {
    fn new(ivars: OutputIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

pub struct SckCapturer {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    frames: Receiver<RawFrame>,
    done: Sender<()>,
    _display: Option<VirtualDisplay>,
}

// SCStream is thread-safe; we only start/stop it.
unsafe impl Send for SckCapturer {}

impl SckCapturer {
    pub fn start(
        display_id: u32,
        req: &ScreenRequest,
        virtual_display: Option<VirtualDisplay>,
    ) -> Result<Self> {
        let content = shareable_content()?;
        let displays = unsafe { content.displays() };
        let display = displays
            .iter()
            .find(|d| unsafe { d.displayID() } == display_id)
            .context(
                "display not available to ScreenCaptureKit (grant Screen Recording permission)",
            )?;

        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(req.width as usize);
            config.setHeight(req.height as usize);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setShowsCursor(true);
            config.setQueueDepth(3);
            config.setMinimumFrameInterval(CMTime {
                value: 1000,
                timescale: req.refresh_mhz.max(1000) as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
        }

        let (frame_tx, frame_rx) = bounded(0);
        let (done_tx, done_rx) = bounded(1);
        let output = StreamOutput::new(OutputIvars {
            frames: frame_tx,
            done: done_rx,
        });
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };
        let queue = DispatchQueue::new("silkscreen.capture", None);
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("addStreamOutput: {}", e.localizedDescription()))?;
        }
        let (tx, rx) = bounded::<Option<String>>(1);
        let handler = block2::RcBlock::new(move |err: *mut NSError| {
            let msg = unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string());
            let _ = tx.send(msg);
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
        if let Some(e) = rx
            .recv_timeout(Duration::from_secs(10))
            .context("ScreenCaptureKit did not start")?
        {
            bail!("ScreenCaptureKit: {e}");
        }
        info!("capturing display {display_id} with ScreenCaptureKit");
        Ok(SckCapturer {
            stream,
            _output: output,
            frames: frame_rx,
            done: done_tx,
            _display: virtual_display,
        })
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = bounded::<Result<Retained<SCShareableContent>, String>>(1);
    let handler =
        block2::RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
            let r = match (unsafe { Retained::retain(content) }, unsafe {
                err.as_ref()
            }) {
                (Some(c), _) => Ok(c),
                (None, Some(e)) => Err(e.localizedDescription().to_string()),
                (None, None) => Err("no content".into()),
            };
            let _ = tx.send(r);
        });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    rx.recv_timeout(Duration::from_secs(10))
        .context("ScreenCaptureKit did not answer")?
        .map_err(|e| {
            anyhow!("ScreenCaptureKit: {e} (allow Screen Recording in System Settings › Privacy)")
        })
}

impl Capturer for SckCapturer {
    fn next_frame(&mut self, timeout: Duration, f: &mut dyn FnMut(Frame<'_>)) -> Result<bool> {
        match self.frames.recv_timeout(timeout) {
            Ok(raw) => {
                let data = unsafe { std::slice::from_raw_parts(raw.ptr, raw.len) };
                f(Frame {
                    width: raw.width,
                    height: raw.height,
                    stride: raw.stride,
                    layout: PixelLayout::Bgrx,
                    data,
                });
                let _ = self.done.send(());
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => bail!("screen capture stopped"),
        }
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}
