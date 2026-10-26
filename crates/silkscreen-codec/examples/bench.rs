//! Encode timing breakdown: `cargo run --release --example bench -- 4500 3000 [encoder]`
use std::time::Instant;

use silkscreen_codec::convert::{ChromaLayout, YuvPlanes, rgb_to_yuv420};
use silkscreen_codec::{Encoder, EncoderConfig, RgbFrame, RgbLayout};
use silkscreen_proto::Codec;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(3840, |a| a.parse().unwrap());
    let h: u32 = args.get(2).map_or(2160, |a| a.parse().unwrap());
    let forced = args.get(3).cloned();
    let img: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
    let frame = RgbFrame {
        width: w,
        height: h,
        stride: w as usize * 4,
        layout: RgbLayout::Bgrx,
        data: &img,
    };

    let mut y = vec![0u8; (w * h) as usize];
    let mut uv = vec![0u8; (w * h / 2) as usize];
    let t = Instant::now();
    for _ in 0..30 {
        rgb_to_yuv420(
            &frame,
            YuvPlanes {
                y: &mut y,
                y_stride: w as usize,
                u: &mut uv,
                u_stride: w as usize,
                v: None,
            },
            ChromaLayout::Nv12,
        );
    }
    println!(
        "convert {w}x{h}: {:.2} ms",
        t.elapsed().as_secs_f64() * 1000.0 / 30.0
    );

    let codec = if forced.as_deref().is_some_and(|e| e.starts_with("h264")) {
        Codec::H264
    } else {
        Codec::Hevc
    };
    let mut enc = Encoder::open(&EncoderConfig {
        codec,
        width: w,
        height: h,
        refresh_mhz: 60_000,
        bitrate_kbps: 50_000,
        encoder: forced,
        chroma444: false,
    })
    .unwrap();
    for i in 0..5 {
        enc.encode(&frame, i * 16_666, i == 0, |_, _| {}).unwrap();
    }
    let t = Instant::now();
    for i in 0..60 {
        enc.encode(&frame, (i + 5) * 16_666, false, |_, _| {})
            .unwrap();
    }
    println!(
        "{} full encode: {:.2} ms/frame",
        enc.name(),
        t.elapsed().as_secs_f64() * 1000.0 / 60.0
    );
}
