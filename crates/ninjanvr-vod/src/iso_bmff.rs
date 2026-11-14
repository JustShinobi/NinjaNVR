//! Shared ISO-BMFF (`ftyp`/`moov`/`moof`/`mdat`) box iteration, strict fMP4 segment
//! validation, keyframe sync classification, and H.264 SPS live init-segment dimension patching.

pub const MAX_BOXES_PER_LEVEL: usize = 4_096;
pub const MAX_EXACT_FMP4_BOXES_PER_LEVEL: usize = 256;

/// Zero-copy view into a parsed ISO-BMFF box payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp4BoxSlice<'a> {
    pub box_type: [u8; 4],
    pub payload: &'a [u8],
}

/// Iterates over ISO-BMFF boxes in `buf` (supporting 32-bit, 64-bit `size == 1`, and EOF `size == 0`
/// box headers), stopping cleanly on truncated or out-of-bounds trailing bytes.
pub fn iter_boxes(mut buf: &[u8]) -> Vec<Mp4BoxSlice<'_>> {
    let mut out = Vec::new();
    while buf.len() >= 8 {
        if out.len() >= MAX_BOXES_PER_LEVEL {
            break;
        }
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        let box_type = [buf[4], buf[5], buf[6], buf[7]];
        let (box_len_u64, hdr_len) = if size32 == 1 {
            if buf.len() < 16 {
                break;
            }
            let size64 = u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]);
            (size64, 16usize)
        } else if size32 == 0 {
            (buf.len() as u64, 8usize)
        } else {
            (size32, 8usize)
        };
        let Ok(box_len) = usize::try_from(box_len_u64) else {
            break;
        };
        if box_len < hdr_len || box_len > buf.len() {
            break;
        }
        out.push(Mp4BoxSlice {
            box_type,
            payload: &buf[hdr_len..box_len],
        });
        buf = &buf[box_len..];
    }
    out
}

/// Strictly parses `buf` into exact-boundary ISO-BMFF boxes, rejecting open-ended (`size == 0`),
/// truncated, trailing-byte, or over-nested (`>= max_boxes`) buffers.
pub fn parse_exact_iso_boxes_bounded(
    mut buf: &[u8],
    max_boxes: usize,
) -> Option<Vec<([u8; 4], &[u8])>> {
    if buf.len() < 8 {
        return None;
    }
    let mut out = Vec::new();
    while !buf.is_empty() {
        if buf.len() < 8 || out.len() >= max_boxes {
            return None;
        }
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        let box_type = [buf[4], buf[5], buf[6], buf[7]];
        let (box_len, hdr_len) = if size32 == 1 {
            if buf.len() < 16 {
                return None;
            }
            let size64 = u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]);
            if size64 < 16 {
                return None;
            }
            (usize::try_from(size64).ok()?, 16usize)
        } else if size32 == 0 {
            return None;
        } else {
            if size32 < 8 {
                return None;
            }
            (size32 as usize, 8usize)
        };
        if box_len > buf.len() {
            return None;
        }
        out.push((box_type, &buf[hdr_len..box_len]));
        buf = &buf[box_len..];
    }
    Some(out)
}

/// Strictly parses `buf` into exact-boundary ISO-BMFF boxes capped at [`MAX_EXACT_FMP4_BOXES_PER_LEVEL`].
pub fn parse_exact_iso_boxes(buf: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    parse_exact_iso_boxes_bounded(buf, MAX_EXACT_FMP4_BOXES_PER_LEVEL)
}

/// Returns the first box payload matching `name`.
pub fn find_box<'a>(boxes: &'a [Mp4BoxSlice<'a>], name: &[u8; 4]) -> Option<&'a [u8]> {
    boxes
        .iter()
        .find(|b| &b.box_type == name)
        .map(|b| b.payload)
}

pub(crate) fn make_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
    out
}

