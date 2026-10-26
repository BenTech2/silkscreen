//! RGB → YUV 4:2:0 conversion (BT.709, limited range), parallelised over row
//! bands. Hardware encoders all accept NV12, so this is the only colour
//! conversion on the host path.

use rayon::prelude::*;

/// Byte order of a captured 32-bit pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgbLayout {
    /// B, G, R, A/X in memory (Windows DXGI, macOS, PipeWire `BGRx`).
    Bgrx,
    /// R, G, B, A/X in memory (PipeWire `RGBx`).
    Rgbx,
}

/// A captured frame in CPU memory.
#[derive(Debug, Clone, Copy)]
pub struct RgbFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, at least `width * 4`.
    pub stride: usize,
    pub layout: RgbLayout,
    pub data: &'a [u8],
}

/// Destination chroma layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromaLayout {
    /// One interleaved UV plane.
    Nv12,
    /// Separate U and V planes.
    I420,
    /// Separate U and V planes at full resolution (4:4:4): sharper colour
    /// edges, e.g. coloured text.
    I444,
}

pub struct YuvPlanes<'a> {
    pub y: &'a mut [u8],
    pub y_stride: usize,
    /// NV12: interleaved UV. I420 / I444: U.
    pub u: &'a mut [u8],
    pub u_stride: usize,
    /// I420 and I444 only.
    pub v: Option<(&'a mut [u8], usize)>,
}

#[inline(always)]
fn px(row: &[u8], x: usize, layout: RgbLayout) -> (i32, i32, i32) {
    let p = &row[x * 4..x * 4 + 4];
    match layout {
        RgbLayout::Bgrx => (p[2] as i32, p[1] as i32, p[0] as i32),
        RgbLayout::Rgbx => (p[0] as i32, p[1] as i32, p[2] as i32),
    }
}

// BT.709 limited-range coefficients in 8.8 fixed point.
#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8
}

#[inline(always)]
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    // Inputs are sums of 4 pixels; the extra >> 2 averages them.
    let u = ((-26 * r - 86 * g + 112 * b + 512) >> 10) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 512) >> 10) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Converts `src` into `dst` in the given layout.
pub fn rgb_to_yuv(src: &RgbFrame, dst: YuvPlanes, chroma_layout: ChromaLayout) {
    match chroma_layout {
        ChromaLayout::I444 => rgb_to_yuv444(src, dst),
        _ => rgb_to_yuv420(src, dst, chroma_layout),
    }
}

/// Full-resolution chroma: every pixel keeps its own colour.
fn rgb_to_yuv444(src: &RgbFrame, dst: YuvPlanes) {
    let w = src.width as usize;
    let h = src.height as usize;
    let (v_plane, v_stride) = dst.v.expect("I444 destination needs a V plane");
    let layout = src.layout;
    dst.y
        .par_chunks_mut(dst.y_stride)
        .zip(dst.u.par_chunks_mut(dst.u_stride))
        .zip(v_plane.par_chunks_mut(v_stride))
        .take(h)
        .enumerate()
        .for_each(|(row, ((yr, ur), vr))| {
            let s = &src.data[row * src.stride..];
            for x in 0..w {
                let (r, g, b) = px(s, x, layout);
                yr[x] = luma(r, g, b);
                // `chroma` takes sums of four pixels.
                let (u, v) = chroma(r * 4, g * 4, b * 4);
                ur[x] = u;
                vr[x] = v;
            }
        });
}

