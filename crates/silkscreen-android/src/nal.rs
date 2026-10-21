//! Annex B bitstream helpers.

use silkscreen_proto::Codec;

/// Parameter-set NAL units (VPS/SPS/PPS) at the start of an Annex B access
/// unit, which MediaCodec wants as a separate codec-config buffer.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn parameter_sets(codec: Codec, au: &[u8]) -> Option<&[u8]> {
    let is_param = |nal: u8| match codec {
        Codec::H264 => matches!(nal & 0x1f, 7 | 8),
        Codec::Hevc => matches!((nal >> 1) & 0x3f, 32..=34),
        Codec::Av1 | Codec::PyroWave => false,
    };
    let mut end = 0;
    let mut i = 0;
    while i + 3 < au.len() {
        // Start code: 00 00 01 (possibly preceded by another 00).
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let header = au[i + 3];
            if !is_param(header) {
                break;
            }
            end = au[i + 3..]
                .windows(3)
                .position(|w| w == [0, 0, 1])
                .map_or(au.len(), |p| i + 3 + p);
            // Include a 4-byte start code's leading zero in the next unit.
            if end > 0 && end < au.len() && au[end - 1] == 0 {
                end -= 1;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    (end > 0).then(|| &au[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_hevc_parameter_sets() {
        // VPS (32), SPS (33), PPS (34), then an IDR slice (19).
        let au = [
            0, 0, 0, 1, 0x40, 1, 0xAA, //
            0, 0, 0, 1, 0x42, 1, 0xBB, //
            0, 0, 0, 1, 0x44, 1, 0xCC, //
            0, 0, 0, 1, 0x26, 1, 0xDD,
        ];
        assert_eq!(parameter_sets(Codec::Hevc, &au), Some(&au[..21]));
        let slice_only = [0, 0, 0, 1, 0x26, 1, 0xDD];
        assert_eq!(parameter_sets(Codec::Hevc, &slice_only), None);
    }

    #[test]
    fn splits_h264_parameter_sets() {
        let au = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0, 0, 1, 0x65, 0xCC,
        ];
        assert_eq!(parameter_sets(Codec::H264, &au), Some(&au[..12]));
    }
}