pub(crate) fn make_full_box(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let total = (12 + body.len()) as u32;
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
    out.extend_from_slice(&vf.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// Returns `true` if `buf` is an ISO-BMFF fMP4 initialization segment (`ftyp` and/or `moov` box, with no `moof` box).
pub fn is_fmp4_init_segment(buf: &[u8]) -> bool {
    let boxes = iter_boxes(buf);
    if boxes.is_empty() {
        return false;
    }
    let has_init_box = boxes
        .iter()
        .any(|b| &b.box_type == b"ftyp" || &b.box_type == b"moov");
    let has_moof = boxes.iter().any(|b| &b.box_type == b"moof");
    has_init_box && !has_moof
}

/// Validates that `buf` (`<= max_bytes`) is a structurally complete ISO-BMFF fMP4 initialization
/// segment containing both `ftyp` and non-empty `moov` boxes, no `moof` box, and zero trailing bytes.
pub fn is_complete_fmp4_init_segment(buf: &[u8], max_bytes: usize) -> bool {
    if buf.is_empty() || buf.len() > max_bytes {
        return false;
    }
    let Some(boxes) = parse_exact_iso_boxes(buf) else {
        return false;
    };
    let has_ftyp = boxes
        .iter()
        .any(|(t, body)| t == b"ftyp" && body.len() >= 8);
    let has_moov = boxes
        .iter()
        .any(|(t, body)| t == b"moov" && !body.is_empty());
    let has_moof = boxes.iter().any(|(t, _)| t == b"moof");
    has_ftyp && has_moov && !has_moof && is_fmp4_init_segment(buf)
}

/// Validates that `buf` (`<= max_bytes`) is a structurally complete ISO-BMFF fMP4 media fragment
/// (`moof` + `mdat`) with exact box boundaries and at least one valid `traf` (`tfhd` + `trun`).
pub fn is_complete_fmp4_media_fragment(buf: &[u8], max_bytes: usize) -> bool {
    if buf.is_empty() || buf.len() > max_bytes {
        return false;
    }
    let Some(top_boxes) = parse_exact_iso_boxes(buf) else {
        return false;
    };
    let has_ftyp_or_moov = top_boxes.iter().any(|(t, _)| t == b"ftyp" || t == b"moov");
    if has_ftyp_or_moov {
        return false;
    }
    let has_mdat = top_boxes
        .iter()
        .any(|(t, body)| t == b"mdat" && !body.is_empty());
    if !has_mdat {
        return false;
    }
    let mut saw_moof = false;
    for (top_type, moof_body) in &top_boxes {
        if top_type != b"moof" {
            continue;
        }
        saw_moof = true;
        let Some(moof_boxes) = parse_exact_iso_boxes(moof_body) else {
            return false;
        };
        let mut valid_traf_in_moof = false;
        for (btype, bbody) in &moof_boxes {
            if btype != b"traf" {
                continue;
            }
            let Some(traf_boxes) = parse_exact_iso_boxes(bbody) else {
                return false;
            };
            let has_tfhd = traf_boxes
                .iter()
                .any(|(t, body)| t == b"tfhd" && body.len() >= 8);
            let has_trun = traf_boxes
                .iter()
                .any(|(t, body)| t == b"trun" && body.len() >= 8);
            if !has_tfhd || !has_trun {
                return false;
            }
            valid_traf_in_moof = true;
        }
        if !valid_traf_in_moof {
            return false;
        }
    }
    saw_moof
}

#[inline]
fn sample_flags_is_sync(flags: u32) -> bool {
    let sample_depends_on = (flags >> 24) & 0x03;
    let sample_is_non_sync_sample = (flags >> 16) & 0x01;
    sample_depends_on == 2 || (sample_is_non_sync_sample == 0 && sample_depends_on != 1)
}

/// Parses an ISO-BMFF `trun` box payload (starting with `version + tr_flags`) and returns whether
/// its first sample is a sync sample (IDR Keyframe).
pub fn parse_trun_first_sample_is_sync(
    trun: &[u8],
    default_sample_flags: Option<u32>,
) -> Option<bool> {
    if trun.len() < 8 {
        return None;
    }
    let vf = u32::from_be_bytes([trun[0], trun[1], trun[2], trun[3]]);
    let tr_flags = vf & 0x00FF_FFFF;
    let sample_count = u32::from_be_bytes([trun[4], trun[5], trun[6], trun[7]]);
    if sample_count == 0 {
        return None;
    }
    let mut pos = 8usize;
    if (tr_flags & 0x000001) != 0 {
        pos += 4;
    }
    if pos > trun.len() {
        return None;
    }
    if (tr_flags & 0x000004) != 0 {
        if pos + 4 > trun.len() {
            return None;
        }
        let first_sample_flags =
            u32::from_be_bytes([trun[pos], trun[pos + 1], trun[pos + 2], trun[pos + 3]]);
        return Some(sample_flags_is_sync(first_sample_flags));
    }
    if (tr_flags & 0x000400) != 0 {
        if (tr_flags & 0x000100) != 0 {
            pos += 4;
        }
        if (tr_flags & 0x000200) != 0 {
            pos += 4;
        }
        if pos + 4 > trun.len() {
            return None;
        }
        let first_sample_flags =
            u32::from_be_bytes([trun[pos], trun[pos + 1], trun[pos + 2], trun[pos + 3]]);
        return Some(sample_flags_is_sync(first_sample_flags));
    }
    default_sample_flags.map(sample_flags_is_sync)
}

fn parse_tfhd_track_and_default_flags(tfhd: &[u8]) -> (Option<u32>, Option<u32>) {
    if tfhd.len() < 8 {
        return (None, None);
    }
    let vf = u32::from_be_bytes([tfhd[0], tfhd[1], tfhd[2], tfhd[3]]);
    let tf_flags = vf & 0x00FF_FFFF;
    let track_id = u32::from_be_bytes([tfhd[4], tfhd[5], tfhd[6], tfhd[7]]);
    let mut pos = 8usize;
    if (tf_flags & 0x000001) != 0 {
        pos += 8;
    }
    if (tf_flags & 0x000002) != 0 {
        pos += 4;
    }
    if (tf_flags & 0x000008) != 0 {
        pos += 4;
    }
    if (tf_flags & 0x000010) != 0 {
        pos += 4;
    }
    if pos > tfhd.len() {
        return (None, None);
    }
    let default_flags = if (tf_flags & 0x000020) != 0 {
        if pos + 4 > tfhd.len() {
            return (None, None);
        }
        Some(u32::from_be_bytes([
            tfhd[pos],
            tfhd[pos + 1],
            tfhd[pos + 2],
            tfhd[pos + 3],
        ]))
    } else {
        None
    };
    (Some(track_id), default_flags)
}

/// Returns `true` if `buf` is an ISO-BMFF fMP4 media fragment (`moof` + `mdat`) whose video track
/// (`track_id == 1`, or a `traf` with no `track_id`) begins with an IDR Sync Sample (Keyframe).
/// Standalone audio fragments (`track_id > 1`, e.g. `track_id == 2`) return `false` so AAC sync
/// packets never evict the cached video keyframe in `MseGopCache`.
pub fn is_fmp4_sync_fragment(buf: &[u8]) -> bool {
    let top_boxes = iter_boxes(buf);
    let Some(moof) = find_box(&top_boxes, b"moof") else {
        return false;
    };
    let moof_boxes = iter_boxes(moof);
    for b in &moof_boxes {
        if &b.box_type != b"traf" {
            continue;
        }
        let traf_boxes = iter_boxes(b.payload);
        let (track_id, default_flags) =
            find_box(&traf_boxes, b"tfhd").map_or((None, None), parse_tfhd_track_and_default_flags);
        if matches!(track_id, Some(id) if id != 1) {
            continue;
        }
        let Some(trun) = find_box(&traf_boxes, b"trun") else {
            continue;
        };
        if let Some(is_sync) = parse_trun_first_sample_is_sync(trun, default_flags) {
            return is_sync;
        }
    }
    false
}

/// Hardcoded dummy H.264 Baseline `128x96` SPS emitted by `go2rtc`'s `mp4.Muxer` when an upstream
/// producer (such as `tapo://` HTTP `Streamd` or raw MPEG-TS) starts without SDP `sprop-parameter-sets`.
pub const GO2RTC_PLACEHOLDER_H264_SPS: [u8; 7] = [0x67, 0x42, 0x00, 0x0a, 0xf8, 0x41, 0xa2];

struct RbspBitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> RbspBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let byte_idx = self.bit_pos / 8;
        if byte_idx >= self.data.len() {
            return None;
        }
        let shift = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Some(u32::from((self.data[byte_idx] >> shift) & 1))
    }

    fn read_bits(&mut self, count: usize) -> Option<u32> {
        if count > 32 {
            return None;
        }
        let mut val = 0u32;
        for _ in 0..count {
            val = (val << 1) | self.read_bit()?;
        }
        Some(val)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0usize;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        ((1u32 << leading_zeros) - 1).checked_add(suffix)
    }

    fn read_se(&mut self) -> Option<i32> {
        let code_num = self.read_ue()?;
        let mag = i32::try_from(code_num.div_ceil(2)).ok()?;
        if (code_num & 1) == 1 {
            Some(mag)
        } else {
            Some(-mag)
        }
    }
}

