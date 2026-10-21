//! Video encode/decode for Silkscreen, built on FFmpeg so every platform's
//! hardware codecs (NVENC, AMF, QSV, VAAPI, VideoToolbox, Media Foundation)
//! are reachable through one API.

pub mod convert;
pub mod decoder;
pub mod encoder;
mod ff;

pub use convert::{RgbFrame, RgbLayout};
pub use decoder::{Decoder, Matrix, Picture, PictureFormat, decodable_codecs};
pub use encoder::{
    Encoder, EncoderConfig, compiled_encoders, default_bitrate_kbps, ffmpeg_version,
};

#[cfg(test)]
mod tests {
    use super::*;
    use silkscreen_proto::Codec;

    /// Encode a moving gradient with whatever encoder is available and check
    /// the decoder reproduces approximately the same image.
    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (320u32, 240u32);
        let forced = std::env::var("SILKSCREEN_TEST_ENCODER").ok();
        let codec = match forced.as_deref() {
            Some(n) if n.starts_with("hevc") || n.contains("265") => Codec::Hevc,
            Some(n) if n.starts_with("av1") || n.contains("av1") => Codec::Av1,
            _ => Codec::H264,
        };
        let hw_decode = std::env::var_os("SILKSCREEN_TEST_HW_DECODE").is_some();
        let cfg = EncoderConfig {
            codec,
            width: w,
            height: h,
            refresh_mhz: 60_000,
            bitrate_kbps: 4_000,
            encoder: forced,
        };
        let mut enc = match Encoder::open(&cfg) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: {e}");
                return;
            }
        };
        let mut dec = Decoder::open(codec, hw_decode).unwrap();
        eprintln!("encoder {} -> decoder {}", enc.name(), dec.name());

        // Asynchronous encoders (VideoToolbox) may deliver a frame's packet
        // a little later; synchronous ones must emit exactly one per frame.
        let asynchronous = enc.name().contains("videotoolbox");
        let mut decoded = 0;
        for i in 0..10u32 {
            let mut img = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let p = ((y * w + x) * 4) as usize;
                    img[p] = (x + i) as u8; // B
                    img[p + 1] = y as u8; // G
                    img[p + 2] = 200; // R
                }
            }
            let frame = RgbFrame {
                width: w,
                height: h,
                stride: (w * 4) as usize,
                layout: RgbLayout::Bgrx,
                data: &img,
            };
            let mut packets = Vec::new();
            let t = std::time::Instant::now();
            enc.encode(&frame, i as u64 * 16_667, i == 0, |d, key| {
                packets.push((d.to_vec(), key))
            })
            .unwrap();
            eprintln!(
                "frame {i}: {} packet(s) after {:?}",
                packets.len(),
                t.elapsed()
            );
            if asynchronous {
                enc.poll(|d, key| packets.push((d.to_vec(), key))).unwrap();
                if packets.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    enc.poll(|d, key| packets.push((d.to_vec(), key))).unwrap();
                }
            } else {
                assert_eq!(
                    packets.len(),
                    1,
                    "low-latency encoder must emit one packet per frame"
                );
            }
            if i == 0 {
                assert!(packets[0].1, "first frame must be a keyframe");
            }
            let Some((data, _)) = packets.last() else {
                continue;
            };
            if let Some(pic) = dec.decode(data).unwrap() {
                assert_eq!((pic.width, pic.height), (w, h));
                // Compare decoded luma against the source's BT.709 luma.
                let (y_plane, y_stride) = pic.planes[0];
                let mut err = 0u64;
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let p = (y * w as usize + x) * 4;
                        let (b, g, r) = (img[p] as f32, img[p + 1] as f32, img[p + 2] as f32);
                        let want = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0;
                        err += (y_plane[y * y_stride + x] as f32 - want).abs() as u64;
                    }
                }
                let mean = err as f64 / (w * h) as f64;
                assert!(mean < 4.0, "frame {i}: mean luma error {mean:.2}");
                decoded += 1;
            }
        }
        if asynchronous {
            assert!(decoded >= 8, "decoded only {decoded} of 10 frames");
        } else {
            assert_eq!(decoded, 10, "decoder must output every frame without delay");
        }
    }
}
