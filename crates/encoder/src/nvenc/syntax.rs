//! The parameter-set fields that decide how far a decoder can be predicted past a gap
//! (`refs::Syntax`), read from the sequence parameter set NVENC writes into each keyframe.

use remothing_core::encode::Codec;

use super::refs::Syntax;

/// The limits the first sequence parameter set in `annexb` (a keyframe) sets; AV1 carries
/// every order hint in full, so nothing limits it. `known: false` when it cannot be read.
pub(super) fn read(codec: Codec, annexb: &[u8]) -> Syntax {
    let parsed = match codec {
        Codec::H264 => nal(annexb, |h| h[0] & 0x1f == 7, 1).and_then(|rbsp| h264(&rbsp)),
        Codec::Hevc => nal(annexb, |h| (h[0] >> 1) & 0x3f == 33, 2).and_then(|rbsp| hevc(&rbsp)),
        Codec::Av1 => Some(Syntax::default()),
    };
    parsed.map_or(Syntax::default(), |s| Syntax { known: true, ..s })
}

/// The payload (emulation prevention removed, `header` bytes skipped) of the first NAL unit
/// whose header `is` matches.
fn nal(annexb: &[u8], is: impl Fn(&[u8]) -> bool, header: usize) -> Option<Vec<u8>> {
    let starts: Vec<usize> = annexb.windows(3).enumerate().filter(|(_, w)| *w == [0, 0, 1]).map(|(i, _)| i + 3).collect();
    let (i, &start) = starts.iter().enumerate().find(|(_, &s)| annexb.len() > s + header && is(&annexb[s..]))?;
    let end = starts.get(i + 1).map_or(annexb.len(), |&next| next - 3);
    let mut rbsp = Vec::with_capacity(end - start);
    let mut zeros = 0;
    for &b in &annexb[start + header..end] {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    Some(rbsp)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some(v << 1 | self.bit()?))
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Exp-Golomb `ue(v)`.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
}

/// H.264 seq_parameter_set_rbsp up to the picture order count fields (7.3.2.1.1).
fn h264(rbsp: &[u8]) -> Option<Syntax> {
    let mut r = Bits { data: rbsp, pos: 0 };
    let profile = r.bits(8)?;
    r.skip(16); // constraint flags, level_idc
    r.ue()?; // seq_parameter_set_id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        if r.ue()? == 3 {
            r.skip(1); // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1); // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            // Scaling matrices: NVENC writes none.
            return None;
        }
    }
    let frame_num_range = 1u64 << (r.ue()? + 4);
    let poc_lsb_range = match r.ue()? {
        0 => Some(1u64 << (r.ue()? + 4)),
        2 => None,
        // Type 1 (cycles of expected order counts): NVENC does not use it.
        _ => return None,
    };
    Some(Syntax { frame_num_range: Some(frame_num_range), poc_lsb_range, known: true })
}

/// HEVC seq_parameter_set_rbsp up to log2_max_pic_order_cnt_lsb_minus4 (7.3.2.2.1).
fn hevc(rbsp: &[u8]) -> Option<Syntax> {
    let mut r = Bits { data: rbsp, pos: 0 };
    r.skip(4); // sps_video_parameter_set_id
    let sub_layers = r.bits(3)?; // sps_max_sub_layers_minus1
    r.skip(1); // sps_temporal_id_nesting_flag
    // profile_tier_level(1, sps_max_sub_layers_minus1): general part, then per sub-layer.
    r.skip(88 + 8);
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((r.bit()?, r.bit()?));
    }
    if sub_layers > 0 {
        r.skip(2 * (8 - sub_layers as usize));
    }
    for (profile, level) in present {
        r.skip(88 * profile as usize + 8 * level as usize);
    }
    r.ue()?; // sps_seq_parameter_set_id
    if r.ue()? == 3 {
        r.skip(1); // separate_colour_plane_flag
    }
    r.ue()?; // pic_width_in_luma_samples
    r.ue()?; // pic_height_in_luma_samples
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?; // conf_win_*_offset
        }
    }
    r.ue()?; // bit_depth_luma_minus8
    r.ue()?; // bit_depth_chroma_minus8
    let poc_lsb_range = 1u64 << (r.ue()? + 4);
    Some(Syntax { frame_num_range: None, poc_lsb_range: Some(poc_lsb_range), known: true })
}
