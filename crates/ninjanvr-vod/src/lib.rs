use anyhow::Result;
use ninjanvr_db::RecordingRow;

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

    for (idx, seg) in segments.iter().enumerate() {
        let dur = seg.duration.max(0.1);
        lines.push(format!("#EXTINF:{dur:.3},"));
        lines.push(format!("seg-{}-v1-a1.m4s", idx + 1));
    }

    lines.push("#EXT-X-ENDLIST".to_string());
    lines.push(String::new());
    Ok(lines.join("\n"))
}