/// Converts `src` into `dst`. Width and height should be even (encoders
/// require it); a trailing odd row/column is ignored for chroma.
pub fn rgb_to_yuv420(src: &RgbFrame, dst: YuvPlanes, chroma_layout: ChromaLayout) {
    let w = src.width as usize;
    let h = src.height as usize;
    let cw = w / 2;
    let ch = h / 2;
    let layout = src.layout;

    let y_rows = dst.y.par_chunks_mut(dst.y_stride * 2).take(ch);
    let u_rows = dst.u.par_chunks_mut(dst.u_stride).take(ch);
    match (chroma_layout, dst.v) {
        (ChromaLayout::Nv12, _) => {
            y_rows
                .zip(u_rows)
                .enumerate()
                .for_each(|(cy, (ypair, uv))| {
                    let (y0, y1) = ypair.split_at_mut(dst.y_stride);
                    let s0 = &src.data[cy * 2 * src.stride..];
                    let s1 = &src.data[(cy * 2 + 1) * src.stride..];
                    for cx in 0..cw {
                        let (sum_r, sum_g, sum_b) = convert_2x2(s0, s1, cx, layout, y0, y1);
                        let (u, v) = chroma(sum_r, sum_g, sum_b);
                        uv[cx * 2] = u;
                        uv[cx * 2 + 1] = v;
                    }
                });
        }
        (ChromaLayout::I420, Some((v_plane, v_stride))) => {
            let v_rows = v_plane.par_chunks_mut(v_stride).take(ch);
            y_rows
                .zip(u_rows)
                .zip(v_rows)
                .enumerate()
                .for_each(|(cy, ((ypair, ur), vr))| {
                    let (y0, y1) = ypair.split_at_mut(dst.y_stride);
                    let s0 = &src.data[cy * 2 * src.stride..];
                    let s1 = &src.data[(cy * 2 + 1) * src.stride..];
                    for cx in 0..cw {
                        let (sum_r, sum_g, sum_b) = convert_2x2(s0, s1, cx, layout, y0, y1);
                        let (u, v) = chroma(sum_r, sum_g, sum_b);
                        ur[cx] = u;
                        vr[cx] = v;
                    }
                });
        }
        (ChromaLayout::I420, None) => panic!("I420 destination needs a V plane"),
        (ChromaLayout::I444, _) => unreachable!("4:4:4 goes through rgb_to_yuv444"),
    }
}

#[inline(always)]
fn convert_2x2(
    s0: &[u8],
    s1: &[u8],
    cx: usize,
    layout: RgbLayout,
    y0: &mut [u8],
    y1: &mut [u8],
) -> (i32, i32, i32) {
    let x = cx * 2;
    let (r00, g00, b00) = px(s0, x, layout);
    let (r01, g01, b01) = px(s0, x + 1, layout);
    let (r10, g10, b10) = px(s1, x, layout);
    let (r11, g11, b11) = px(s1, x + 1, layout);
    y0[x] = luma(r00, g00, b00);
    y0[x + 1] = luma(r01, g01, b01);
    y1[x] = luma(r10, g10, b10);
    y1[x + 1] = luma(r11, g11, b11);
    (
        r00 + r01 + r10 + r11,
        g00 + g01 + g10 + g11,
        b00 + b01 + b10 + b11,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert_solid(rgb: (u8, u8, u8)) -> (u8, u8, u8) {
        let (w, h) = (4usize, 4usize);
        let mut src = vec![0u8; w * h * 4];
        for p in src.chunks_mut(4) {
            p.copy_from_slice(&[rgb.2, rgb.1, rgb.0, 255]);
        }
        let mut y = vec![0u8; w * h];
        let mut uv = vec![0u8; w * h / 2];
        rgb_to_yuv420(
            &RgbFrame {
                width: 4,
                height: 4,
                stride: w * 4,
                layout: RgbLayout::Bgrx,
                data: &src,
            },
            YuvPlanes {
                y: &mut y,
                y_stride: w,
                u: &mut uv,
                u_stride: w,
                v: None,
            },
            ChromaLayout::Nv12,
        );
        assert!(y.iter().all(|&v| v == y[0]));
        (y[0], uv[0], uv[1])
    }

    #[test]
    fn reference_colours() {
        // Expected BT.709 limited range values, ±1 for fixed-point rounding.
        let cases = [
            ((0, 0, 0), (16, 128, 128)),
            ((255, 255, 255), (235, 128, 128)),
            ((255, 0, 0), (63, 102, 240)),
            ((0, 255, 0), (173, 42, 26)),
            ((0, 0, 255), (32, 240, 118)),
        ];
        for (rgb, want) in cases {
            let got = convert_solid(rgb);
            for (g, w) in [(got.0, want.0), (got.1, want.1), (got.2, want.2)] {
                assert!(
                    (g as i32 - w).abs() <= 1,
                    "{rgb:?}: got {got:?}, want {want:?}"
                );
            }
        }
    }
}
