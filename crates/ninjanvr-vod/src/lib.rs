use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use ninjanvr_db::RecordingRow;

/// Probes the `(width, height)` video dimensions from an MP4 file's `vide` `tkhd` box in pure Rust.
pub fn probe_mp4_resolution(mp4_path: &Path) -> Option<(u32, u32)> {
    let (moov_bytes, _) = read_moov_from_file(mp4_path).ok()?;
    let moov_boxes = iter_boxes(&moov_bytes);
    for b in &moov_boxes {
        if &b.box_type == b"trak" {
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
            let min_tkhd_len = if tkhd.first().copied() == Some(1) {
                96
            } else {
                84
            };
            if hdlr.len() >= 12 && &hdlr[8..12] == b"vide" && tkhd.len() >= min_tkhd_len {
                let n = tkhd.len();
                let w =
                    u32::from_be_bytes([tkhd[n - 8], tkhd[n - 7], tkhd[n - 6], tkhd[n - 5]]) >> 16;
                let h =
                    u32::from_be_bytes([tkhd[n - 4], tkhd[n - 3], tkhd[n - 2], tkhd[n - 1]]) >> 16;
                if w > 0 && h > 0 {
                    return Some((w, h));
                }
            }
        }
    }
    None
}

/// Generates an HLS master playlist (`master.m3u8`).
pub fn build_master_playlist() -> String {
    [
        "#EXTM3U",
        "#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1500000,RESOLUTION=1920x1080,FRAME-RATE=20.000,CODECS=\"avc1.640028,mp4a.40.2\",VIDEO-RANGE=SDR",
        "index-v1-a1.m3u8",
        "",
    ]
    .join("\n")
}

/// Generates an HLS master playlist (`master.m3u8`).
pub fn build_master_m3u8() -> String {
    build_master_playlist()
}

/// Generates a VOD media playlist (`index-v1-a1.m3u8`) from a slice of recording segments.
pub fn build_media_playlist(segments: &[RecordingRow]) -> Result<String> {
    build_media_playlist_with_resolutions(segments, None)
}

/// Generates a VOD media playlist (`index-v1-a1.m3u8`) from a slice of recording segments.
pub fn build_media_m3u8(segments: &[RecordingRow]) -> Result<String> {
    build_media_playlist(segments)
}

fn format_program_date_time_utc(epoch_sec: f64) -> String {
    let total_ms = (epoch_sec.max(0.0) * 1000.0).round() as i64;
    let secs = total_ms.div_euclid(1000);
    let ms = total_ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let day_secs = secs.rem_euclid(86400);
    let hh = day_secs / 3600;
    let mm = (day_secs % 3600) / 60;
    let ss = day_secs % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z")
}

/// Generates a VOD media playlist (`index-v1-a1.m3u8`) supporting hybrid sub-stream / main-stream
/// resolution switches via `#EXT-X-DISCONTINUITY` and updated `#EXT-X-MAP` tags.
pub fn build_media_playlist_with_resolutions(
    segments: &[RecordingRow],
    resolutions: Option<&[(u32, u32)]>,
) -> Result<String> {
    let max_dur = segments
        .iter()
        .map(|s| s.duration.ceil() as u64)
        .max()
        .unwrap_or(10)
        .max(1);

    let mut lines = vec![
        "#EXTM3U".to_string(),
        format!("#EXT-X-TARGETDURATION:{max_dur}"),
        "#EXT-X-ALLOW-CACHE:YES".to_string(),
        "#EXT-X-PLAYLIST-TYPE:VOD".to_string(),
        "#EXT-X-VERSION:7".to_string(),
        "#EXT-X-MEDIA-SEQUENCE:1".to_string(),
        "#EXT-X-MAP:URI=\"init-v1-a1.mp4\"".to_string(),
    ];

    let mut prev_end: Option<f64> = None;
    let mut prev_res: Option<(u32, u32)> = None;
    let mut prev_rate: Option<f64> = None;
    for (idx, seg) in segments.iter().enumerate() {
        let cur_res = resolutions.and_then(|r| r.get(idx).copied());
        let dur = seg.duration.max(0.1);
        let cur_rate = seg.segment_size / dur;

        let time_gap = prev_end
            .map(|pe| (seg.start_time - pe).abs() > 1.5)
            .unwrap_or(false);
        let res_changed = match (prev_res, cur_res) {
            (Some(pr), Some(cr)) => pr != cr,
            _ => false,
        };
        let tier_switched = match (prev_res, cur_res, prev_rate) {
            (Some(pr_res), Some(cr_res), _) if pr_res == cr_res => false,
            (_, _, Some(pr)) if pr > 0.001 && cur_rate > 0.001 => {
                (cur_rate / pr) > 3.2 || (pr / cur_rate) > 3.2
            }
            _ => false,
        };

        if time_gap || res_changed || tier_switched {
            lines.push("#EXT-X-DISCONTINUITY".to_string());
            if res_changed || tier_switched {
                lines.push(format!("#EXT-X-MAP:URI=\"init-v1-a1.mp4?seg={}\"", idx + 1));
            }
        }
        lines.push(format!(
            "#EXT-X-PROGRAM-DATE-TIME:{}",
            format_program_date_time_utc(seg.start_time)
        ));
        lines.push(format!("#EXTINF:{dur:.3},"));
        lines.push(format!("seg-{}-v1-a1.m4s", idx + 1));
        prev_end = Some(seg.end_time);
        if cur_res.is_some() {
            prev_res = cur_res;
        }
        prev_rate = Some(cur_rate);
    }

    lines.push("#EXT-X-ENDLIST".to_string());
    lines.push(String::new());
    Ok(lines.join("\n"))
}

#[derive(Debug, Clone)]
struct Mp4BoxSlice<'a> {
    box_type: [u8; 4],
    payload: &'a [u8],
}

fn iter_boxes(mut buf: &[u8]) -> Vec<Mp4BoxSlice<'_>> {
    let mut out = Vec::new();
    while buf.len() >= 8 {
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let box_type = [buf[4], buf[5], buf[6], buf[7]];
        let (box_len, hdr_len) = if size32 == 1 {
            if buf.len() < 16 {
                break;
            }
            let size64 = u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]) as usize;
            (size64, 16)
        } else if size32 == 0 {
            (buf.len(), 8)
        } else {
            (size32, 8)
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

fn find_box<'a>(boxes: &'a [Mp4BoxSlice<'a>], name: &[u8; 4]) -> Option<&'a [u8]> {
    boxes
        .iter()
        .find(|b| &b.box_type == name)
        .map(|b| b.payload)
}

fn make_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
    out
}

fn make_full_box(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let total = (12 + body.len()) as u32;
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
    out.extend_from_slice(&vf.to_be_bytes());
    out.extend_from_slice(body);
    out
}

#[derive(Debug, Clone)]
struct ParsedSample {
    offset: usize,
    size: u32,
    duration: u32,
    cts_offset: i32,
    is_sync: bool,
}

#[derive(Debug, Clone)]
struct ParsedTrack {
    track_id: u32,
    handler_type: [u8; 4],
    timescale: u32,
    tkhd_payload: Vec<u8>,
    mdhd_payload: Vec<u8>,
    hdlr_payload: Vec<u8>,
    media_header_box: Vec<u8>,
    dinf_box: Vec<u8>,
    stsd_payload: Vec<u8>,
    samples: Vec<ParsedSample>,
}

fn read_moov_from_file(path: &Path) -> Result<(Vec<u8>, usize)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open MP4 segment {}", path.display()))?;
    let file_len = file.metadata()?.len();
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    while pos + 8 <= file_len {
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut hdr)?;
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (box_size, hdr_len) = if size32 == 1 {
            let mut ext = [0u8; 8];
            file.read_exact(&mut ext)?;
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        if box_size < hdr_len || pos + box_size > file_len {
            break;
        }
        if &btype == b"moov" {
            let payload_len = (box_size - hdr_len) as usize;
            let mut buf = vec![0u8; payload_len];
            file.read_exact(&mut buf)?;
            return Ok((buf, file_len as usize));
        }
        pos += box_size;
    }
    bail!("no moov box found in {}", path.display())
}