fn strip_h264_emulation_prevention(nal_body: &[u8], max_bytes: usize) -> Vec<u8> {
    let cap = nal_body.len().min(max_bytes);
    let mut rbsp = Vec::with_capacity(cap);
    let mut i = 0usize;
    while i < cap {
        if i + 2 < cap && nal_body[i] == 0x00 && nal_body[i + 1] == 0x00 && nal_body[i + 2] == 0x03
        {
            rbsp.push(0x00);
            rbsp.push(0x00);
            i += 3;
        } else {
            rbsp.push(nal_body[i]);
            i += 1;
        }
    }
    rbsp
}

fn skip_h264_scaling_list(br: &mut RbspBitReader<'_>, size: usize) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = br.read_se()?;
            next_scale = (last_scale.wrapping_add(delta).wrapping_add(256)).rem_euclid(256);
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
    Some(())
}

/// Decodes the cropped display `(width, height)` from a raw H.264 SPS NAL unit (`nal_unit_type == 7`,
/// without Annex-B start code or AVCC length prefix) per ITU-T H.264 §7.3.2.1.1.
pub fn parse_h264_sps_dimensions(sps_nal: &[u8]) -> Option<(u32, u32)> {
    if sps_nal.len() < 4 || (sps_nal[0] & 0x1F) != 7 {
        return None;
    }
    let rbsp = strip_h264_emulation_prevention(&sps_nal[1..], 1024);
    let mut br = RbspBitReader::new(&rbsp);

    let profile_idc = br.read_bits(8)?;
    let _constraint_flags = br.read_bits(8)?;
    let _level_idc = br.read_bits(8)?;
    let _sps_id = br.read_ue()?;

    let mut chroma_format_idc = 1u32;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = br.read_ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = br.read_bit()? == 1;
        }
        let _bit_depth_luma_minus8 = br.read_ue()?;
        let _bit_depth_chroma_minus8 = br.read_ue()?;
        let _qpprime_y_zero_transform_bypass = br.read_bit()?;
        let seq_scaling_matrix_present = br.read_bit()? == 1;
        if seq_scaling_matrix_present {
            let list_count = if chroma_format_idc != 3 {
                8usize
            } else {
                12usize
            };
            for idx in 0..list_count {
                if br.read_bit()? == 1 {
                    skip_h264_scaling_list(&mut br, if idx < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    let _log2_max_frame_num_minus4 = br.read_ue()?;
    let pic_order_cnt_type = br.read_ue()?;
    if pic_order_cnt_type == 0 {
        let _log2_max_pic_order_cnt_lsb_minus4 = br.read_ue()?;
    } else if pic_order_cnt_type == 1 {
        let _delta_pic_order_always_zero = br.read_bit()?;
        let _offset_for_non_ref_pic = br.read_se()?;
        let _offset_for_top_to_bottom_field = br.read_se()?;
        let num_ref_frames_in_cycle = br.read_ue()?;
        if num_ref_frames_in_cycle > 255 {
            return None;
        }
        for _ in 0..num_ref_frames_in_cycle {
            let _ = br.read_se()?;
        }
    } else if pic_order_cnt_type > 2 {
        return None;
    }

    let _max_num_ref_frames = br.read_ue()?;
    let _gaps_in_frame_num_allowed = br.read_bit()?;
    let pic_width_in_mbs_minus1 = br.read_ue()?;
    let pic_height_in_map_units_minus1 = br.read_ue()?;
    let frame_mbs_only_flag = br.read_bit()?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive_frame_field = br.read_bit()?;
    }
    let _direct_8x8_inference = br.read_bit()?;

    let mut crop_left = 0u32;
    let mut crop_right = 0u32;
    let mut crop_top = 0u32;
    let mut crop_bottom = 0u32;
    if br.read_bit()? == 1 {
        crop_left = br.read_ue()?;
        crop_right = br.read_ue()?;
        crop_top = br.read_ue()?;
        crop_bottom = br.read_ue()?;
    }

    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let frame_mbs_factor = 2u32.checked_sub(frame_mbs_only_flag)?;
    let (crop_unit_x, crop_unit_y) = match chroma_array_type {
        0 => (1u32, frame_mbs_factor),
        1 => (2u32, 2u32.checked_mul(frame_mbs_factor)?),
        2 => (2u32, frame_mbs_factor),
        3 => (1u32, frame_mbs_factor),
        _ => return None,
    };

    let raw_width = pic_width_in_mbs_minus1.checked_add(1)?.checked_mul(16)?;
    let raw_height = pic_height_in_map_units_minus1
        .checked_add(1)?
        .checked_mul(16)?
        .checked_mul(frame_mbs_factor)?;
    let width = raw_width.checked_sub(
        crop_left
            .checked_add(crop_right)?
            .checked_mul(crop_unit_x)?,
    )?;
    let height = raw_height.checked_sub(
        crop_top
            .checked_add(crop_bottom)?
            .checked_mul(crop_unit_y)?,
    )?;

    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return None;
    }
    Some((width, height))
}

/// Scans an ISO-BMFF fMP4 media fragment (`moof` + `mdat`) containing 4-byte length-prefixed AVCC
/// NAL units and returns the `(width, height)` from the first in-band H.264 SPS (`nal_unit_type == 7`).
pub fn extract_fmp4_fragment_h264_dimensions(fragment: &[u8]) -> Option<(u32, u32)> {
    let top_boxes = iter_boxes(fragment);
    let mdat = find_box(&top_boxes, b"mdat")?;
    let mut pos = 0usize;
    let mut nals_seen = 0usize;
    while pos.checked_add(5).is_some_and(|end| end <= mdat.len()) && nals_seen < 64 {
        nals_seen += 1;
        let nal_len =
            u32::from_be_bytes([mdat[pos], mdat[pos + 1], mdat[pos + 2], mdat[pos + 3]]) as usize;
        pos += 4;
        let Some(nal_end) = pos.checked_add(nal_len) else {
            break;
        };
        if nal_len == 0 || nal_end > mdat.len() {
            break;
        }
        let nal = &mdat[pos..nal_end];
        if (nal[0] & 0x1F) == 7
            && let Some(dims) = parse_h264_sps_dimensions(nal)
        {
            return Some(dims);
        }
        pos = nal_end;
    }
    None
}

/// Returns `true` if `init` is an ISO-BMFF fMP4 initialization segment whose `moov` carries
/// `go2rtc`'s hardcoded `128x96` placeholder H.264 SPS (`GO2RTC_PLACEHOLDER_H264_SPS`) and still
/// has `128x96` in its `vide` `tkhd` track header.
pub fn fmp4_init_has_placeholder_video_dimensions(init: &[u8]) -> bool {
    let top_boxes = iter_boxes(init);
    let Some(moov) = find_box(&top_boxes, b"moov") else {
        return false;
    };
    if !moov
        .windows(GO2RTC_PLACEHOLDER_H264_SPS.len())
        .any(|w| w == GO2RTC_PLACEHOLDER_H264_SPS)
    {
        return false;
    }
    for b in iter_boxes(moov) {
        if &b.box_type != b"trak" {
            continue;
        }
        let trak_boxes = iter_boxes(b.payload);
        let Some(tkhd) = find_box(&trak_boxes, b"tkhd") else {
            continue;
        };
        let Some(mdia) = find_box(&trak_boxes, b"mdia") else {
            continue;
        };
        let mdia_boxes = iter_boxes(mdia);
        let Some(hdlr) = find_box(&mdia_boxes, b"hdlr") else {
            continue;
        };
        let min_tkhd_len = match tkhd.first().copied() {
            Some(0) => 84usize,
            Some(1) => 96usize,
            _ => continue,
        };
        if hdlr.len() >= 12 && &hdlr[8..12] == b"vide" && tkhd.len() >= min_tkhd_len {
            let w = u32::from_be_bytes([
                tkhd[min_tkhd_len - 8],
                tkhd[min_tkhd_len - 7],
                tkhd[min_tkhd_len - 6],
                tkhd[min_tkhd_len - 5],
            ]) >> 16;
            let h = u32::from_be_bytes([
                tkhd[min_tkhd_len - 4],
                tkhd[min_tkhd_len - 3],
                tkhd[min_tkhd_len - 2],
                tkhd[min_tkhd_len - 1],
            ]) >> 16;
            if (w, h) == (128, 96) {
                return true;
            }
        }
    }
    false
}

fn next_box_range(buf: &[u8], pos: usize) -> Option<([u8; 4], usize, usize, usize)> {
    if pos.checked_add(8)? > buf.len() {
        return None;
    }
    let size32 = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
    let btype = [buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]];
    let (box_size, hdr_len) = if size32 == 1 {
        if pos.checked_add(16)? > buf.len() {
            return None;
        }
        let mut ext = [0u8; 8];
        ext.copy_from_slice(&buf[pos + 8..pos + 16]);
        let size64 = usize::try_from(u64::from_be_bytes(ext)).ok()?;
        (size64, 16usize)
    } else if size32 == 0 {
        (buf.len() - pos, 8usize)
    } else {
        (size32, 8usize)
    };
    let end = pos.checked_add(box_size)?;
    if box_size < hdr_len || end > buf.len() {
        return None;
    }
    Some((btype, pos + hdr_len, end, end))
}

