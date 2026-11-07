use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Notify, RwLock, broadcast};
use tokio_tungstenite::tungstenite::Message as TungMsg;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

pub const MAX_GOP_FRAGMENTS: usize = 90;
pub const MAX_GOP_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_INIT_SEGMENT_BYTES: usize = 512 * 1024;
pub const MAX_CODEC_MSG_BYTES: usize = 4096;
pub const MAX_CACHED_STREAMS: usize = 64;
pub const MAX_FMP4_BOXES_PER_LEVEL: usize = 256;
pub const MAX_GOP_CACHE_AGE: Duration = Duration::from_secs(10);
pub const PREWARM_KEEPALIVE_TTL: Duration = Duration::from_secs(45);
pub const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const UPSTREAM_SEND_TIMEOUT: Duration = Duration::from_secs(3);

/// Validates that a `go2rtc` stream identifier is 1..=128 ASCII `[A-Za-z0-9_-]` characters
/// and does not start with `.` so stream names cannot inject query parameters, fragments,
/// or path traversal sequences into upstream WebSocket URLs.
pub fn is_valid_stream_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn parse_exact_iso_boxes(mut buf: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    if buf.len() < 8 {
        return None;
    }
    let mut out = Vec::new();
    while !buf.is_empty() {
        if buf.len() < 8 || out.len() >= MAX_FMP4_BOXES_PER_LEVEL {
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

/// Validates that `buf` is a structurally complete ISO-BMFF fMP4 initialization segment
/// containing both `ftyp` and non-empty `moov` boxes, no `moof` box, and zero truncated/trailing bytes.
pub fn is_complete_fmp4_init_segment(buf: &[u8]) -> bool {
    if buf.is_empty() || buf.len() > MAX_INIT_SEGMENT_BYTES {
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
    has_ftyp && has_moov && !has_moof && ninjanvr_vod::is_fmp4_init_segment(buf)
}

/// Validates that `buf` is a structurally complete ISO-BMFF fMP4 media fragment (`moof` + `mdat`)
/// with exact box boundaries and at least one valid `traf` (`tfhd` + `trun`).
pub fn is_complete_fmp4_media_fragment(buf: &[u8]) -> bool {
    if buf.is_empty() || buf.len() > MAX_GOP_BYTES {
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

#[derive(Debug, Clone)]
pub struct MseGopBurst {
    pub codec_msg: String,
    pub init_segment: Bytes,
    pub gop_fragments: Vec<Bytes>,
    pub last_seq: u64,
}

#[derive(Debug, Clone)]
pub enum MseLivePacket {
    Codec(String),
    Init(Bytes),
    Fragment { seq: u64, bytes: Bytes },
}

#[derive(Debug, Clone)]
pub struct MseGopCache {
    pub codec_msg: Option<String>,
    pub init_segment: Option<Bytes>,
    pub gop_fragments: VecDeque<Bytes>,
    pub gop_bytes: usize,
    pub updated_at: Instant,
    pub last_seq: u64,
}

impl Default for MseGopCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MseGopCache {
    pub fn new() -> Self {
        Self {
            codec_msg: None,
            init_segment: None,
            gop_fragments: VecDeque::new(),
            gop_bytes: 0,
            updated_at: Instant::now(),
            last_seq: 0,
        }
    }

    pub fn ingest_text(&mut self, text: &str) -> bool {
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_CODEC_MSG_BYTES {
            return false;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return false;
        };
        if parsed.get("type").and_then(|v| v.as_str()) != Some("mse") {
            return false;
        }
        let Some(codec_val) = parsed.get("value").and_then(|v| v.as_str()) else {
            return false;
        };
        if codec_val.trim().is_empty() {
            return false;
        }
        if self
            .codec_msg
            .as_deref()
            .is_some_and(|prev| prev != trimmed)
        {
            self.init_segment = None;
            self.gop_fragments.clear();
            self.gop_bytes = 0;
        }
        self.codec_msg = Some(trimmed.to_string());
        self.updated_at = Instant::now();
        true
    }

    pub fn ingest_binary(&mut self, bytes: Bytes) -> bool {
        self.ingest_binary_with_seq(bytes).0
    }

    pub fn ingest_binary_with_seq(&mut self, bytes: Bytes) -> (bool, Option<MseLivePacket>) {
        if bytes.is_empty() || bytes.len() > MAX_GOP_BYTES {
            return (false, None);
        }
        if is_complete_fmp4_init_segment(&bytes) {
            self.init_segment = Some(bytes.clone());
            self.gop_fragments.clear();
            self.gop_bytes = 0;
            self.updated_at = Instant::now();
            return (false, Some(MseLivePacket::Init(bytes)));
        }
        if !is_complete_fmp4_media_fragment(&bytes) {
            return (false, None);
        }
        self.last_seq = self.last_seq.wrapping_add(1);
        let seq = self.last_seq;
        if ninjanvr_vod::is_fmp4_sync_fragment(&bytes) {
            self.gop_fragments.clear();
            self.gop_bytes = bytes.len();
            self.gop_fragments.push_back(bytes.clone());
            self.updated_at = Instant::now();
            return (true, Some(MseLivePacket::Fragment { seq, bytes }));
        }
        // Delta (P/B or audio) fragment: append if we already have an IDR keyframe anchor in gop_fragments
        // and adding this fragment stays within MAX_GOP_FRAGMENTS and MAX_GOP_BYTES.
        if !self.gop_fragments.is_empty()
            && self.gop_fragments.len() < MAX_GOP_FRAGMENTS
            && self.gop_bytes.saturating_add(bytes.len()) <= MAX_GOP_BYTES
        {
            self.gop_bytes += bytes.len();
            self.gop_fragments.push_back(bytes.clone());
            self.updated_at = Instant::now();
        }
        (false, Some(MseLivePacket::Fragment { seq, bytes }))
    }

    pub fn has_ready_burst(&self) -> bool {
        self.codec_msg.is_some() && self.init_segment.is_some() && !self.gop_fragments.is_empty()
    }

    pub fn snapshot_fresh_burst(&self, max_age: Duration) -> Option<MseGopBurst> {
        if self.updated_at.elapsed() > max_age {
            return None;
        }
        let codec_msg = self.codec_msg.clone()?;
        let init_segment = self.init_segment.clone()?;
        if self.gop_fragments.is_empty() {
            return None;
        }
        Some(MseGopBurst {
            codec_msg,
            init_segment,
            gop_fragments: self.gop_fragments.iter().cloned().collect(),
            last_seq: self.last_seq,
        })
    }
}

struct UpstreamSession {
    expires_at: Instant,
    persistent: bool,
    active_clients: Arc<AtomicUsize>,
    ready_notify: Arc<Notify>,
    tx: broadcast::Sender<MseLivePacket>,
}

pub struct MseClientSubscription {
    pub burst: Option<MseGopBurst>,
    pub rx: broadcast::Receiver<MseLivePacket>,
    pub last_seq: u64,
    pub needs_resync: bool,
    active_clients: Arc<AtomicUsize>,
}

impl MseClientSubscription {
    /// Receives the next live packet or, if the subscriber lagged behind the broadcast ring,
    /// refreshes `self.burst` from the hub's current GOP cache so the caller can resync cleanly
    /// at an IDR keyframe boundary instead of feeding broken delta frames.
    pub async fn recv_or_resync(
        &mut self,
        hub: &MseStreamHub,
        stream: &str,
    ) -> Result<MseLivePacket, Option<MseGopBurst>> {
        if self.needs_resync {
            let (fresh, fresh_rx) = hub.resync_after_lag(stream).await;
            if let Some(rx) = fresh_rx {
                self.rx = rx;
            } else {
                while self.rx.try_recv().is_ok() {}
            }
            if let Some(ref b) = fresh {
                self.last_seq = self.last_seq.max(b.last_seq);
            }
            self.burst = fresh.clone();
            self.needs_resync = false;
            return Err(fresh);
        }
        loop {
            match self.rx.recv().await {
                Ok(MseLivePacket::Fragment { seq, bytes }) => {
                    if seq <= self.last_seq {
                        continue;
                    }
                    self.last_seq = seq;
                    return Ok(MseLivePacket::Fragment { seq, bytes });
                }
                Ok(pkt) => return Ok(pkt),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.needs_resync = true;
                    let (fresh, fresh_rx) = hub.resync_after_lag(stream).await;
                    if let Some(rx) = fresh_rx {
                        self.rx = rx;
                    } else {
                        while self.rx.try_recv().is_ok() {}
                    }
                    if let Some(ref b) = fresh {
                        self.last_seq = self.last_seq.max(b.last_seq);
                    }
                    self.burst = fresh.clone();
                    self.needs_resync = false;
                    return Err(fresh);
                }
                Err(broadcast::error::RecvError::Closed) => return Err(None),
            }
        }
    }
}

impl Drop for MseClientSubscription {
    fn drop(&mut self) {
        self.active_clients.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
pub struct MseStreamHub {
    caches: Arc<RwLock<HashMap<String, MseGopCache>>>,
    sessions: Arc<RwLock<HashMap<String, UpstreamSession>>>,
}

impl MseStreamHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune_stale_caches_locked(caches: &mut HashMap<String, MseGopCache>, incoming_stream: &str) {
        if caches.len() < MAX_CACHED_STREAMS || caches.contains_key(incoming_stream) {
            return;
        }
        caches.retain(|_, c| c.updated_at.elapsed() <= MAX_GOP_CACHE_AGE);
        while caches.len() >= MAX_CACHED_STREAMS {
            let Some(oldest_key) = caches
                .iter()
                .min_by_key(|(_, c)| c.updated_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            caches.remove(&oldest_key);
        }
    }

    pub async fn ingest_upstream_text(&self, stream: &str, text: &str) {
        let stream_key = stream.trim();
        if !is_valid_stream_name(stream_key) {
            return;
        }
        let mut guard = self.caches.write().await;
        Self::prune_stale_caches_locked(&mut guard, stream_key);
        let updated = guard
            .entry(stream_key.to_string())
            .or_default()
            .ingest_text(text);
        if updated && let Some(sess) = self.sessions.read().await.get(stream_key) {
            let _ = sess.tx.send(MseLivePacket::Codec(text.trim().to_string()));
        }
    }

    pub async fn ingest_upstream_binary(&self, stream: &str, bytes: Bytes) -> bool {
        let stream_key = stream.trim();
        if !is_valid_stream_name(stream_key) {
            return false;
        }
        let mut guard = self.caches.write().await;
        Self::prune_stale_caches_locked(&mut guard, stream_key);
        let cache = guard.entry(stream_key.to_string()).or_default();
        let (is_sync, pkt_opt) = cache.ingest_binary_with_seq(bytes);
        let burst_ready = cache.has_ready_burst();
        if let Some(sess) = self.sessions.read().await.get(stream_key) {
            if let Some(pkt) = pkt_opt {
                let _ = sess.tx.send(pkt);
            }
            if is_sync && burst_ready {
                sess.ready_notify.notify_waiters();
            }
        }
        is_sync
    }

    pub async fn get_fresh_burst(&self, stream: &str) -> Option<MseGopBurst> {
        let stream_key = stream.trim();
        if !is_valid_stream_name(stream_key) {
            return None;
        }
        let guard = self.caches.read().await;
        guard
            .get(stream_key)
            .and_then(|c| c.snapshot_fresh_burst(MAX_GOP_CACHE_AGE))
    }

    async fn resync_after_lag(
        &self,
        stream: &str,
    ) -> (
        Option<MseGopBurst>,
        Option<broadcast::Receiver<MseLivePacket>>,
    ) {
        let stream_key = stream.trim();
        if !is_valid_stream_name(stream_key) {
            return (None, None);
        }
        let caches = self.caches.read().await;
        let sessions = self.sessions.read().await;
        let burst = caches
            .get(stream_key)
            .and_then(|c| c.snapshot_fresh_burst(MAX_GOP_CACHE_AGE));
        let rx = sessions.get(stream_key).map(|s| s.tx.subscribe());
        (burst, rx)
    }

    async fn ensure_upstream_session(
        &self,
        go2rtc_ws_base: &str,
        stream_key: &str,
        persistent: bool,
    ) -> Option<(
        Arc<Notify>,
        broadcast::Receiver<MseLivePacket>,
        Arc<AtomicUsize>,
    )> {
        let now = Instant::now();
        let new_expires = now + PREWARM_KEEPALIVE_TTL;

        let mut sessions = self.sessions.write().await;
        if let Some(sess) = sessions.get_mut(stream_key) {
            sess.expires_at = new_expires;
            if persistent {
                sess.persistent = true;
            }
            return Some((
                sess.ready_notify.clone(),
                sess.tx.subscribe(),
                sess.active_clients.clone(),
            ));
        }

        if sessions.len() >= MAX_CACHED_STREAMS {
            sessions.retain(|_, s| {
                s.persistent || s.active_clients.load(Ordering::SeqCst) > 0 || now < s.expires_at
            });
            while sessions.len() >= MAX_CACHED_STREAMS {
                let Some(evict_key) = sessions
                    .iter()
                    .filter(|(_, s)| !s.persistent && s.active_clients.load(Ordering::SeqCst) == 0)
                    .min_by_key(|(_, s)| s.expires_at)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                sessions.remove(&evict_key);
            }
            if !persistent && sessions.len() >= MAX_CACHED_STREAMS {
                return None;
            }
        }

        let notify = Arc::new(Notify::new());
        let (tx, rx) = broadcast::channel(256);
        let active_clients = Arc::new(AtomicUsize::new(0));
        sessions.insert(
            stream_key.to_string(),
            UpstreamSession {
                expires_at: new_expires,
                persistent,
                active_clients: active_clients.clone(),
                ready_notify: notify.clone(),
                tx,
            },
        );
        drop(sessions);

        let hub = self.clone();
        let ws_base = go2rtc_ws_base.to_string();
        let stream_owned = stream_key.to_string();
        let notify_bg = notify.clone();
        tokio::spawn(async move {
            run_upstream_worker(hub, ws_base, stream_owned, notify_bg).await;
        });

        Some((notify, rx, active_clients))
    }

    /// Spawns or refreshes a shared background upstream `go2rtc` MSE consumer for `stream`
    /// that keeps its `MseGopCache` warm for `PREWARM_KEEPALIVE_TTL` (45s).
    pub async fn prewarm_stream(&self, go2rtc_ws_base: &str, stream: &str) -> bool {
        let stream_key = stream.trim();
        if !is_valid_stream_name(stream_key) {
            return false;
        }

        let Some((notify, _rx, _clients)) = self
            .ensure_upstream_session(go2rtc_ws_base, stream_key, false)
            .await
        else {
            return false;
        };

        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if self.get_fresh_burst(stream_key).await.is_some() {
            return true;
        }

        let _ = tokio::time::timeout(Duration::from_millis(250), notified).await;
        self.get_fresh_burst(stream_key).await.is_some()
    }

    /// Starts persistent background MSE GOP warmers for all configured cameras (`{cam}` and `{cam}_sub`)
    /// so switching to any camera or layout in the console delivers an instant keyframe burst (< 5ms).
    pub async fn start_persistent_camera_streams(
        &self,
        go2rtc_ws_base: &str,
        cameras: Vec<String>,
    ) {
        let mut streams = Vec::with_capacity(cameras.len() * 2);
        for cam in cameras {
            let trimmed = cam.trim();
            if !is_valid_stream_name(trimmed) {
                continue;
            }
            streams.push(trimmed.to_string());
            let sub = format!("{trimmed}_sub");
            if is_valid_stream_name(&sub) {
                streams.push(sub);
            }
        }
        self.start_persistent_streams(go2rtc_ws_base, streams).await;
    }

    /// Starts persistent background MSE GOP warmers for the exact deduplicated `streams` list.
    pub async fn start_persistent_streams(&self, go2rtc_ws_base: &str, streams: Vec<String>) {
        for stream in streams {
            let trimmed = stream.trim();
            if !is_valid_stream_name(trimmed) {
                continue;
            }
            let _ = self
                .ensure_upstream_session(go2rtc_ws_base, trimmed, true)
                .await;
        }
    }

    /// Subscribes a client WebSocket to the shared upstream `go2rtc` session for `stream`,
    /// returning an atomic `(Option<MseGopBurst>, broadcast::Receiver<MseLivePacket>)` pair
    /// with continuous `tfdt` timestamps and zero duplicate or dropped fragments.
    pub async fn subscribe_client(
        &self,
        go2rtc_ws_base: &str,
        stream: &str,
    ) -> Option<MseClientSubscription> {
        let stream_key = stream.trim();
        if !is_valid_stream_name(stream_key) {
            return None;
        }

        let (notify, initial_rx, active_clients) = self
            .ensure_upstream_session(go2rtc_ws_base, stream_key, false)
            .await?;
        active_clients.fetch_add(1, Ordering::SeqCst);
        let mut sub = MseClientSubscription {
            burst: None,
            rx: initial_rx,
            last_seq: 0,
            needs_resync: false,
            active_clients,
        };

        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if self.get_fresh_burst(stream_key).await.is_none() {
            let _ = tokio::time::timeout(Duration::from_millis(120), notified).await;
        }

        // Subscribe and snapshot under both read locks so `last_seq` aligns atomically with `rx`
        {
            let caches = self.caches.read().await;
            let sessions = self.sessions.read().await;
            if let Some(fresh_rx) = sessions.get(stream_key).map(|s| s.tx.subscribe()) {
                sub.rx = fresh_rx;
            }
            sub.burst = caches
                .get(stream_key)
                .and_then(|c| c.snapshot_fresh_burst(MAX_GOP_CACHE_AGE));
            sub.last_seq = sub.burst.as_ref().map(|b| b.last_seq).unwrap_or(0);
        }

        Some(sub)
    }
}

async fn run_upstream_worker(
    hub: MseStreamHub,
    go2rtc_ws_base: String,
    stream: String,
    notify: Arc<Notify>,
) {
    if !is_valid_stream_name(&stream) {
        notify.notify_waiters();
        return;
    }
    let ws_url = format!(
        "{}/api/ws?src={}",
        go2rtc_ws_base.trim_end_matches('/'),
        stream
    );
    let ws_cfg = WebSocketConfig::default()
        .max_message_size(Some(MAX_GOP_BYTES))
        .max_frame_size(Some(MAX_GOP_BYTES));

    let mut failed_cycles = 0u32;
    loop {
        let should_run = {
            let mut sessions = hub.sessions.write().await;
            let active = sessions
                .get(&stream)
                .map(|s| {
                    s.persistent
                        || s.active_clients.load(Ordering::SeqCst) > 0
                        || Instant::now() < s.expires_at
                })
                .unwrap_or(false);
            if !active {
                sessions.remove(&stream);
            }
            active
        };
        if !should_run {
            break;
        }

        let mut received_media = false;
        if let Ok(Ok((ws_stream, _))) = tokio::time::timeout(
            UPSTREAM_CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(&ws_url, Some(ws_cfg), false),
        )
        .await
        {
            let (mut sink, mut stream_rx) = ws_stream.split();
            let handshake = r#"{"type":"mse","value":"avc1.640029,avc1.64002A,avc1.640028,avc1.4D401F,avc1.4D0029,avc1.42E01E,hev1.1.6.L153.B0,mp4a.40.2,mp4a.40.5"}"#;
            if tokio::time::timeout(
                UPSTREAM_SEND_TIMEOUT,
                sink.send(TungMsg::Text(handshake.into())),
            )
            .await
            .is_ok_and(|r| r.is_ok())
            {
                let mut consecutive_timeouts = 0u8;
                loop {
                    let keep_alive = {
                        let mut sessions = hub.sessions.write().await;
                        if let Some(s) = sessions.get_mut(&stream) {
                            if s.active_clients.load(Ordering::SeqCst) > 0 {
                                s.expires_at = Instant::now() + PREWARM_KEEPALIVE_TTL;
                                true
                            } else {
                                s.persistent || Instant::now() < s.expires_at
                            }
                        } else {
                            false
                        }
                    };
                    if !keep_alive {
                        let _ =
                            tokio::time::timeout(Duration::from_millis(200), sink.close()).await;
                        break;
                    }

                    match tokio::time::timeout(Duration::from_secs(3), stream_rx.next()).await {
                        Ok(Some(Ok(TungMsg::Text(t)))) => {
                            consecutive_timeouts = 0;
                            hub.ingest_upstream_text(&stream, t.as_str()).await;
                        }
                        Ok(Some(Ok(TungMsg::Binary(b)))) => {
                            consecutive_timeouts = 0;
                            received_media = true;
                            failed_cycles = 0;
                            hub.ingest_upstream_binary(&stream, b).await;
                        }
                        Ok(Some(Ok(TungMsg::Ping(p)))) => {
                            consecutive_timeouts = 0;
                            let _ = tokio::time::timeout(
                                UPSTREAM_SEND_TIMEOUT,
                                sink.send(TungMsg::Pong(p)),
                            )
                            .await;
                        }
                        Ok(Some(Ok(TungMsg::Pong(_)))) => {
                            consecutive_timeouts = 0;
                        }
                        Ok(Some(Ok(TungMsg::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break,
                        Err(_) => {
                            consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                            if consecutive_timeouts >= 4 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let (should_retry, has_active_clients) = {
            let mut sessions = hub.sessions.write().await;
            let state = sessions.get(&stream).map(|s| {
                let clients = s.active_clients.load(Ordering::SeqCst) > 0;
                let active = s.persistent || clients || Instant::now() < s.expires_at;
                (active, clients)
            });
            let (active, clients) = state.unwrap_or((false, false));
            if !active {
                sessions.remove(&stream);
            }
            (active, clients)
        };
        if !should_retry {
            break;
        }
        let backoff_ms = if received_media || failed_cycles == 0 {
            failed_cycles = failed_cycles.saturating_add(1);
            500
        } else {
            failed_cycles = failed_cycles.saturating_add(1);
            if has_active_clients {
                match failed_cycles {
                    1 => 500,
                    2 => 1_500,
                    3 => 3_000,
                    _ => 5_000,
                }
            } else {
                match failed_cycles {
                    1 => 1_000,
                    2 => 3_000,
                    3 => 6_000,
                    _ => 10_000,
                }
            }
        };
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    }

    {
        let mut caches = hub.caches.write().await;
        if let Some(c) = caches.get(&stream)
            && c.updated_at.elapsed() > MAX_GOP_CACHE_AGE
        {
            caches.remove(&stream);
        }
    }
    notify.notify_waiters();
}

pub fn extract_stream_src_from_query(query: &str) -> Option<String> {
    let mut matched_src: Option<String> = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == "src"
        {
            let trimmed = v.trim();
            if !is_valid_stream_name(trimmed) || matched_src.is_some() {
                return None;
            }
            matched_src = Some(trimmed.to_string());
        }
    }
    matched_src
}

#[cfg(test)]
mod tests {
    use super::*;

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

    pub fn build_test_init_segment() -> Bytes {
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41");
        let moov = make_box(b"moov", &[0u8; 32]);
        let mut out = Vec::new();
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&moov);
        Bytes::from(out)
    }

    pub fn build_test_fragment(is_sync: bool, mdat_payload: &[u8]) -> Bytes {
        build_test_track_fragment(1, is_sync, mdat_payload)
    }

    pub fn build_test_track_fragment(track_id: u32, is_sync: bool, mdat_payload: &[u8]) -> Bytes {
        let sample_flags: u32 = if is_sync { 0x0200_0000 } else { 0x0101_0000 };
        let mfhd = make_full_box(b"mfhd", 0, 0, &1u32.to_be_bytes());
        let tfhd = make_full_box(b"tfhd", 0, 0x020000, &track_id.to_be_bytes());
        let mut trun_body = Vec::new();
        trun_body.extend_from_slice(&1u32.to_be_bytes());
        trun_body.extend_from_slice(&64i32.to_be_bytes());
        trun_body.extend_from_slice(&3000u32.to_be_bytes());
        trun_body.extend_from_slice(&(mdat_payload.len() as u32).to_be_bytes());
        trun_body.extend_from_slice(&sample_flags.to_be_bytes());
        let trun = make_full_box(b"trun", 0, 0x000701, &trun_body);
        let mut traf_children = Vec::new();
        traf_children.extend_from_slice(&tfhd);
        traf_children.extend_from_slice(&trun);
        let traf = make_box(b"traf", &traf_children);
        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&mfhd);
        moof_children.extend_from_slice(&traf);
        let moof = make_box(b"moof", &moof_children);
        let mdat = make_box(b"mdat", mdat_payload);
        let mut out = Vec::new();
        out.extend_from_slice(&moof);
        out.extend_from_slice(&mdat);
        Bytes::from(out)
    }

    #[test]
    fn mse_gop_cache_evicts_previous_gop_on_new_keyframe_and_enforces_byte_cap() {
        let mut cache = MseGopCache::new();
        assert!(cache.ingest_text(r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#));

        let init = build_test_init_segment();
        cache.ingest_binary(init.clone());
        assert_eq!(cache.init_segment, Some(init.clone()));
        assert!(cache.gop_fragments.is_empty());

        // GOP #1: I + Audio(track 2) + P1 + P2
        let gop1_i = build_test_fragment(true, &[0x11; 64]);
        let gop1_audio = build_test_track_fragment(2, true, &[0xAA; 24]);
        let gop1_p1 = build_test_fragment(false, &[0x12; 32]);
        let gop1_p2 = build_test_fragment(false, &[0x13; 32]);
        assert!(cache.ingest_binary(gop1_i));
        // Audio track_id=2 with sync flags must NOT evict the video keyframe!
        assert!(!cache.ingest_binary(gop1_audio));
        assert!(!cache.ingest_binary(gop1_p1));
        assert!(!cache.ingest_binary(gop1_p2));
        assert_eq!(cache.gop_fragments.len(), 4);

        // GOP #2: I + P1 -> must evict GOP #1 and retain init_segment + GOP #2 only
        let gop2_i = build_test_fragment(true, &[0x21; 80]);
        let gop2_p1 = build_test_fragment(false, &[0x22; 40]);
        assert!(cache.ingest_binary(gop2_i.clone()));
        assert!(!cache.ingest_binary(gop2_p1.clone()));
        assert_eq!(cache.init_segment, Some(init));
        assert_eq!(cache.gop_fragments.len(), 2);
        assert_eq!(cache.gop_fragments[0], gop2_i);
        assert_eq!(cache.gop_fragments[1], gop2_p1);

        // Enforce MAX_GOP_BYTES cap on oversized delta fragment
        let huge_delta = build_test_fragment(false, &vec![0x99u8; MAX_GOP_BYTES]);
        cache.ingest_binary(huge_delta);
        assert_eq!(cache.gop_fragments.len(), 2);
        assert!(cache.gop_bytes <= MAX_GOP_BYTES);
    }

    #[tokio::test]
    async fn mse_stream_hub_retries_upstream_when_go2rtc_disconnects_or_starts_late() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ws_base = format!("ws://{addr}");
        let hub = MseStreamHub::new();

        // Spawn upstream mock go2rtc server that drops the first connection and streams on the second
        tokio::spawn(async move {
            // 1st connection: accept and close immediately (simulating go2rtc restart / pod boot race)
            if let Ok((tcp1, _)) = listener.accept().await
                && let Ok(mut ws1) = tokio_tungstenite::accept_async(tcp1).await
            {
                let _ = ws1.close(None).await;
            }
            // 2nd connection: accept and send MSE codec + init + IDR sync fragment
            if let Ok((tcp2, _)) = listener.accept().await
                && let Ok(mut ws2) = tokio_tungstenite::accept_async(tcp2).await
            {
                let _ = ws2.next().await;
                let _ = ws2
                    .send(TungMsg::Text(
                        r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#.into(),
                    ))
                    .await;
                let _ = ws2.send(TungMsg::Binary(build_test_init_segment())).await;
                let _ = ws2
                    .send(TungMsg::Binary(build_test_fragment(true, &[0x42; 64])))
                    .await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });

        let mut sub = hub
            .subscribe_client(&ws_base, "cozinha_sub")
            .await
            .expect("client subscription must succeed");

        // Wait up to 2s for the retry worker to reconnect and broadcast the keyframe fragment
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut received_fragment = false;
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(MseLivePacket::Fragment { .. })) =
                tokio::time::timeout(Duration::from_millis(800), sub.rx.recv()).await
            {
                received_fragment = true;
                break;
            }
        }
        assert!(
            received_fragment,
            "MseStreamHub must retry upstream go2rtc while active client is subscribed"
        );
        assert!(hub.get_fresh_burst("cozinha_sub").await.is_some());
    }

    #[test]
    fn extract_stream_src_and_stream_validation_reject_injection_and_duplicate_keys() {
        assert_eq!(
            extract_stream_src_from_query("src=front_door_sub"),
            Some("front_door_sub".to_string())
        );
        assert_eq!(
            extract_stream_src_from_query("foo=1&src=cam-01&bar=2"),
            Some("cam-01".to_string())
        );
        // Reject query/path/fragment injection & duplicate src parameters
        assert_eq!(
            extract_stream_src_from_query("src=front_door&src=evil"),
            None
        );
        assert_eq!(extract_stream_src_from_query("src=../etc/passwd"), None);
        assert_eq!(extract_stream_src_from_query("src=cam%20name"), None);
        assert_eq!(extract_stream_src_from_query("src=cam#frag"), None);
        assert_eq!(
            extract_stream_src_from_query(&format!("src={}", "a".repeat(129))),
            None
        );
        assert!(!is_valid_stream_name(".hidden"));
        assert!(!is_valid_stream_name("cam/sub"));
    }

    #[test]
    fn mse_gop_cache_rejects_oversized_codec_init_and_sync_frames_without_corrupting_existing_gop()
    {
        let mut cache = MseGopCache::new();
        let valid_codec = r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#;
        assert!(cache.ingest_text(valid_codec));

        // Oversized codec message (> MAX_CODEC_MSG_BYTES) must be rejected and preserve valid_codec
        let huge_codec = format!(
            r#"{{"type":"mse","value":"{}"}}"#,
            "A".repeat(MAX_CODEC_MSG_BYTES + 128)
        );
        assert!(!cache.ingest_text(&huge_codec));
        assert_eq!(cache.codec_msg.as_deref(), Some(valid_codec));

        // Valid init + sync keyframe
        let init = build_test_init_segment();
        cache.ingest_binary(init.clone());
        let gop1_i = build_test_fragment(true, &[0x33; 128]);
        assert!(cache.ingest_binary(gop1_i.clone()));
        assert!(cache.has_ready_burst());

        // Oversized init segment (> MAX_INIT_SEGMENT_BYTES) must be rejected without clearing GOP
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41");
        let huge_moov = make_box(b"moov", &vec![0u8; MAX_INIT_SEGMENT_BYTES + 1024]);
        let mut huge_init = Vec::with_capacity(ftyp.len() + huge_moov.len());
        huge_init.extend_from_slice(&ftyp);
        huge_init.extend_from_slice(&huge_moov);
        let (is_sync_init, pkt_init) = cache.ingest_binary_with_seq(Bytes::from(huge_init));
        assert!(!is_sync_init);
        assert!(pkt_init.is_none());
        assert_eq!(cache.init_segment, Some(init));
        assert_eq!(cache.gop_fragments.len(), 1);

        // Oversized sync keyframe (> MAX_GOP_BYTES) must be rejected without wiping gop_fragments
        let huge_sync = build_test_fragment(true, &vec![0x77u8; MAX_GOP_BYTES]);
        let (is_sync_huge, pkt_huge) = cache.ingest_binary_with_seq(huge_sync);
        assert!(!is_sync_huge);
        assert!(pkt_huge.is_none());
        assert_eq!(cache.gop_fragments.len(), 1);
        assert_eq!(cache.gop_fragments[0], gop1_i);
    }

    #[tokio::test]
    async fn mse_subscription_recovers_from_broadcast_lag_and_decrements_active_clients_on_drop() {
        let hub = MseStreamHub::new();
        let stream = "yard_sub";
        hub.ingest_upstream_text(
            stream,
            r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#,
        )
        .await;
        hub.ingest_upstream_binary(stream, build_test_init_segment())
            .await;
        hub.ingest_upstream_binary(stream, build_test_fragment(true, &[0x10; 64]))
            .await;

        let mut sub = hub
            .subscribe_client("ws://127.0.0.1:9", stream)
            .await
            .expect("subscription must succeed from warm cache");
        assert_eq!(sub.active_clients.load(Ordering::SeqCst), 1);

        // Overflow the 256-slot broadcast channel to force RecvError::Lagged on `sub.rx`
        for i in 0..300u16 {
            let is_key = i == 290;
            let payload = [(i & 0xFF) as u8; 32];
            hub.ingest_upstream_binary(stream, build_test_fragment(is_key, &payload))
                .await;
        }

        let resync = sub.recv_or_resync(&hub, stream).await;
        assert!(
            resync.is_err(),
            "lagged receiver must trigger resync burst error branch"
        );
        let refreshed_burst = resync
            .err()
            .flatten()
            .expect("resync must provide the latest fresh GOP burst");
        assert!(!refreshed_burst.gop_fragments.is_empty());

        let active_ref = sub.active_clients.clone();
        drop(sub);
        assert_eq!(active_ref.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cycle2_mse_rejects_malformed_boxes_error_json_skips_stale_post_lag_seq_and_bounds_cache_map()
     {
        // 1. MSE-C2-01a: Non-MSE or error JSON containing "mse" must be rejected without clobbering codec_msg
        let mut cache = MseGopCache::new();
        let valid_codec = r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#;
        assert!(cache.ingest_text(valid_codec));
        assert!(!cache.ingest_text(r#"{"type":"error","value":"mse codec unsupported"}"#));
        assert!(!cache.ingest_text(r#"{"type":"mse","value":"   "}"#));
        assert!(!cache.ingest_text(r#"not json with "type" and "mse""#));
        assert_eq!(cache.codec_msg.as_deref(), Some(valid_codec));

        // 2. MSE-C2-01b: Truncated init segment (ftyp without moov or truncated moov) & malformed fragments
        //    must be rejected without incrementing last_seq or polluting gop_fragments
        let init = build_test_init_segment();
        assert!(cache.ingest_binary_with_seq(init.clone()).1.is_some());
        let keyframe = build_test_fragment(true, &[0x55; 64]);
        let (is_sync, pkt) = cache.ingest_binary_with_seq(keyframe.clone());
        assert!(is_sync);
        assert!(pkt.is_some());
        assert_eq!(cache.last_seq, 1);
        assert_eq!(cache.gop_fragments.len(), 1);

        // ftyp alone (missing moov) must not replace init_segment or wipe gop_fragments
        let ftyp_only = Bytes::from(make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41"));
        assert!(cache.ingest_binary_with_seq(ftyp_only).1.is_none());
        assert_eq!(cache.init_segment, Some(init.clone()));
        assert_eq!(cache.gop_fragments.len(), 1);

        // Truncated moov appended to ftyp must be rejected
        let mut truncated_init = init.to_vec();
        truncated_init.truncate(truncated_init.len() - 4);
        assert!(
            cache
                .ingest_binary_with_seq(Bytes::from(truncated_init))
                .1
                .is_none()
        );
        assert_eq!(cache.gop_fragments.len(), 1);

        // Garbage binary / truncated moof+mdat fragment must NOT be accepted as a delta frame
        assert!(
            cache
                .ingest_binary_with_seq(Bytes::from_static(b"garbage_non_mp4_bytes"))
                .1
                .is_none()
        );
        let mut truncated_frag = build_test_fragment(false, &[0x66; 64]).to_vec();
        truncated_frag.truncate(truncated_frag.len() - 5);
        assert!(
            cache
                .ingest_binary_with_seq(Bytes::from(truncated_frag))
                .1
                .is_none()
        );
        assert_eq!(
            cache.last_seq, 1,
            "malformed fragments must not advance last_seq"
        );
        assert_eq!(
            cache.gop_fragments.len(),
            1,
            "malformed fragments must not be appended to gop_fragments"
        );

        // 3. MSE-C2-02a: After RecvError::Lagged resync, next recv_or_resync must skip stale ring buffer
        //    entries (seq <= refreshed_burst.last_seq) and deliver only the next new fragment (seq = 302)
        let hub = MseStreamHub::new();
        let stream = "driveway_sub";
        hub.ingest_upstream_text(stream, valid_codec).await;
        hub.ingest_upstream_binary(stream, build_test_init_segment())
            .await;
        hub.ingest_upstream_binary(stream, build_test_fragment(true, &[0x01; 48]))
            .await;

        let mut sub = hub
            .subscribe_client("ws://127.0.0.1:9", stream)
            .await
            .unwrap();
        for i in 0..300u16 {
            let is_key = i == 290;
            let payload = [(i & 0xFF) as u8; 32];
            hub.ingest_upstream_binary(stream, build_test_fragment(is_key, &payload))
                .await;
        }
        let lag_burst = sub
            .recv_or_resync(&hub, stream)
            .await
            .err()
            .flatten()
            .expect("must resync to latest GOP burst");
        assert_eq!(lag_burst.last_seq, 301);

        // Now ingest fragment seq=302; recv_or_resync must skip stale seq 46..=301 in broadcast ring and return 302
        hub.ingest_upstream_binary(stream, build_test_fragment(false, &[0xFE; 32]))
            .await;
        match sub.recv_or_resync(&hub, stream).await {
            Ok(MseLivePacket::Fragment { seq, .. }) => {
                assert_eq!(
                    seq, 302,
                    "recv_or_resync must skip stale pre-burst fragments in ring buffer"
                );
            }
            other => panic!("expected Fragment seq=302, got {other:?}"),
        }

        // 4. MSE-C2-02b: Ingesting > MAX_CACHED_STREAMS fresh streams within MAX_GOP_CACHE_AGE
        //    must strictly bound caches.len() <= MAX_CACHED_STREAMS via LRU eviction
        for idx in 0..(MAX_CACHED_STREAMS + 20) {
            let s_name = format!("cam_burst_{idx:03}");
            hub.ingest_upstream_text(&s_name, valid_codec).await;
        }
        let cache_count = hub.caches.read().await.len();
        assert!(
            cache_count <= MAX_CACHED_STREAMS,
            "caches map len ({cache_count}) must never exceed MAX_CACHED_STREAMS ({MAX_CACHED_STREAMS})"
        );
    }

    #[tokio::test]
    async fn cycle3_mse_enforces_active_session_cap_and_rejects_empty_mdat_or_partial_bad_traf() {
        // 1. MSE-C3-01: Media fragments with empty mdat payload or a second malformed traf/moof must be rejected
        let empty_mdat_frag = build_test_fragment(true, &[]);
        assert!(
            !is_complete_fmp4_media_fragment(&empty_mdat_frag),
            "media fragment with 0-byte mdat payload must be rejected"
        );

        let valid_tfhd = make_full_box(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());
        let mut trun_body = Vec::new();
        trun_body.extend_from_slice(&1u32.to_be_bytes());
        trun_body.extend_from_slice(&64i32.to_be_bytes());
        let valid_trun = make_full_box(b"trun", 0, 0x000001, &trun_body);
        let mut good_traf_children = Vec::new();
        good_traf_children.extend_from_slice(&valid_tfhd);
        good_traf_children.extend_from_slice(&valid_trun);
        let good_traf = make_box(b"traf", &good_traf_children);
        // Second traf missing trun
        let bad_traf = make_box(b"traf", &valid_tfhd);
        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&good_traf);
        moof_children.extend_from_slice(&bad_traf);
        let mixed_moof = make_box(b"moof", &moof_children);
        let mdat = make_box(b"mdat", &[0xAB; 32]);
        let mut mixed_frag = Vec::new();
        mixed_frag.extend_from_slice(&mixed_moof);
        mixed_frag.extend_from_slice(&mdat);
        assert!(
            !is_complete_fmp4_media_fragment(&mixed_frag),
            "moof containing a second traf without trun must be rejected"
        );

        // 2. MSE-C3-02: Holding MAX_CACHED_STREAMS active client subscriptions must prevent a 65th
        //    non-persistent subscription or prewarm from exceeding MAX_CACHED_STREAMS sessions
        let hub = MseStreamHub::new();
        let valid_codec = r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#;
        let init = build_test_init_segment();
        let keyframe = build_test_fragment(true, &[0x44; 48]);

        let mut active_subs = Vec::with_capacity(MAX_CACHED_STREAMS);
        for idx in 0..MAX_CACHED_STREAMS {
            let s_name = format!("active_cam_{idx:03}");
            hub.ingest_upstream_text(&s_name, valid_codec).await;
            hub.ingest_upstream_binary(&s_name, init.clone()).await;
            hub.ingest_upstream_binary(&s_name, keyframe.clone()).await;
            let sub = hub
                .subscribe_client("ws://127.0.0.1:9", &s_name)
                .await
                .expect("subscription within MAX_CACHED_STREAMS must succeed");
            active_subs.push(sub);
        }
        assert_eq!(hub.sessions.read().await.len(), MAX_CACHED_STREAMS);

        // 65th stream while all 64 sessions have active_clients == 1 must fail closed (None / false)
        assert!(
            !hub.prewarm_stream("ws://127.0.0.1:9", "overflow_stream_065")
                .await,
            "prewarm_stream must refuse to exceed MAX_CACHED_STREAMS when all slots have active clients"
        );
        assert!(
            hub.subscribe_client("ws://127.0.0.1:9", "overflow_stream_065")
                .await
                .is_none(),
            "subscribe_client must refuse to exceed MAX_CACHED_STREAMS when all slots have active clients"
        );
        assert_eq!(hub.sessions.read().await.len(), MAX_CACHED_STREAMS);

        // Dropping one active subscription allows a new stream to evict the now-idle slot
        active_subs.pop();
        hub.ingest_upstream_text("overflow_stream_065", valid_codec)
            .await;
        hub.ingest_upstream_binary("overflow_stream_065", init)
            .await;
        hub.ingest_upstream_binary("overflow_stream_065", keyframe)
            .await;
        let sub_after_drop = hub
            .subscribe_client("ws://127.0.0.1:9", "overflow_stream_065")
            .await;
        assert!(
            sub_after_drop.is_some(),
            "new stream must succeed by evicting the idle session once a client drops"
        );
        assert_eq!(hub.sessions.read().await.len(), MAX_CACHED_STREAMS);
    }

    #[tokio::test]
    async fn recv_or_resync_preserves_needs_resync_when_cancelled_at_resync_after_lag() {
        let hub = MseStreamHub::new();
        let stream = "patio_sub";
        let valid_codec = r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#;
        hub.ingest_upstream_text(stream, valid_codec).await;
        hub.ingest_upstream_binary(stream, build_test_init_segment())
            .await;
        hub.ingest_upstream_binary(stream, build_test_fragment(true, &[0x01; 48]))
            .await;

        let mut sub = hub
            .subscribe_client("ws://127.0.0.1:9", stream)
            .await
            .expect("client subscription must succeed");
        assert!(!sub.needs_resync);

        // Overflow the 256-slot broadcast ring buffer so the next `rx.recv().await` returns `RecvError::Lagged`
        for i in 0..300u16 {
            let is_key = i == 290;
            let payload = [(i & 0xFF) as u8; 32];
            hub.ingest_upstream_binary(stream, build_test_fragment(is_key, &payload))
                .await;
        }

        // Hold a write lock on `hub.caches` so `resync_after_lag(stream).await` suspends at `self.caches.read().await`
        let caches_write_guard = hub.caches.write().await;

        // Poll `recv_or_resync` inside a `tokio::select!` that cancels it while suspended at `resync_after_lag`
        tokio::select! {
            _ = sub.recv_or_resync(&hub, stream) => {
                panic!("recv_or_resync must suspend on hub.caches.read() while write lock is held");
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }

        assert!(
            sub.needs_resync,
            "cancelling recv_or_resync while suspended at resync_after_lag must preserve needs_resync = true"
        );

        // Release write lock and push a non-keyframe delta fragment into the broadcast ring
        drop(caches_write_guard);
        hub.ingest_upstream_binary(stream, build_test_fragment(false, &[0xDE; 32]))
            .await;

        // Next `recv_or_resync` call must immediately complete the pending resync and return Err(Some(burst))
        // instead of returning the post-gap delta fragment!
        let recovered = sub.recv_or_resync(&hub, stream).await;
        assert!(
            !sub.needs_resync,
            "completing the resync must clear needs_resync = false"
        );
        let burst = recovered
            .err()
            .flatten()
            .expect("post-cancellation recv_or_resync must deliver the GOP keyframe burst");
        assert!(!burst.gop_fragments.is_empty());
        assert_eq!(burst.last_seq, 302);
    }
}