fn parse_tracks_from_moov(
    moov_payload: &[u8],
    file_len: usize,
) -> Result<(Vec<u8>, Vec<ParsedTrack>)> {
    let moov_boxes = iter_boxes(moov_payload);
    let mvhd_payload = find_box(&moov_boxes, b"mvhd")
        .ok_or_else(|| anyhow!("missing mvhd inside moov"))?
        .to_vec();

    let mut tracks = Vec::new();
    for b in &moov_boxes {
        if &b.box_type != b"trak" {
            continue;
        }
        if let Ok(mut t) = parse_single_trak(b.payload) {
            // Sanitize sample bounds; if an audio (`soun`) track has corrupted or out-of-bounds
            // samples or an empty sample table, strip the corrupted audio samples/track cleanly
            // so video (`vide`) fMP4 fragment generation never fails.
            if &t.handler_type == b"soun" {
                t.samples.retain(|s| {
                    s.size > 0
                        && s.duration > 0
                        && s.offset
                            .checked_add(s.size as usize)
                            .map(|end| end <= file_len)
                            .unwrap_or(false)
                });
                if t.samples.is_empty() {
                    continue;
                }
            } else {
                t.samples.retain(|s| {
                    s.size > 0
                        && s.offset
                            .checked_add(s.size as usize)
                            .map(|end| end <= file_len)
                            .unwrap_or(false)
                });
                if t.samples.is_empty() {
                    continue;
                }
            }
            tracks.push(t);
        }
    }
    if tracks.is_empty() {
        bail!("no valid tracks found in moov");
    }
    Ok((mvhd_payload, tracks))
}

fn parse_single_trak(trak_payload: &[u8]) -> Result<ParsedTrack> {
    let trak_boxes = iter_boxes(trak_payload);
    let tkhd = find_box(&trak_boxes, b"tkhd").ok_or_else(|| anyhow!("missing tkhd"))?;
    if tkhd.len() < 20 {
        bail!("tkhd too short");
    }
    let tkhd_ver = tkhd[0];
    let track_id = if tkhd_ver == 1 {
        if tkhd.len() < 24 {
            bail!("tkhd v1 too short");
        }
        u32::from_be_bytes([tkhd[20], tkhd[21], tkhd[22], tkhd[23]])
    } else {
        u32::from_be_bytes([tkhd[12], tkhd[13], tkhd[14], tkhd[15]])
    };

    let mdia = find_box(&trak_boxes, b"mdia").ok_or_else(|| anyhow!("missing mdia"))?;
    let mdia_boxes = iter_boxes(mdia);
    let mdhd = find_box(&mdia_boxes, b"mdhd").ok_or_else(|| anyhow!("missing mdhd"))?;
    if mdhd.len() < 20 {
        bail!("mdhd too short");
    }
    let mdhd_ver = mdhd[0];
    let timescale = if mdhd_ver == 1 {
        if mdhd.len() < 24 {
            bail!("mdhd v1 too short");
        }
        u32::from_be_bytes([mdhd[20], mdhd[21], mdhd[22], mdhd[23]])
    } else {
        u32::from_be_bytes([mdhd[12], mdhd[13], mdhd[14], mdhd[15]])
    }
    .max(1);

    let hdlr = find_box(&mdia_boxes, b"hdlr").ok_or_else(|| anyhow!("missing hdlr"))?;
    if hdlr.len() < 12 {
        bail!("hdlr too short");
    }
    let handler_type = [hdlr[8], hdlr[9], hdlr[10], hdlr[11]];

    let minf = find_box(&mdia_boxes, b"minf").ok_or_else(|| anyhow!("missing minf"))?;
    let minf_boxes = iter_boxes(minf);

    let media_header_box = if let Some(vmhd) = find_box(&minf_boxes, b"vmhd") {
        make_box(b"vmhd", vmhd)
    } else if let Some(smhd) = find_box(&minf_boxes, b"smhd") {
        make_box(b"smhd", smhd)
    } else {
        make_full_box(b"nmhd", 0, 0, &[])
    };

    let dinf_box = if let Some(dinf) = find_box(&minf_boxes, b"dinf") {
        make_box(b"dinf", dinf)
    } else {
        let dref = make_full_box(
            b"dref",
            0,
            0,
            &[
                0, 0, 0, 1, // entry_count = 1
                0, 0, 0, 12, b'u', b'r', b'l', b' ', 0, 0, 0, 1,
            ],
        );
        make_box(b"dinf", &dref)
    };

    let stbl = find_box(&minf_boxes, b"stbl").ok_or_else(|| anyhow!("missing stbl"))?;
    let stbl_boxes = iter_boxes(stbl);
    let stsd = find_box(&stbl_boxes, b"stsd").ok_or_else(|| anyhow!("missing stsd"))?;

    let samples = parse_sample_table(&stbl_boxes, &handler_type)?;

    Ok(ParsedTrack {
        track_id,
        handler_type,
        timescale,
        tkhd_payload: tkhd.to_vec(),
        mdhd_payload: mdhd.to_vec(),
        hdlr_payload: hdlr.to_vec(),
        media_header_box,
        dinf_box,
        stsd_payload: stsd.to_vec(),
        samples,
    })
}

