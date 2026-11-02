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
            let tkhd = find_box(&trak_boxes, b"tkhd")?;
            let mdia = find_box(&trak_boxes, b"mdia")?;
            let mdia_boxes = iter_boxes(mdia);
            let hdlr = find_box(&mdia_boxes, b"hdlr")?;
            if hdlr.len() >= 12 && &hdlr[8..12] == b"vide" && tkhd.len() >= 84 {
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

/// Generates a Frigate-compatible HLS master playlist (`master.m3u8`).
pub fn build_master_playlist() -> String {
    [
        "#EXTM3U",
        "#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1500000,RESOLUTION=1920x1080,FRAME-RATE=20.000,CODECS=\"avc1.640028,mp4a.40.2\",VIDEO-RANGE=SDR",
        "index-v1-a1.m3u8",
        "",
    ]
    .join("\n")
}

/// Generates a VOD media playlist (`index-v1-a1.m3u8`) from a slice of recording segments.
pub fn build_media_playlist(segments: &[RecordingRow]) -> Result<String> {
    build_media_playlist_with_resolutions(segments, None)
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
        "#EXT-X-VERSION:6".to_string(),
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
        let tier_switched = match prev_rate {
            Some(pr) if pr > 0.001 && cur_rate > 0.001 => {
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
}
