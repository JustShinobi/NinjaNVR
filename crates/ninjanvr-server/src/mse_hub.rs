use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Notify, RwLock, broadcast};
use tokio_tungstenite::tungstenite::Message as TungMsg;

pub const MAX_GOP_FRAGMENTS: usize = 90;
pub const MAX_GOP_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_GOP_CACHE_AGE: Duration = Duration::from_secs(10);
pub const PREWARM_KEEPALIVE_TTL: Duration = Duration::from_secs(45);

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
        if text.contains("\"type\"") && text.contains("\"mse\"") {
            self.codec_msg = Some(text.to_string());
            self.updated_at = Instant::now();
            return true;
        }
        false
    }

    pub fn ingest_binary(&mut self, bytes: Bytes) -> bool {
        self.ingest_binary_with_seq(bytes).0
    }

    pub fn ingest_binary_with_seq(&mut self, bytes: Bytes) -> (bool, Option<MseLivePacket>) {
        if bytes.is_empty() {
            return (false, None);
        }
        if ninjanvr_vod::is_fmp4_init_segment(&bytes) {
            self.init_segment = Some(bytes.clone());
            self.gop_fragments.clear();
            self.gop_bytes = 0;
            self.updated_at = Instant::now();
            return (false, Some(MseLivePacket::Init(bytes)));
        }
        self.last_seq = self.last_seq.wrapping_add(1);
        let seq = self.last_seq;
        if ninjanvr_vod::is_fmp4_sync_fragment(&bytes) {
            self.gop_fragments.clear();
            self.gop_bytes = 0;
            if bytes.len() <= MAX_GOP_BYTES {
                self.gop_bytes = bytes.len();
                self.gop_fragments.push_back(bytes.clone());
            }
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
    active_clients: Arc<AtomicUsize>,
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

    pub async fn ingest_upstream_text(&self, stream: &str, text: &str) {
        if stream.is_empty() {
            return;
        }
        let updated = {
            let mut guard = self.caches.write().await;
            guard
                .entry(stream.to_string())
                .or_default()
                .ingest_text(text)
        };
        if updated && let Some(sess) = self.sessions.read().await.get(stream) {
            let _ = sess.tx.send(MseLivePacket::Codec(text.to_string()));
        }
    }

    pub async fn ingest_upstream_binary(&self, stream: &str, bytes: Bytes) -> bool {
        if stream.is_empty() {
            return false;
        }
        let (is_sync, pkt_opt) = {
            let mut guard = self.caches.write().await;
            guard
                .entry(stream.to_string())
                .or_default()
                .ingest_binary_with_seq(bytes)
        };
        if let Some(sess) = self.sessions.read().await.get(stream) {
            if let Some(pkt) = pkt_opt {
                let _ = sess.tx.send(pkt);
            }
            if is_sync {
                sess.ready_notify.notify_waiters();
            }
        }
        is_sync
    }

    pub async fn get_fresh_burst(&self, stream: &str) -> Option<MseGopBurst> {
        if stream.is_empty() {
            return None;
        }
        let guard = self.caches.read().await;
        guard
            .get(stream)
            .and_then(|c| c.snapshot_fresh_burst(MAX_GOP_CACHE_AGE))
    }

    async fn ensure_upstream_session(
        &self,
        go2rtc_ws_base: &str,
        stream_key: &str,
        persistent: bool,
    ) -> (
        Arc<Notify>,
        broadcast::Receiver<MseLivePacket>,
        Arc<AtomicUsize>,
    ) {
        let now = Instant::now();
        let new_expires = now + PREWARM_KEEPALIVE_TTL;

        let mut sessions = self.sessions.write().await;
        if let Some(sess) = sessions.get_mut(stream_key) {
            sess.expires_at = new_expires;
            if persistent {
                sess.persistent = true;
            }
            return (
                sess.ready_notify.clone(),
                sess.tx.subscribe(),
                sess.active_clients.clone(),
            );
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

        (notify, rx, active_clients)
    }

    /// Spawns or refreshes a shared background upstream `go2rtc` MSE consumer for `stream`
    /// that keeps its `MseGopCache` warm for `PREWARM_KEEPALIVE_TTL` (45s).
    pub async fn prewarm_stream(&self, go2rtc_ws_base: &str, stream: &str) -> bool {
        let stream_key = stream.trim();
        if stream_key.is_empty() {
            return false;
        }

        let (notify, _rx, _clients) = self
            .ensure_upstream_session(go2rtc_ws_base, stream_key, false)
            .await;

        if self.get_fresh_burst(stream_key).await.is_some() {
            return true;
        }

        let _ = tokio::time::timeout(Duration::from_millis(250), notify.notified()).await;
        self.get_fresh_burst(stream_key).await.is_some()
    }

    /// Starts persistent background MSE GOP warmers for all configured cameras (`{cam}` and `{cam}_sub`)
    /// so switching to any camera or layout in the console delivers an instant keyframe burst (< 5ms).
    pub async fn start_persistent_camera_streams(
        &self,
        go2rtc_ws_base: &str,
        cameras: Vec<String>,
    ) {
        for cam in cameras {
            let trimmed = cam.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = self
                .ensure_upstream_session(go2rtc_ws_base, trimmed, true)
                .await;
            let sub = format!("{trimmed}_sub");
            let _ = self
                .ensure_upstream_session(go2rtc_ws_base, &sub, true)
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
        if stream_key.is_empty() {
            return None;
        }

        let (notify, _initial_rx, active_clients) = self
            .ensure_upstream_session(go2rtc_ws_base, stream_key, false)
            .await;
        active_clients.fetch_add(1, Ordering::SeqCst);

        if self.get_fresh_burst(stream_key).await.is_none() {
            let _ = tokio::time::timeout(Duration::from_millis(120), notify.notified()).await;
        }

        // Subscribe and snapshot under the locks so `last_seq` aligns with `rx`
        let rx = {
            let sessions = self.sessions.read().await;
            sessions.get(stream_key)?.tx.subscribe()
        };
        let burst = {
            let caches = self.caches.read().await;
            caches
                .get(stream_key)
                .and_then(|c| c.snapshot_fresh_burst(MAX_GOP_CACHE_AGE))
        };

        Some(MseClientSubscription {
            burst,
            rx,
            active_clients,
        })
    }
}

async fn run_upstream_worker(
    hub: MseStreamHub,
    go2rtc_ws_base: String,
    stream: String,
    notify: Arc<Notify>,
) {
    let ws_url = format!("{}/api/ws?src={}", go2rtc_ws_base, stream);
    loop {
        let should_run = {
            let sessions = hub.sessions.read().await;
            sessions
                .get(&stream)
                .map(|s| {
                    s.persistent
                        || s.active_clients.load(Ordering::SeqCst) > 0
                        || Instant::now() < s.expires_at
                })
                .unwrap_or(false)
        };
        if !should_run {
            break;
        }

        if let Ok((ws_stream, _)) = tokio_tungstenite::connect_async(&ws_url).await {
            let (mut sink, mut stream_rx) = ws_stream.split();
            let handshake = r#"{"type":"mse","value":"avc1.640029,avc1.64002A,avc1.640028,avc1.4D401F,avc1.4D0029,avc1.42E01E,hev1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus"}"#;
            if sink.send(TungMsg::Text(handshake.into())).await.is_ok() {
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
                        break;
                    }

                    match tokio::time::timeout(Duration::from_secs(3), stream_rx.next()).await {
                        Ok(Some(Ok(TungMsg::Text(t)))) => {
                            consecutive_timeouts = 0;
                            hub.ingest_upstream_text(&stream, t.as_str()).await;
                        }
                        Ok(Some(Ok(TungMsg::Binary(b)))) => {
                            consecutive_timeouts = 0;
                            hub.ingest_upstream_binary(&stream, b).await;
                        }
                        Ok(Some(Ok(TungMsg::Ping(p)))) => {
                            consecutive_timeouts = 0;
                            let _ = sink.send(TungMsg::Pong(p)).await;
                        }
                        Ok(Some(Ok(TungMsg::Pong(_)))) => {
                            consecutive_timeouts = 0;
                        }
                        Ok(Some(Ok(TungMsg::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break,
                        Err(_) => {
                            consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                            if consecutive_timeouts >= 3 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let should_retry = {
            let sessions = hub.sessions.read().await;
            sessions
                .get(&stream)
                .map(|s| {
                    s.persistent
                        || s.active_clients.load(Ordering::SeqCst) > 0
                        || Instant::now() < s.expires_at
                })
                .unwrap_or(false)
        };
        if !should_retry {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    hub.sessions.write().await.remove(&stream);
    notify.notify_waiters();
}

pub fn extract_stream_src_from_query(query: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == "src"
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
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
}