fn parse_sample_table(
    stbl_boxes: &[Mp4BoxSlice<'_>],
    handler_type: &[u8; 4],
) -> Result<Vec<ParsedSample>> {
    // 1. Parse stsz (sample sizes)
    let stsz = find_box(stbl_boxes, b"stsz").ok_or_else(|| anyhow!("missing stsz"))?;
    if stsz.len() < 12 {
        bail!("stsz too short");
    }
    let fixed_size = u32::from_be_bytes([stsz[4], stsz[5], stsz[6], stsz[7]]);
    let sample_count = u32::from_be_bytes([stsz[8], stsz[9], stsz[10], stsz[11]]) as usize;
    let mut sizes = Vec::with_capacity(sample_count);
    if fixed_size != 0 {
        sizes.resize(sample_count, fixed_size);
    } else {
        if stsz.len() < 12 + sample_count * 4 {
            bail!("stsz table truncated");
        }
        for i in 0..sample_count {
            let p = 12 + i * 4;
            sizes.push(u32::from_be_bytes([
                stsz[p],
                stsz[p + 1],
                stsz[p + 2],
                stsz[p + 3],
            ]));
        }
    }

    // 2. Parse stts (sample durations)
    let stts = find_box(stbl_boxes, b"stts").ok_or_else(|| anyhow!("missing stts"))?;
    let mut durations = Vec::with_capacity(sample_count);
    if stts.len() >= 8 {
        let entry_count = u32::from_be_bytes([stts[4], stts[5], stts[6], stts[7]]) as usize;
        for i in 0..entry_count {
            let p = 8 + i * 8;
            if p + 8 > stts.len() {
                break;
            }
            let count = u32::from_be_bytes([stts[p], stts[p + 1], stts[p + 2], stts[p + 3]]);
            let delta = u32::from_be_bytes([stts[p + 4], stts[p + 5], stts[p + 6], stts[p + 7]]);
            for _ in 0..count {
                if durations.len() < sample_count {
                    durations.push(delta);
                }
            }
        }
    }
    while durations.len() < sample_count {
        durations.push(durations.last().copied().unwrap_or(1024));
    }

    // 3. Parse ctts (optional composition time offsets)
    let mut cts_offsets = vec![0i32; sample_count];
    if let Some(ctts) = find_box(stbl_boxes, b"ctts")
        && ctts.len() >= 8
    {
        let entry_count = u32::from_be_bytes([ctts[4], ctts[5], ctts[6], ctts[7]]) as usize;
        let mut idx = 0usize;
        for i in 0..entry_count {
            let p = 8 + i * 8;
            if p + 8 > ctts.len() {
                break;
            }
            let count =
                u32::from_be_bytes([ctts[p], ctts[p + 1], ctts[p + 2], ctts[p + 3]]) as usize;
            let offset = i32::from_be_bytes([ctts[p + 4], ctts[p + 5], ctts[p + 6], ctts[p + 7]]);
            for _ in 0..count {
                if idx < sample_count {
                    cts_offsets[idx] = offset;
                    idx += 1;
                }
            }
        }
    }

    // 4. Parse stss (sync samples / keyframes)
    let mut sync_flags = vec![handler_type != b"vide"; sample_count];
    if let Some(stss) = find_box(stbl_boxes, b"stss")
        && stss.len() >= 8
    {
        let entry_count = u32::from_be_bytes([stss[4], stss[5], stss[6], stss[7]]) as usize;
        for i in 0..entry_count {
            let p = 8 + i * 4;
            if p + 4 > stss.len() {
                break;
            }
            let sample_num =
                u32::from_be_bytes([stss[p], stss[p + 1], stss[p + 2], stss[p + 3]]) as usize;
            if sample_num >= 1 && sample_num <= sample_count {
                sync_flags[sample_num - 1] = true;
            }
        }
    } else if handler_type == b"vide" && !sync_flags.is_empty() {
        sync_flags[0] = true;
    }

    // 5. Parse stco / co64 (chunk offsets) and stsc (sample-to-chunk)
    let mut chunk_offsets = Vec::new();
    if let Some(stco) = find_box(stbl_boxes, b"stco")
        && stco.len() >= 8
    {
        let entry_count = u32::from_be_bytes([stco[4], stco[5], stco[6], stco[7]]) as usize;
        for i in 0..entry_count {
            let p = 8 + i * 4;
            if p + 4 <= stco.len() {
                chunk_offsets.push(u32::from_be_bytes([
                    stco[p],
                    stco[p + 1],
                    stco[p + 2],
                    stco[p + 3],
                ]) as usize);
            }
        }
    } else if let Some(co64) = find_box(stbl_boxes, b"co64")
        && co64.len() >= 8
    {
        let entry_count = u32::from_be_bytes([co64[4], co64[5], co64[6], co64[7]]) as usize;
        for i in 0..entry_count {
            let p = 8 + i * 8;
            if p + 8 <= co64.len() {
                chunk_offsets.push(u64::from_be_bytes([
                    co64[p],
                    co64[p + 1],
                    co64[p + 2],
                    co64[p + 3],
                    co64[p + 4],
                    co64[p + 5],
                    co64[p + 6],
                    co64[p + 7],
                ]) as usize);
            }
        }
    }

    let stsc = find_box(stbl_boxes, b"stsc").ok_or_else(|| anyhow!("missing stsc"))?;
    let mut stsc_entries = Vec::new();
    if stsc.len() >= 8 {
        let entry_count = u32::from_be_bytes([stsc[4], stsc[5], stsc[6], stsc[7]]) as usize;
        for i in 0..entry_count {
            let p = 8 + i * 12;
            if p + 12 <= stsc.len() {
                let first_chunk =
                    u32::from_be_bytes([stsc[p], stsc[p + 1], stsc[p + 2], stsc[p + 3]]) as usize;
                let samples_per_chunk =
                    u32::from_be_bytes([stsc[p + 4], stsc[p + 5], stsc[p + 6], stsc[p + 7]])
                        as usize;
                stsc_entries.push((first_chunk.max(1), samples_per_chunk));
            }
        }
    }

    let mut samples = Vec::with_capacity(sample_count);
    let mut sample_idx = 0usize;
    for (chunk_idx_0, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let chunk_num_1 = chunk_idx_0 + 1;
        let mut spc = 1usize;
        for &(first_c, count_c) in &stsc_entries {
            if chunk_num_1 >= first_c {
                spc = count_c;
            } else {
                break;
            }
        }
        let mut cur_offset = chunk_offset;
        for _ in 0..spc {
            if sample_idx >= sample_count {
                break;
            }
            let sz = sizes[sample_idx];
            samples.push(ParsedSample {
                offset: cur_offset,
                size: sz,
                duration: durations[sample_idx],
                cts_offset: cts_offsets[sample_idx],
                is_sync: sync_flags[sample_idx],
            });
            cur_offset += sz as usize;
            sample_idx += 1;
        }
        if sample_idx >= sample_count {
            break;
        }
    }

    Ok(samples)
}

/// Synthesizes an HLS fMP4 initialization segment (`init-v1-a1.mp4`: `ftyp` + `moov` with `mvex`)
/// from the `moov` atom of a standard MP4 recording file.
pub fn build_init_mp4(first_segment_path: &Path) -> Result<Vec<u8>> {
    let (moov_payload, file_len) = read_moov_from_file(first_segment_path)?;
    let (mut mvhd, tracks) = parse_tracks_from_moov(&moov_payload, file_len)?;

    // Zero out duration in mvhd so players treat this as a fragmented stream
    if mvhd.len() >= 20 {
        if mvhd[0] == 1 && mvhd.len() >= 32 {
            mvhd[24..32].fill(0);
        } else {
            mvhd[16..20].fill(0);
        }
    }

    let mut moov_children = Vec::new();
    moov_children.extend_from_slice(&make_box(b"mvhd", &mvhd));

    let mut mvex_children = Vec::new();

    for t in &tracks {
        let mut tkhd = t.tkhd_payload.clone();
        if tkhd.len() >= 24 {
            if tkhd[0] == 1 && tkhd.len() >= 36 {
                tkhd[28..36].fill(0);
            } else {
                tkhd[20..24].fill(0);
            }
        }
        let mut mdhd = t.mdhd_payload.clone();
        if mdhd.len() >= 20 {
            if mdhd[0] == 1 && mdhd.len() >= 32 {
                mdhd[24..32].fill(0);
            } else {
                mdhd[16..20].fill(0);
            }
        }

        // Empty sample table boxes required by ISO-BMFF for fragmented MP4 init segments
        let mut stbl_children = Vec::new();
        stbl_children.extend_from_slice(&make_box(b"stsd", &t.stsd_payload));
        stbl_children.extend_from_slice(&make_full_box(b"stts", 0, 0, &[0, 0, 0, 0]));
        stbl_children.extend_from_slice(&make_full_box(b"stsc", 0, 0, &[0, 0, 0, 0]));
        stbl_children.extend_from_slice(&make_full_box(b"stsz", 0, 0, &[0, 0, 0, 0, 0, 0, 0, 0]));
        stbl_children.extend_from_slice(&make_full_box(b"stco", 0, 0, &[0, 0, 0, 0]));
        let stbl_box = make_box(b"stbl", &stbl_children);

        let mut minf_children = Vec::new();
        minf_children.extend_from_slice(&t.media_header_box);
        minf_children.extend_from_slice(&t.dinf_box);
        minf_children.extend_from_slice(&stbl_box);
        let minf_box = make_box(b"minf", &minf_children);

        let mut mdia_children = Vec::new();
        mdia_children.extend_from_slice(&make_box(b"mdhd", &mdhd));
        mdia_children.extend_from_slice(&make_box(b"hdlr", &t.hdlr_payload));
        mdia_children.extend_from_slice(&minf_box);
        let mdia_box = make_box(b"mdia", &mdia_children);

        let mut trak_children = Vec::new();
        trak_children.extend_from_slice(&make_box(b"tkhd", &tkhd));
        trak_children.extend_from_slice(&mdia_box);
        moov_children.extend_from_slice(&make_box(b"trak", &trak_children));

        // Build trex entry for mvex
        let mut trex_body = Vec::with_capacity(20);
        trex_body.extend_from_slice(&t.track_id.to_be_bytes());
        trex_body.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index = 1
        trex_body.extend_from_slice(&0u32.to_be_bytes()); // default_sample_duration = 0
        trex_body.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size = 0
        trex_body.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags = 0
        mvex_children.extend_from_slice(&make_full_box(b"trex", 0, 0, &trex_body));
    }

    moov_children.extend_from_slice(&make_box(b"mvex", &mvex_children));

    let ftyp_payload = b"isom\x00\x00\x02\x00isomiso6dashmp41";
    let mut out = make_box(b"ftyp", ftyp_payload);
    out.extend_from_slice(&make_box(b"moov", &moov_children));
    Ok(out)
}

/// Synthesizes an HLS fMP4 media segment (`seg-N-v1-a1.m4s`: `moof` + `mdat`)
/// from a standard MP4 recording file on disk.
pub fn build_media_segment_m4s(
    segment_path: &Path,
    sequence_number: u32,
    base_time_seconds: f64,
) -> Result<Vec<u8>> {
    let file_bytes = std::fs::read(segment_path)
        .with_context(|| format!("failed to read MP4 file {}", segment_path.display()))?;
    let top_boxes = iter_boxes(&file_bytes);
    let moov_payload =
        find_box(&top_boxes, b"moov").ok_or_else(|| anyhow!("missing moov in segment"))?;
    let (_mvhd, tracks) = parse_tracks_from_moov(moov_payload, file_bytes.len())?;

    // Two-pass moof construction so `data_offset` in each `trun` points accurately into `mdat`
    let build_moof = |data_offsets: &[i32]| -> Vec<u8> {
        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&make_full_box(
            b"mfhd",
            0,
            0,
            &sequence_number.to_be_bytes(),
        ));

        for (idx, t) in tracks.iter().enumerate() {
            let mut traf_children = Vec::new();
            // tfhd: flags = 0x020000 (default-base-is-moof)
            traf_children.extend_from_slice(&make_full_box(
                b"tfhd",
                0,
                0x020000,
                &t.track_id.to_be_bytes(),
            ));

            // tfdt v1: 64-bit baseMediaDecodeTime in track timescale units
            let base_dts = (base_time_seconds.max(0.0) * (t.timescale as f64)).round() as u64;
            traf_children.extend_from_slice(&make_full_box(b"tfdt", 1, 0, &base_dts.to_be_bytes()));

            let has_cts = t.samples.iter().any(|s| s.cts_offset != 0);
            // trun flags:
            // 0x000001 = data-offset-present
            // 0x000100 = sample-duration-present
            // 0x000200 = sample-size-present
            // 0x000400 = sample-flags-present
            // 0x000800 = sample-composition-time-offsets-present
            let trun_flags = if has_cts { 0x000F01 } else { 0x000701 };
            let mut trun_body =
                Vec::with_capacity(8 + t.samples.len() * if has_cts { 16 } else { 12 });
            trun_body.extend_from_slice(&(t.samples.len() as u32).to_be_bytes());
            trun_body.extend_from_slice(&data_offsets[idx].to_be_bytes());

            for s in &t.samples {
                trun_body.extend_from_slice(&s.duration.to_be_bytes());
                trun_body.extend_from_slice(&s.size.to_be_bytes());
                let flags: u32 = if s.is_sync {
                    0x0200_0000 // depends_on=2 (does not depend on others / I-frame)
                } else {
                    0x0101_0000 // depends_on=1, is_non_sync=1
                };
                trun_body.extend_from_slice(&flags.to_be_bytes());
                if has_cts {
                    trun_body.extend_from_slice(&s.cts_offset.to_be_bytes());
                }
            }

            let trun_version = if has_cts { 1 } else { 0 };
            traf_children.extend_from_slice(&make_full_box(
                b"trun",
                trun_version,
                trun_flags,
                &trun_body,
            ));
            moof_children.extend_from_slice(&make_box(b"traf", &traf_children));
        }

        make_box(b"moof", &moof_children)
    };

    let dummy_offsets = vec![0i32; tracks.len()];
    let initial_moof = build_moof(&dummy_offsets);
    let moof_len = initial_moof.len() as i32;

    let mut real_offsets = Vec::with_capacity(tracks.len());
    let mut total_mdat_payload = 0usize;
    for t in &tracks {
        // +8 for the mdat box header (size + 'mdat')
        real_offsets.push(moof_len + 8 + (total_mdat_payload as i32));
        for s in &t.samples {
            total_mdat_payload += s.size as usize;
        }
    }

    let final_moof = build_moof(&real_offsets);
    let mut out = Vec::with_capacity(final_moof.len() + 8 + total_mdat_payload);
    out.extend_from_slice(&final_moof);
    out.extend_from_slice(&((8 + total_mdat_payload) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");

    for t in &tracks {
        for s in &t.samples {
            let end = s.offset + (s.size as usize);
            if end <= file_bytes.len() {
                out.extend_from_slice(&file_bytes[s.offset..end]);
            } else {
                bail!("sample offset out of bounds in {}", segment_path.display());
            }
        }
    }

    Ok(out)
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
    let default_flags = if (tf_flags & 0x000020) != 0 && pos + 4 <= tfhd.len() {
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
        let (track_id, default_flags) = find_box(&traf_boxes, b"tfhd")
            .map(parse_tfhd_track_and_default_flags)
            .unwrap_or((None, None));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_and_media_playlists_include_discontinuity_on_gap() {
        let master = build_master_playlist();
        assert!(master.contains("index-v1-a1.m3u8"));

        let segs = vec![
            RecordingRow {
                id: "1".to_string(),
                camera: "cam".to_string(),
                path: "/a.mp4".to_string(),
                start_time: 100.0,
                end_time: 110.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 1.0,
                regions: 0,
                motion_heatmap: None,
            },
            RecordingRow {
                id: "2".to_string(),
                camera: "cam".to_string(),
                path: "/b.mp4".to_string(),
                start_time: 120.0,
                end_time: 130.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 1.0,
                regions: 0,
                motion_heatmap: None,
            },
        ];
        let m3u8 = build_media_playlist(&segs).unwrap();
        assert!(m3u8.contains("#EXT-X-MAP:URI=\"init-v1-a1.mp4\""));
        assert!(m3u8.contains("#EXT-X-DISCONTINUITY"));
        assert!(m3u8.contains("seg-1-v1-a1.m4s"));
        assert!(m3u8.contains("seg-2-v1-a1.m4s"));
    }

    #[test]
    fn remuxes_video_when_soun_track_is_corrupted_or_out_of_bounds() {
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let video_sample = vec![0x65u8; 64];
        let mdat_offset = (ftyp.len() + 8) as u32;
        let mdat = make_box(b"mdat", &video_sample);

        let build_trak =
            |track_id: u32, handler: &[u8; 4], sample_offset: u32, sample_size: u32| {
                let mut tkhd_body = vec![0u8; 80];
                tkhd_body[8..12].copy_from_slice(&track_id.to_be_bytes());
                let tkhd = make_full_box(b"tkhd", 0, 3, &tkhd_body);

                let mut mdhd_body = vec![0u8; 20];
                mdhd_body[8..12].copy_from_slice(&90000u32.to_be_bytes());
                mdhd_body[12..16].copy_from_slice(&90000u32.to_be_bytes());
                let mdhd = make_full_box(b"mdhd", 0, 0, &mdhd_body);

                let mut hdlr_body = vec![0u8; 20];
                hdlr_body[4..8].copy_from_slice(handler);
                let hdlr = make_full_box(b"hdlr", 0, 0, &hdlr_body);

                let stsd = make_full_box(b"stsd", 0, 0, &[0, 0, 0, 1]);
                let mut stsz_body = Vec::new();
                stsz_body.extend_from_slice(&sample_size.to_be_bytes());
                stsz_body.extend_from_slice(&1u32.to_be_bytes());
                let stsz = make_full_box(b"stsz", 0, 0, &stsz_body);

                let mut stts_body = Vec::new();
                stts_body.extend_from_slice(&1u32.to_be_bytes());
                stts_body.extend_from_slice(&1u32.to_be_bytes());
                stts_body.extend_from_slice(&3000u32.to_be_bytes());
                let stts = make_full_box(b"stts", 0, 0, &stts_body);

                let mut stsc_body = Vec::new();
                stsc_body.extend_from_slice(&1u32.to_be_bytes());
                stsc_body.extend_from_slice(&1u32.to_be_bytes());
                stsc_body.extend_from_slice(&1u32.to_be_bytes());
                stsc_body.extend_from_slice(&1u32.to_be_bytes());
                let stsc = make_full_box(b"stsc", 0, 0, &stsc_body);

                let mut stco_body = Vec::new();
                stco_body.extend_from_slice(&1u32.to_be_bytes());
                stco_body.extend_from_slice(&sample_offset.to_be_bytes());
                let stco = make_full_box(b"stco", 0, 0, &stco_body);

                let mut stbl_children = Vec::new();
                stbl_children.extend_from_slice(&stsd);
                stbl_children.extend_from_slice(&stsz);
                stbl_children.extend_from_slice(&stts);
                stbl_children.extend_from_slice(&stsc);
                stbl_children.extend_from_slice(&stco);
                let stbl = make_box(b"stbl", &stbl_children);
                let minf = make_box(b"minf", &stbl);

                let mut mdia_children = Vec::new();
                mdia_children.extend_from_slice(&mdhd);
                mdia_children.extend_from_slice(&hdlr);
                mdia_children.extend_from_slice(&minf);
                let mdia = make_box(b"mdia", &mdia_children);

                let mut trak_children = Vec::new();
                trak_children.extend_from_slice(&tkhd);
                trak_children.extend_from_slice(&mdia);
                make_box(b"trak", &trak_children)
            };

        let mvhd = make_full_box(b"mvhd", 0, 0, &[0u8; 96]);
        let vide_trak = build_trak(1, b"vide", mdat_offset, video_sample.len() as u32);
        // Corrupted soun track pointing far past EOF (999_999)
        let corrupt_soun_trak = build_trak(2, b"soun", 999_999, 512);

        let mut moov_children = Vec::new();
        moov_children.extend_from_slice(&mvhd);
        moov_children.extend_from_slice(&vide_trak);
        moov_children.extend_from_slice(&corrupt_soun_trak);
        let moov = make_box(b"moov", &moov_children);

        let mut mp4_bytes = Vec::new();
        mp4_bytes.extend_from_slice(&ftyp);
        mp4_bytes.extend_from_slice(&mdat);
        mp4_bytes.extend_from_slice(&moov);

        let tmp_path = std::env::temp_dir().join("ninjanvr-vod-corrupt-soun-test.mp4");
        std::fs::write(&tmp_path, &mp4_bytes).unwrap();

        let init = build_init_mp4(&tmp_path).expect("init mp4 should succeed despite corrupt soun");
        let m4s = build_media_segment_m4s(&tmp_path, 1, 0.0)
            .expect("m4s segment should succeed despite corrupt soun");

        assert!(init.len() > 64);
        assert!(m4s.ends_with(&video_sample));
        let _ = std::fs::remove_file(&tmp_path);
    }

    #[test]
    fn hybrid_substream_and_mainstream_segments_emit_discontinuity_and_map_tags() {
        let segs = vec![
            RecordingRow {
                id: "sub1".to_string(),
                camera: "cam".to_string(),
                path: "/sub1.mp4".to_string(),
                start_time: 100.0,
                end_time: 110.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 0.4, // 640x360 substream
                regions: 0,
                motion_heatmap: None,
            },
            RecordingRow {
                id: "main2".to_string(),
                camera: "cam".to_string(),
                path: "/main2.mp4".to_string(),
                start_time: 110.0,
                end_time: 120.0,
                duration: 10.0,
                motion: 120,
                objects: 1,
                dbfs: 0,
                segment_size: 4.5, // 1920x1080 main stream event
                regions: 1,
                motion_heatmap: None,
            },
        ];
        let resolutions = [(640, 360), (1920, 1080)];
        let m3u8 = build_media_playlist_with_resolutions(&segs, Some(&resolutions)).unwrap();
        assert!(m3u8.contains("#EXT-X-DISCONTINUITY"));
        assert!(m3u8.contains("#EXT-X-MAP:URI=\"init-v1-a1.mp4?seg=2\""));
    }

    #[test]
    fn probe_mp4_resolution_parses_v0_and_v1_tkhd_fixed_point_dimensions() {
        let build_resolution_mp4 = |version: u8, width: u32, height: u32| -> Vec<u8> {
            let body_len = if version == 1 { 92 } else { 80 };
            let mut tkhd_body = vec![0u8; body_len];
            let track_id_pos = if version == 1 { 16 } else { 8 };
            tkhd_body[track_id_pos..track_id_pos + 4].copy_from_slice(&1u32.to_be_bytes());
            let w_fixed = width << 16;
            let h_fixed = height << 16;
            tkhd_body[body_len - 8..body_len - 4].copy_from_slice(&w_fixed.to_be_bytes());
            tkhd_body[body_len - 4..body_len].copy_from_slice(&h_fixed.to_be_bytes());
            let tkhd = make_full_box(b"tkhd", version, 3, &tkhd_body);

            let mut hdlr_body = vec![0u8; 20];
            hdlr_body[4..8].copy_from_slice(b"vide");
            let hdlr = make_full_box(b"hdlr", 0, 0, &hdlr_body);
            let mdia = make_box(b"mdia", &hdlr);

            let mut trak_children = Vec::new();
            trak_children.extend_from_slice(&tkhd);
            trak_children.extend_from_slice(&mdia);
            let trak = make_box(b"trak", &trak_children);
            let moov = make_box(b"moov", &trak);

            let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
            let mut out = Vec::new();
            out.extend_from_slice(&ftyp);
            out.extend_from_slice(&moov);
            out
        };

        let tmp_v0 = std::env::temp_dir().join("ninjanvr-vod-res-v0-1920x1080.mp4");
        let tmp_v1 = std::env::temp_dir().join("ninjanvr-vod-res-v1-640x360.mp4");
        std::fs::write(&tmp_v0, build_resolution_mp4(0, 1920, 1080)).unwrap();
        std::fs::write(&tmp_v1, build_resolution_mp4(1, 640, 360)).unwrap();

        assert_eq!(probe_mp4_resolution(&tmp_v0), Some((1920, 1080)));
        assert_eq!(probe_mp4_resolution(&tmp_v1), Some((640, 360)));

        let _ = std::fs::remove_file(&tmp_v0);
        let _ = std::fs::remove_file(&tmp_v1);
    }

    #[test]
    fn build_master_m3u8_and_build_media_m3u8_format_hls_v7_tags_and_program_date_time() {
        let master = build_master_m3u8();
        assert!(master.starts_with("#EXTM3U\n"));
        assert!(master.contains("#EXT-X-STREAM-INF:"));
        assert!(master.contains("index-v1-a1.m3u8"));

        let segs = vec![RecordingRow {
            id: "seg_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-02/14/front_door/05.10.mp4".to_string(),
            start_time: 1_793_628_310.0, // 2026-11-02T14:05:10.000Z
            end_time: 1_793_628_320.0,
            duration: 10.0,
            motion: 12,
            objects: 1,
            dbfs: -20,
            segment_size: 2.5,
            regions: 1,
            motion_heatmap: None,
        }];

        let media = build_media_m3u8(&segs).unwrap();
        assert!(media.starts_with("#EXTM3U\n"));
        assert!(media.contains("#EXT-X-VERSION:7"));
        assert!(media.contains("#EXT-X-MAP:URI=\"init-v1-a1.mp4\""));
        assert!(media.contains("#EXT-X-PROGRAM-DATE-TIME:2026-11-02T14:05:10.000Z"));
        assert!(media.contains("#EXTINF:10.000,"));
        assert!(media.contains("seg-1-v1-a1.m4s"));
        assert!(media.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn build_init_and_m4s_support_co64_multi_entry_stsc_ctts_and_dual_av_interleaving() {
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");

        // Video samples: 3 samples across 2 chunks (chunk 1 has 2 samples, chunk 2 has 1 sample)
        let v_sample_1 = vec![0x11u8; 32];
        let v_sample_2 = vec![0x22u8; 24];
        let v_sample_3 = vec![0x33u8; 40];
        // Audio samples: 2 samples in 1 chunk
        let a_sample_1 = vec![0xAAu8; 16];
        let a_sample_2 = vec![0xBBu8; 16];

        let mdat_header_end = (ftyp.len() + 8) as u64;
        let v_chunk1_offset = mdat_header_end;
        let a_chunk1_offset = v_chunk1_offset + (v_sample_1.len() + v_sample_2.len()) as u64;
        let v_chunk2_offset = a_chunk1_offset + (a_sample_1.len() + a_sample_2.len()) as u64;

        let mut mdat_payload = Vec::new();
        mdat_payload.extend_from_slice(&v_sample_1);
        mdat_payload.extend_from_slice(&v_sample_2);
        mdat_payload.extend_from_slice(&a_sample_1);
        mdat_payload.extend_from_slice(&a_sample_2);
        mdat_payload.extend_from_slice(&v_sample_3);
        let mdat = make_box(b"mdat", &mdat_payload);

        // Build video trak (track_id = 1, handler = vide/avc1, co64, 2-entry stsc, non-zero ctts)
        let mut v_tkhd_body = vec![0u8; 80];
        v_tkhd_body[8..12].copy_from_slice(&1u32.to_be_bytes());
        v_tkhd_body[72..76].copy_from_slice(&(1920u32 << 16).to_be_bytes());
        v_tkhd_body[76..80].copy_from_slice(&(1080u32 << 16).to_be_bytes());
        let v_tkhd = make_full_box(b"tkhd", 0, 3, &v_tkhd_body);

        let mut v_mdhd_body = vec![0u8; 20];
        v_mdhd_body[8..12].copy_from_slice(&90_000u32.to_be_bytes());
        v_mdhd_body[12..16].copy_from_slice(&9_000u32.to_be_bytes());
        let v_mdhd = make_full_box(b"mdhd", 0, 0, &v_mdhd_body);

        let mut v_hdlr_body = vec![0u8; 20];
        v_hdlr_body[4..8].copy_from_slice(b"vide");
        let v_hdlr = make_full_box(b"hdlr", 0, 0, &v_hdlr_body);

        let mut v_stsd_body = Vec::new();
        v_stsd_body.extend_from_slice(&1u32.to_be_bytes());
        v_stsd_body.extend_from_slice(&make_box(b"avc1", &[0u8; 24]));
        let v_stsd = make_full_box(b"stsd", 0, 0, &v_stsd_body);

        // Variable stsz (fixed_size = 0, sample_count = 3)
        let mut v_stsz_body = Vec::new();
        v_stsz_body.extend_from_slice(&0u32.to_be_bytes());
        v_stsz_body.extend_from_slice(&3u32.to_be_bytes());
        v_stsz_body.extend_from_slice(&(v_sample_1.len() as u32).to_be_bytes());
        v_stsz_body.extend_from_slice(&(v_sample_2.len() as u32).to_be_bytes());
        v_stsz_body.extend_from_slice(&(v_sample_3.len() as u32).to_be_bytes());
        let v_stsz = make_full_box(b"stsz", 0, 0, &v_stsz_body);

        let mut v_stts_body = Vec::new();
        v_stts_body.extend_from_slice(&1u32.to_be_bytes());
        v_stts_body.extend_from_slice(&3u32.to_be_bytes());
        v_stts_body.extend_from_slice(&3000u32.to_be_bytes());
        let v_stts = make_full_box(b"stts", 0, 0, &v_stts_body);

        // Non-zero ctts composition offsets: sample 1 = +3000, samples 2..3 = +1500
        let mut v_ctts_body = Vec::new();
        v_ctts_body.extend_from_slice(&2u32.to_be_bytes()); // 2 entries
        v_ctts_body.extend_from_slice(&1u32.to_be_bytes());
        v_ctts_body.extend_from_slice(&3000i32.to_be_bytes());
        v_ctts_body.extend_from_slice(&2u32.to_be_bytes());
        v_ctts_body.extend_from_slice(&1500i32.to_be_bytes());
        let v_ctts = make_full_box(b"ctts", 0, 0, &v_ctts_body);

        // stss: only sample 1 is sync
        let mut v_stss_body = Vec::new();
        v_stss_body.extend_from_slice(&1u32.to_be_bytes());
        v_stss_body.extend_from_slice(&1u32.to_be_bytes());
        let v_stss = make_full_box(b"stss", 0, 0, &v_stss_body);

        // Multi-entry stsc: chunk 1 has 2 samples, chunk 2 has 1 sample
        let mut v_stsc_body = Vec::new();
        v_stsc_body.extend_from_slice(&2u32.to_be_bytes()); // 2 entries
        v_stsc_body.extend_from_slice(&1u32.to_be_bytes()); // first_chunk = 1
        v_stsc_body.extend_from_slice(&2u32.to_be_bytes()); // samples_per_chunk = 2
        v_stsc_body.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index = 1
        v_stsc_body.extend_from_slice(&2u32.to_be_bytes()); // first_chunk = 2
        v_stsc_body.extend_from_slice(&1u32.to_be_bytes()); // samples_per_chunk = 1
        v_stsc_body.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index = 1
        let v_stsc = make_full_box(b"stsc", 0, 0, &v_stsc_body);

        // co64 (64-bit chunk offsets for the 2 video chunks)
        let mut v_co64_body = Vec::new();
        v_co64_body.extend_from_slice(&2u32.to_be_bytes());
        v_co64_body.extend_from_slice(&v_chunk1_offset.to_be_bytes());
        v_co64_body.extend_from_slice(&v_chunk2_offset.to_be_bytes());
        let v_co64 = make_full_box(b"co64", 0, 0, &v_co64_body);

        let mut v_stbl_children = Vec::new();
        v_stbl_children.extend_from_slice(&v_stsd);
        v_stbl_children.extend_from_slice(&v_stsz);
        v_stbl_children.extend_from_slice(&v_stts);
        v_stbl_children.extend_from_slice(&v_ctts);
        v_stbl_children.extend_from_slice(&v_stss);
        v_stbl_children.extend_from_slice(&v_stsc);
        v_stbl_children.extend_from_slice(&v_co64);
        let v_stbl = make_box(b"stbl", &v_stbl_children);
        let v_minf = make_box(b"minf", &v_stbl);
        let mut v_mdia_children = Vec::new();
        v_mdia_children.extend_from_slice(&v_mdhd);
        v_mdia_children.extend_from_slice(&v_hdlr);
        v_mdia_children.extend_from_slice(&v_minf);
        let v_mdia = make_box(b"mdia", &v_mdia_children);
        let mut v_trak_children = Vec::new();
        v_trak_children.extend_from_slice(&v_tkhd);
        v_trak_children.extend_from_slice(&v_mdia);
        let v_trak = make_box(b"trak", &v_trak_children);

        // Build audio trak (track_id = 2, handler = soun/mp4a, co64)
        let mut a_tkhd_body = vec![0u8; 80];
        a_tkhd_body[8..12].copy_from_slice(&2u32.to_be_bytes());
        let a_tkhd = make_full_box(b"tkhd", 0, 3, &a_tkhd_body);

        let mut a_mdhd_body = vec![0u8; 20];
        a_mdhd_body[8..12].copy_from_slice(&48_000u32.to_be_bytes());
        a_mdhd_body[12..16].copy_from_slice(&2048u32.to_be_bytes());
        let a_mdhd = make_full_box(b"mdhd", 0, 0, &a_mdhd_body);

        let mut a_hdlr_body = vec![0u8; 20];
        a_hdlr_body[4..8].copy_from_slice(b"soun");
        let a_hdlr = make_full_box(b"hdlr", 0, 0, &a_hdlr_body);

        let mut a_stsd_body = Vec::new();
        a_stsd_body.extend_from_slice(&1u32.to_be_bytes());
        a_stsd_body.extend_from_slice(&make_box(b"mp4a", &[0u8; 20]));
        let a_stsd = make_full_box(b"stsd", 0, 0, &a_stsd_body);

        let mut a_stsz_body = Vec::new();
        a_stsz_body.extend_from_slice(&(a_sample_1.len() as u32).to_be_bytes());
        a_stsz_body.extend_from_slice(&2u32.to_be_bytes());
        let a_stsz = make_full_box(b"stsz", 0, 0, &a_stsz_body);

        let mut a_stts_body = Vec::new();
        a_stts_body.extend_from_slice(&1u32.to_be_bytes());
        a_stts_body.extend_from_slice(&2u32.to_be_bytes());
        a_stts_body.extend_from_slice(&1024u32.to_be_bytes());
        let a_stts = make_full_box(b"stts", 0, 0, &a_stts_body);

        let mut a_stsc_body = Vec::new();
        a_stsc_body.extend_from_slice(&1u32.to_be_bytes());
        a_stsc_body.extend_from_slice(&1u32.to_be_bytes());
        a_stsc_body.extend_from_slice(&2u32.to_be_bytes());
        a_stsc_body.extend_from_slice(&1u32.to_be_bytes());
        let a_stsc = make_full_box(b"stsc", 0, 0, &a_stsc_body);

        let mut a_co64_body = Vec::new();
        a_co64_body.extend_from_slice(&1u32.to_be_bytes());
        a_co64_body.extend_from_slice(&a_chunk1_offset.to_be_bytes());
        let a_co64 = make_full_box(b"co64", 0, 0, &a_co64_body);

        let mut a_stbl_children = Vec::new();
        a_stbl_children.extend_from_slice(&a_stsd);
        a_stbl_children.extend_from_slice(&a_stsz);
        a_stbl_children.extend_from_slice(&a_stts);
        a_stbl_children.extend_from_slice(&a_stsc);
        a_stbl_children.extend_from_slice(&a_co64);
        let a_stbl = make_box(b"stbl", &a_stbl_children);
        let a_minf = make_box(b"minf", &a_stbl);
        let mut a_mdia_children = Vec::new();
        a_mdia_children.extend_from_slice(&a_mdhd);
        a_mdia_children.extend_from_slice(&a_hdlr);
        a_mdia_children.extend_from_slice(&a_minf);
        let a_mdia = make_box(b"mdia", &a_mdia_children);
        let mut a_trak_children = Vec::new();
        a_trak_children.extend_from_slice(&a_tkhd);
        a_trak_children.extend_from_slice(&a_mdia);
        let a_trak = make_box(b"trak", &a_trak_children);

        let mvhd = make_full_box(b"mvhd", 0, 0, &[0u8; 96]);
        let mut moov_children = Vec::new();
        moov_children.extend_from_slice(&mvhd);
        moov_children.extend_from_slice(&v_trak);
        moov_children.extend_from_slice(&a_trak);
        let moov = make_box(b"moov", &moov_children);

        let mut mp4_bytes = Vec::new();
        mp4_bytes.extend_from_slice(&ftyp);
        mp4_bytes.extend_from_slice(&mdat);
        mp4_bytes.extend_from_slice(&moov);

        let tmp_path = std::env::temp_dir().join("ninjanvr-vod-co64-ctts-av.mp4");
        std::fs::write(&tmp_path, &mp4_bytes).unwrap();

        // 1. Verify build_init_mp4 contains mvex with 2 trex boxes (track 1 and track 2)
        let init_bytes = build_init_mp4(&tmp_path).expect("build_init_mp4");
        let init_boxes = iter_boxes(&init_bytes);
        let init_moov = find_box(&init_boxes, b"moov").expect("moov in init");
        let init_moov_boxes = iter_boxes(init_moov);
        let mvex = find_box(&init_moov_boxes, b"mvex").expect("mvex in init moov");
        let mvex_boxes = iter_boxes(mvex);
        let trex_boxes: Vec<_> = mvex_boxes
            .iter()
            .filter(|b| &b.box_type == b"trex")
            .collect();
        assert_eq!(trex_boxes.len(), 2, "expected 2 trex boxes in mvex");
        assert_eq!(
            u32::from_be_bytes(trex_boxes[0].payload[4..8].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_be_bytes(trex_boxes[1].payload[4..8].try_into().unwrap()),
            2
        );

        // 2. Verify build_media_segment_m4s trun flags (0x000F01 for vide with ctts, 0x000701 for soun)
        // and verify sample payload integrity in mdat
        let m4s_bytes =
            build_media_segment_m4s(&tmp_path, 3, 20.0).expect("build_media_segment_m4s");
        let m4s_boxes = iter_boxes(&m4s_bytes);
        let moof = find_box(&m4s_boxes, b"moof").expect("moof in m4s");
        let m4s_mdat = find_box(&m4s_boxes, b"mdat").expect("mdat in m4s");

        let moof_boxes = iter_boxes(moof);
        let trafs: Vec<_> = moof_boxes
            .iter()
            .filter(|b| &b.box_type == b"traf")
            .collect();
        assert_eq!(trafs.len(), 2, "expected 2 traf boxes (vide + soun)");

        // Video traf (track 1): trun version 1, flags 0x000F01, sample_count = 3
        let v_traf_boxes = iter_boxes(trafs[0].payload);
        let v_trun = find_box(&v_traf_boxes, b"trun").expect("video trun");
        let v_trun_vf = u32::from_be_bytes(v_trun[0..4].try_into().unwrap());
        assert_eq!(
            v_trun_vf >> 24,
            1,
            "video trun version must be 1 when ctts present"
        );
        assert_eq!(
            v_trun_vf & 0x00FF_FFFF,
            0x000F01,
            "video trun flags must be 0x000F01 when ctts present"
        );
        let v_sample_count = u32::from_be_bytes(v_trun[4..8].try_into().unwrap());
        assert_eq!(v_sample_count, 3);
        let v_flags_1 = u32::from_be_bytes(v_trun[20..24].try_into().unwrap());
        let v_cts_1 = i32::from_be_bytes(v_trun[24..28].try_into().unwrap());
        let v_flags_2 = u32::from_be_bytes(v_trun[36..40].try_into().unwrap());
        let v_cts_2 = i32::from_be_bytes(v_trun[40..44].try_into().unwrap());
        assert_eq!(v_flags_1, 0x0200_0000);
        assert_eq!(v_cts_1, 3000);
        assert_eq!(v_flags_2, 0x0101_0000);
        assert_eq!(v_cts_2, 1500);

        // Audio traf (track 2): trun version 0, flags 0x000701, sample_count = 2
        let a_traf_boxes = iter_boxes(trafs[1].payload);
        let a_trun = find_box(&a_traf_boxes, b"trun").expect("audio trun");
        let a_trun_vf = u32::from_be_bytes(a_trun[0..4].try_into().unwrap());
        assert_eq!(a_trun_vf & 0x00FF_FFFF, 0x000701);
        let a_sample_count = u32::from_be_bytes(a_trun[4..8].try_into().unwrap());
        assert_eq!(a_sample_count, 2);

        // Verify reassembled mdat payload integrity (v_sample_1 + v_sample_2 + v_sample_3 + a_sample_1 + a_sample_2)
        let mut expected_mdat = Vec::new();
        expected_mdat.extend_from_slice(&v_sample_1);
        expected_mdat.extend_from_slice(&v_sample_2);
        expected_mdat.extend_from_slice(&v_sample_3);
        expected_mdat.extend_from_slice(&a_sample_1);
        expected_mdat.extend_from_slice(&a_sample_2);
        assert_eq!(m4s_mdat, expected_mdat.as_slice());

        let _ = std::fs::remove_file(&tmp_path);
    }

    #[test]
    fn format_program_date_time_utc_handles_epoch_leap_year_and_ms_rollover() {
        assert_eq!(
            format_program_date_time_utc(0.0),
            "1970-01-01T00:00:00.000Z"
        );
        // Negative timestamp clamps to 0.0
        assert_eq!(
            format_program_date_time_utc(-50.0),
            "1970-01-01T00:00:00.000Z"
        );
        // 2024 leap year Feb 29 23:59:59.500 UTC (1709251199.5)
        assert_eq!(
            format_program_date_time_utc(1_709_251_199.5),
            "2024-02-29T23:59:59.500Z"
        );
        // Sub-millisecond rounding rollover (.9999 -> next second .000)
        assert_eq!(
            format_program_date_time_utc(1709251199.9999),
            "2024-03-01T00:00:00.000Z"
        );
    }

    #[test]
    fn iter_boxes_parses_size64_eof_boxes_and_stops_on_truncated_headers() {
        // 1. 64-bit extended box (size32 == 1, 16-byte header, total size 20 -> 4-byte payload)
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&20u64.to_be_bytes());
        buf.extend_from_slice(b"ABCD");

        // 2. Box extending to EOF (size32 == 0, 8-byte header -> remaining 3 bytes)
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(b"free");
        buf.extend_from_slice(b"XYZ");

        let boxes = iter_boxes(&buf);
        assert_eq!(boxes.len(), 2);
        assert_eq!(&boxes[0].box_type, b"mdat");
        assert_eq!(boxes[0].payload, b"ABCD");
        assert_eq!(&boxes[1].box_type, b"free");
        assert_eq!(boxes[1].payload, b"XYZ");

        // 3. Invalid header with size32 < 8 (e.g. 4) or truncated size64 stops cleanly without panic
        let mut malformed = Vec::new();
        malformed.extend_from_slice(&4u32.to_be_bytes());
        malformed.extend_from_slice(b"bad!");
        assert!(iter_boxes(&malformed).is_empty());

        let mut truncated_64 = Vec::new();
        truncated_64.extend_from_slice(&1u32.to_be_bytes());
        truncated_64.extend_from_slice(b"mdat");
        truncated_64.extend_from_slice(&[0u8; 4]); // fewer than 16 bytes
        assert!(iter_boxes(&truncated_64).is_empty());
    }

    #[test]
    fn build_media_playlist_handles_empty_segments_and_time_gap_without_map_duplication() {
        let empty_m3u8 = build_media_playlist_with_resolutions(&[], None).unwrap();
        assert!(empty_m3u8.contains("#EXT-X-TARGETDURATION:10"));
        assert!(empty_m3u8.contains("#EXT-X-MAP:URI=\"init-v1-a1.mp4\""));
        assert!(empty_m3u8.contains("#EXT-X-ENDLIST"));
        assert!(!empty_m3u8.contains("#EXT-X-DISCONTINUITY"));

        // Two segments with identical resolution and bitrate but a 10s time gap
        let segs = vec![
            RecordingRow {
                id: "s1".to_string(),
                camera: "cam".to_string(),
                path: "/1.mp4".to_string(),
                start_time: 100.0,
                end_time: 110.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 2.0,
                regions: 0,
                motion_heatmap: None,
            },
            RecordingRow {
                id: "s2".to_string(),
                camera: "cam".to_string(),
                path: "/2.mp4".to_string(),
                start_time: 120.0,
                end_time: 130.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 2.0,
                regions: 0,
                motion_heatmap: None,
            },
        ];
        let res = [(1920, 1080), (1920, 1080)];
        let m3u8 = build_media_playlist_with_resolutions(&segs, Some(&res)).unwrap();
        assert_eq!(m3u8.matches("#EXT-X-DISCONTINUITY").count(), 1);
        // Only the initial #EXT-X-MAP should be present since neither resolution nor tier changed
        assert_eq!(m3u8.matches("#EXT-X-MAP:").count(), 1);

        // Contiguous segments with identical known resolution (1920x1080) but a >3.2x VBR bitrate spike
        // must NOT emit a false tier_switched discontinuity
        let vbr_segs = vec![
            RecordingRow {
                id: "v1".to_string(),
                camera: "cam".to_string(),
                path: "/v1.mp4".to_string(),
                start_time: 200.0,
                end_time: 210.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 1.0,
                regions: 0,
                motion_heatmap: None,
            },
            RecordingRow {
                id: "v2".to_string(),
                camera: "cam".to_string(),
                path: "/v2.mp4".to_string(),
                start_time: 210.0,
                end_time: 220.0,
                duration: 10.0,
                motion: 500,
                objects: 1,
                dbfs: 0,
                segment_size: 5.0,
                regions: 1,
                motion_heatmap: None,
            },
        ];
        let vbr_m3u8 = build_media_playlist_with_resolutions(&vbr_segs, Some(&res)).unwrap();
        assert_eq!(vbr_m3u8.matches("#EXT-X-DISCONTINUITY").count(), 0);
        assert_eq!(vbr_m3u8.matches("#EXT-X-MAP:").count(), 1);
    }

    #[test]
    fn fmp4_classifier_distinguishes_init_segment_sync_keyframe_and_non_sync_fragments() {
        // 1. Synthesize init-v1-a1.mp4 (ftyp + moov)
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41");
        let mvhd = make_full_box(b"mvhd", 0, 0, &[0u8; 96]);
        let moov = make_box(b"moov", &mvhd);
        let mut init_seg = Vec::new();
        init_seg.extend_from_slice(&ftyp);
        init_seg.extend_from_slice(&moov);

        assert!(is_fmp4_init_segment(&init_seg));
        assert!(!is_fmp4_sync_fragment(&init_seg));

        // Helper to build moof+mdat fragment with sample-flags-present (0x000701)
        let build_fragment = |track_id: u32, sample_flags: u32| -> Vec<u8> {
            let mfhd = make_full_box(b"mfhd", 0, 0, &1u32.to_be_bytes());
            let tfhd = make_full_box(b"tfhd", 0, 0x020000, &track_id.to_be_bytes());
            let mut trun_body = Vec::new();
            trun_body.extend_from_slice(&1u32.to_be_bytes()); // sample_count = 1
            trun_body.extend_from_slice(&64i32.to_be_bytes()); // data_offset
            trun_body.extend_from_slice(&3000u32.to_be_bytes()); // sample_duration
            trun_body.extend_from_slice(&128u32.to_be_bytes()); // sample_size
            trun_body.extend_from_slice(&sample_flags.to_be_bytes()); // sample_flags
            let trun = make_full_box(b"trun", 0, 0x000701, &trun_body);
            let mut traf_children = Vec::new();
            traf_children.extend_from_slice(&tfhd);
            traf_children.extend_from_slice(&trun);
            let traf = make_box(b"traf", &traf_children);
            let mut moof_children = Vec::new();
            moof_children.extend_from_slice(&mfhd);
            moof_children.extend_from_slice(&traf);
            let moof_box = make_box(b"moof", &moof_children);
            let mdat_box = make_box(b"mdat", &[0x65u8; 32]);
            let mut frag = Vec::new();
            frag.extend_from_slice(&moof_box);
            frag.extend_from_slice(&mdat_box);
            frag
        };

        // 2. Keyframe moof+mdat (flags = 0x0200_0000 -> sample_depends_on == 2)
        let keyframe_frag = build_fragment(1, 0x0200_0000);
        assert!(!is_fmp4_init_segment(&keyframe_frag));
        assert!(is_fmp4_sync_fragment(&keyframe_frag));

        // 3. Non-sync delta moof+mdat (flags = 0x0101_0000 -> sample_depends_on == 1, is_non_sync == 1)
        let delta_frag = build_fragment(1, 0x0101_0000);
        assert!(!is_fmp4_init_segment(&delta_frag));
        assert!(!is_fmp4_sync_fragment(&delta_frag));

        // 3b. Standalone audio moof+mdat (track_id = 2, flags = 0x0200_0000) must NOT be classified as a video sync keyframe
        let audio_frag = build_fragment(2, 0x0200_0000);
        assert!(!is_fmp4_init_segment(&audio_frag));
        assert!(!is_fmp4_sync_fragment(&audio_frag));

        // 4. Test first-sample-flags-present (0x000004) & tfhd default-sample-flags-present (0x000020)
        let mut trun_first_flag_body = Vec::new();
        trun_first_flag_body.extend_from_slice(&1u32.to_be_bytes()); // sample_count
        trun_first_flag_body.extend_from_slice(&0x0200_0000u32.to_be_bytes()); // first_sample_flags (sync)
        let trun_first_sync = make_full_box(b"trun", 0, 0x000004, &trun_first_flag_body);
        assert_eq!(
            parse_trun_first_sample_is_sync(&trun_first_sync[8..], None),
            Some(true)
        );

        let trun_default_only = make_full_box(b"trun", 0, 0x000000, &1u32.to_be_bytes());
        assert_eq!(
            parse_trun_first_sample_is_sync(&trun_default_only[8..], Some(0x0101_0000)),
            Some(false)
        );
        assert_eq!(
            parse_trun_first_sample_is_sync(&trun_default_only[8..], Some(0x0000_0000)),
            Some(true)
        );
    }
}