fn patch_stsd_visual_sample_entries(stsd_payload: &mut [u8], w16: [u8; 2], h16: [u8; 2]) -> bool {
    if stsd_payload.len() < 8 {
        return false;
    }
    let mut changed = false;
    let entries_buf = &mut stsd_payload[8..];
    let mut pos = 0usize;
    let mut seen = 0usize;
    while let Some((btype, p_start, p_end, next_pos)) = next_box_range(entries_buf, pos) {
        seen += 1;
        if seen > MAX_BOXES_PER_LEVEL {
            break;
        }
        if matches!(&btype, b"avc1" | b"avc3" | b"hev1" | b"hvc1") {
            let entry = &mut entries_buf[p_start..p_end];
            if entry.len() >= 28 && (entry[24..26] != w16 || entry[26..28] != h16) {
                entry[24..26].copy_from_slice(&w16);
                entry[26..28].copy_from_slice(&h16);
                changed = true;
            }
        }
        pos = next_pos;
    }
    changed
}

/// Rewrites `vide` `tkhd` (16.16 fixed-point) and `avc1` (`u16`) dimensions in-place inside an
/// ISO-BMFF fMP4 initialization segment without altering box byte lengths.
/// Returns `Some(patched_bytes)` when at least one video dimension field was updated.
pub fn patch_fmp4_init_video_dimensions(init: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return None;
    }
    let w_fixed = (width << 16).to_be_bytes();
    let h_fixed = (height << 16).to_be_bytes();
    let w16 = u16::try_from(width).ok()?.to_be_bytes();
    let h16 = u16::try_from(height).ok()?.to_be_bytes();

    let mut out = init.to_vec();
    let mut changed = false;
    let mut top_pos = 0usize;
    let mut top_seen = 0usize;
    while let Some((top_type, moov_start, moov_end, next_top)) = next_box_range(&out, top_pos) {
        top_seen += 1;
        if top_seen > MAX_BOXES_PER_LEVEL {
            break;
        }
        if &top_type == b"moov" {
            let moov_slice = &mut out[moov_start..moov_end];
            let mut m_pos = 0usize;
            let mut m_seen = 0usize;
            while let Some((m_type, trak_start, trak_end, next_m)) =
                next_box_range(moov_slice, m_pos)
            {
                m_seen += 1;
                if m_seen > MAX_BOXES_PER_LEVEL {
                    break;
                }
                if &m_type == b"trak" {
                    let is_video_trak = {
                        let trak_boxes = iter_boxes(&moov_slice[trak_start..trak_end]);
                        find_box(&trak_boxes, b"mdia").is_some_and(|mdia| {
                            let mb = iter_boxes(mdia);
                            find_box(&mb, b"hdlr")
                                .is_some_and(|hdlr| hdlr.len() >= 12 && &hdlr[8..12] == b"vide")
                        })
                    };
                    if is_video_trak {
                        let trak_slice = &mut moov_slice[trak_start..trak_end];
                        let mut t_pos = 0usize;
                        let mut t_seen = 0usize;
                        while let Some((t_type, t_start, t_end, next_t)) =
                            next_box_range(trak_slice, t_pos)
                        {
                            t_seen += 1;
                            if t_seen > MAX_BOXES_PER_LEVEL {
                                break;
                            }
                            if &t_type == b"tkhd" {
                                let tkhd = &mut trak_slice[t_start..t_end];
                                let min_len = match tkhd.first().copied() {
                                    Some(0) => Some(84usize),
                                    Some(1) => Some(96usize),
                                    _ => None,
                                };
                                if let Some(ml) = min_len
                                    && tkhd.len() >= ml
                                    && (tkhd[ml - 8..ml - 4] != w_fixed
                                        || tkhd[ml - 4..ml] != h_fixed)
                                {
                                    tkhd[ml - 8..ml - 4].copy_from_slice(&w_fixed);
                                    tkhd[ml - 4..ml].copy_from_slice(&h_fixed);
                                    changed = true;
                                }
                            } else if &t_type == b"mdia" {
                                let mdia = &mut trak_slice[t_start..t_end];
                                let mut d_pos = 0usize;
                                while let Some((d_type, d_start, d_end, next_d)) =
                                    next_box_range(mdia, d_pos)
                                {
                                    if &d_type == b"minf" {
                                        let minf = &mut mdia[d_start..d_end];
                                        let mut i_pos = 0usize;
                                        while let Some((i_type, i_start, i_end, next_i)) =
                                            next_box_range(minf, i_pos)
                                        {
                                            if &i_type == b"stbl" {
                                                let stbl = &mut minf[i_start..i_end];
                                                let mut s_pos = 0usize;
                                                while let Some((s_type, s_start, s_end, next_s)) =
                                                    next_box_range(stbl, s_pos)
                                                {
                                                    if &s_type == b"stsd"
                                                        && patch_stsd_visual_sample_entries(
                                                            &mut stbl[s_start..s_end],
                                                            w16,
                                                            h16,
                                                        )
                                                    {
                                                        changed = true;
                                                    }
                                                    s_pos = next_s;
                                                }
                                            }
                                            i_pos = next_i;
                                        }
                                    }
                                    d_pos = next_d;
                                }
                            }
                            t_pos = next_t;
                        }
                    }
                }
                m_pos = next_m;
            }
        }
        top_pos = next_top;
    }
    changed.then_some(out)
}
