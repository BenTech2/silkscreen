//! Thin RAII wrappers over the FFmpeg C API.

use std::ffi::{CStr, CString, c_int};
use std::ptr;

use anyhow::{Result, bail};
use ffmpeg_sys_next as ff;

pub fn err_str(code: c_int) -> String {
    let mut buf = [0 as std::ffi::c_char; 256];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

pub fn check(ret: c_int, what: &str) -> Result<c_int> {
    if ret < 0 {
        bail!("{what}: {}", err_str(ret));
    }
    Ok(ret)
}

pub const EAGAIN: c_int = ff::AVERROR(ff::EAGAIN);

macro_rules! owned {
    ($name:ident, $ty:ty, $free:expr) => {
        pub struct $name(pub *mut $ty);
        impl Drop for $name {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    #[allow(unused_unsafe)]
                    unsafe {
                        $free(&mut self.0)
                    }
                }
            }
        }
        // FFmpeg objects are safe to move between threads when not shared.
        unsafe impl Send for $name {}
    };
}

owned!(Frame, ff::AVFrame, |p: &mut *mut ff::AVFrame| {
    ff::av_frame_free(p)
});
owned!(Packet, ff::AVPacket, |p: &mut *mut ff::AVPacket| {
    ff::av_packet_free(p)
});
owned!(
    CodecCtx,
    ff::AVCodecContext,
    |p: &mut *mut ff::AVCodecContext| { ff::avcodec_free_context(p) }
);
owned!(BufRef, ff::AVBufferRef, |p: &mut *mut ff::AVBufferRef| {
    ff::av_buffer_unref(p)
});

impl Frame {
    pub fn new() -> Result<Self> {
        let f = unsafe { ff::av_frame_alloc() };
        if f.is_null() {
            bail!("av_frame_alloc failed");
        }
        Ok(Frame(f))
    }
}

impl Packet {
    pub fn new() -> Result<Self> {
        let p = unsafe { ff::av_packet_alloc() };
        if p.is_null() {
            bail!("av_packet_alloc failed");
        }
        Ok(Packet(p))
    }
}

pub struct Dict(pub *mut ff::AVDictionary);

impl Dict {
    pub fn new() -> Self {
        Dict(ptr::null_mut())
    }

    pub fn set(&mut self, key: &str, value: &str) {
        let k = CString::new(key).unwrap();
        let v = CString::new(value).unwrap();
        unsafe { ff::av_dict_set(&mut self.0, k.as_ptr(), v.as_ptr(), 0) };
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        unsafe { ff::av_dict_free(&mut self.0) };
    }
}

pub fn create_hw_device(kind: ff::AVHWDeviceType) -> Result<BufRef> {
    let mut dev = ptr::null_mut();
    check(
        unsafe { ff::av_hwdevice_ctx_create(&mut dev, kind, ptr::null(), ptr::null_mut(), 0) },
        "av_hwdevice_ctx_create",
    )?;
    Ok(BufRef(dev))
}

pub fn hw_device_name(kind: ff::AVHWDeviceType) -> String {
    unsafe {
        let p = ff::av_hwdevice_get_type_name(kind);
        if p.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// Make FFmpeg only print real errors, once per process.
pub fn init_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { ff::av_log_set_level(ff::AV_LOG_ERROR) });
}
