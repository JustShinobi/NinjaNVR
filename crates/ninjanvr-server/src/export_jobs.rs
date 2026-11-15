//! Isolated runtime state for asynchronous MP4 export jobs and historical timeline frame caching.
//!
//! Owned by [`crate::AppState`] (zero process-global singletons) so parallel tests and config
//! reloads stay isolated.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

pub const MAX_ACTIVE_EXPORT_JOBS: usize = 24;
const MAX_TERMINAL_JOBS: usize = 128;
const TERMINAL_JOB_RETENTION_SECS: f64 = 900.0;
const WS_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(350);
const WS_PROGRESS_MIN_STEP_PCT: f64 = 2.0;

const MAX_HISTORICAL_FRAME_ENTRIES: usize = 256;
const MAX_HISTORICAL_FRAME_CACHE_BYTES: usize = 16 * 1024 * 1024;
const HISTORICAL_FRAME_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ExportJobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportJobSnapshot {
    pub id: String,
    pub camera: String,
    pub name: String,
    pub start_time: f64,
    pub end_time: f64,
    pub include_audio: bool,
    pub status: ExportJobStatus,
    pub progress_pct: f64,
    pub created_at: f64,
    pub updated_at: f64,
    pub video_url: Option<String>,
    pub thumb_url: Option<String>,
    pub error: Option<String>,
}

struct ExportJobEntry {
    snapshot: ExportJobSnapshot,
    cancel_flag: Arc<AtomicBool>,
    last_ws_emit: Option<Instant>,
    last_ws_pct: f64,
}

#[derive(Clone)]
pub struct ExportJobManager {
    jobs: Arc<RwLock<BTreeMap<String, ExportJobEntry>>>,
    pub worker_semaphore: Arc<Semaphore>,
}

impl Default for ExportJobManager {
    fn default() -> Self {
        Self::new(2)
    }
}

impl ExportJobManager {
    pub fn new(max_concurrent_jobs: usize) -> Self {
        Self {
            jobs: Arc::new(RwLock::new(BTreeMap::new())),
            worker_semaphore: Arc::new(Semaphore::new(max_concurrent_jobs.max(1))),
        }
    }

    pub fn has_active_capacity(&self) -> bool {
        self.jobs.read().is_ok_and(|guard| {
            guard
                .values()
                .filter(|e| !e.snapshot.status.is_terminal())
                .count()
                < MAX_ACTIVE_EXPORT_JOBS
        })
    }

    #[expect(clippy::too_many_arguments)]
    pub fn try_register_queued(
        &self,
        id: String,
        camera: String,
        name: String,
        start_time: f64,
        end_time: f64,
        include_audio: bool,
        now_sec: f64,
    ) -> std::result::Result<(ExportJobSnapshot, Arc<AtomicBool>), usize> {
        let mut guard = self.jobs.write().map_err(|_| MAX_ACTIVE_EXPORT_JOBS)?;
        Self::prune_locked(&mut guard, now_sec);
        let active_count = guard
            .values()
            .filter(|e| !e.snapshot.status.is_terminal())
            .count();
        if active_count >= MAX_ACTIVE_EXPORT_JOBS {
            return Err(MAX_ACTIVE_EXPORT_JOBS);
        }
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let snapshot = ExportJobSnapshot {
            id: id.clone(),
            camera,
            name,
            start_time,
            end_time,
            include_audio,
            status: ExportJobStatus::Queued,
            progress_pct: 0.0,
            created_at: now_sec,
            updated_at: now_sec,
            video_url: None,
            thumb_url: None,
            error: None,
        };
        guard.insert(
            id,
            ExportJobEntry {
                snapshot: snapshot.clone(),
                cancel_flag: cancel_flag.clone(),
                last_ws_emit: None,
                last_ws_pct: 0.0,
            },
        );
        Ok((snapshot, cancel_flag))
    }

    #[expect(clippy::too_many_arguments)]
    pub fn register_queued(
        &self,
        id: String,
        camera: String,
        name: String,
        start_time: f64,
        end_time: f64,
        include_audio: bool,
        now_sec: f64,
    ) -> (ExportJobSnapshot, Arc<AtomicBool>) {
        if let Ok(pair) = self.try_register_queued(
            id.clone(),
            camera.clone(),
            name.clone(),
            start_time,
            end_time,
            include_audio,
            now_sec,
        ) {
            return pair;
        }
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let snapshot = ExportJobSnapshot {
            id,
            camera,
            name,
            start_time,
            end_time,
            include_audio,
            status: ExportJobStatus::Cancelled,
            progress_pct: 0.0,
            created_at: now_sec,
            updated_at: now_sec,
            video_url: None,
            thumb_url: None,
            error: Some("Export queue full".to_string()),
        };
        (snapshot, cancel_flag)
    }

    pub fn mark_running(&self, id: &str, now_sec: f64) -> Option<ExportJobSnapshot> {
        let mut guard = self.jobs.write().ok()?;
        let entry = guard.get_mut(id)?;
        if entry.cancel_flag.load(Ordering::SeqCst) || entry.snapshot.status.is_terminal() {
            entry.snapshot.status = ExportJobStatus::Cancelled;
            entry.snapshot.updated_at = now_sec;
            return None;
        }
        entry.snapshot.status = ExportJobStatus::Running;
        entry.snapshot.progress_pct = entry.snapshot.progress_pct.max(1.0);
        entry.snapshot.updated_at = now_sec;
        entry.last_ws_emit = Some(Instant::now());
        entry.last_ws_pct = entry.snapshot.progress_pct;
        Some(entry.snapshot.clone())
    }

    /// Updates job progress (`frac` in `0.0..=1.0`) and returns `(snapshot, should_broadcast_ws)`.
    pub fn update_progress(
        &self,
        id: &str,
        frac: f64,
        now_sec: f64,
    ) -> Option<(ExportJobSnapshot, bool)> {
        if !frac.is_finite() {
            return None;
        }
        let pct = ((frac.clamp(0.0, 1.0) * 1000.0).round() / 10.0).clamp(0.0, 99.9);
        let mut guard = self.jobs.write().ok()?;
        let entry = guard.get_mut(id)?;
        if entry.snapshot.status.is_terminal() || entry.cancel_flag.load(Ordering::SeqCst) {
            return None;
        }
        if entry.snapshot.status == ExportJobStatus::Queued {
            entry.snapshot.status = ExportJobStatus::Running;
        }
        if pct > entry.snapshot.progress_pct {
            entry.snapshot.progress_pct = pct;
        }
        entry.snapshot.updated_at = now_sec;

        let now_inst = Instant::now();
        let time_due = entry
            .last_ws_emit
            .is_none_or(|t| now_inst.duration_since(t) >= WS_PROGRESS_MIN_INTERVAL);
        let pct_due = (entry.snapshot.progress_pct - entry.last_ws_pct) >= WS_PROGRESS_MIN_STEP_PCT;
        let should_emit = time_due && pct_due;
        if should_emit {
            entry.last_ws_emit = Some(now_inst);
            entry.last_ws_pct = entry.snapshot.progress_pct;
        }
        Some((entry.snapshot.clone(), should_emit))
    }

    pub fn mark_completed(
        &self,
        id: &str,
        video_url: String,
        thumb_url: String,
        now_sec: f64,
    ) -> Option<ExportJobSnapshot> {
        let mut guard = self.jobs.write().ok()?;
        let entry = guard.get_mut(id)?;
        if entry.cancel_flag.load(Ordering::SeqCst)
            || entry.snapshot.status == ExportJobStatus::Cancelled
        {
            entry.snapshot.status = ExportJobStatus::Cancelled;
            entry.snapshot.updated_at = now_sec;
            return None;
        }
        entry.snapshot.status = ExportJobStatus::Completed;
        entry.snapshot.progress_pct = 100.0;
        entry.snapshot.video_url = Some(video_url);
        entry.snapshot.thumb_url = Some(thumb_url);
        entry.snapshot.error = None;
        entry.snapshot.updated_at = now_sec;
        Some(entry.snapshot.clone())
    }

    pub fn mark_failed(
        &self,
        id: &str,
        error_msg: String,
        now_sec: f64,
    ) -> Option<ExportJobSnapshot> {
        let mut guard = self.jobs.write().ok()?;
        let entry = guard.get_mut(id)?;
        if entry.cancel_flag.load(Ordering::SeqCst)
            || entry.snapshot.status == ExportJobStatus::Cancelled
        {
            entry.snapshot.status = ExportJobStatus::Cancelled;
            entry.snapshot.error = None;
            entry.snapshot.updated_at = now_sec;
            return Some(entry.snapshot.clone());
        }
        entry.snapshot.status = ExportJobStatus::Failed;
        entry.snapshot.error = Some(error_msg);
        entry.snapshot.updated_at = now_sec;
        Some(entry.snapshot.clone())
    }

    /// Signals cancellation for `id` if queued or running, returning the updated snapshot.
    pub fn cancel(&self, id: &str, now_sec: f64) -> Option<ExportJobSnapshot> {
        let mut guard = self.jobs.write().ok()?;
        let entry = guard.get_mut(id)?;
        entry.cancel_flag.store(true, Ordering::SeqCst);
        if !entry.snapshot.status.is_terminal() {
            entry.snapshot.status = ExportJobStatus::Cancelled;
            entry.snapshot.error = None;
            entry.snapshot.updated_at = now_sec;
        }
        Some(entry.snapshot.clone())
    }

    pub fn get(&self, id: &str) -> Option<ExportJobSnapshot> {
        let guard = self.jobs.read().ok()?;
        guard.get(id).map(|e| e.snapshot.clone())
    }

    pub fn remove(&self, id: &str) {
        if let Ok(mut guard) = self.jobs.write()
            && let Some(entry) = guard.remove(id)
        {
            entry.cancel_flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn list_for_allowed_cameras(
        &self,
        allowed: Option<&[String]>,
        now_sec: f64,
    ) -> Vec<ExportJobSnapshot> {
        let mut out: Vec<ExportJobSnapshot> = self
            .jobs
            .write()
            .map(|mut guard| {
                Self::prune_locked(&mut guard, now_sec);
                guard
                    .values()
                    .filter(|entry| match allowed {
                        None => true,
                        Some(list) if list.iter().any(|a| a == "*") => true,
                        Some(list) => list.iter().any(|a| a == &entry.snapshot.camera),
                    })
                    .map(|e| e.snapshot.clone())
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| {
            b.created_at
                .partial_cmp(&a.created_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.id.cmp(&a.id))
        });
        out
    }

    fn prune_locked(map: &mut BTreeMap<String, ExportJobEntry>, now_sec: f64) {
        map.retain(|_, entry| {
            if !entry.snapshot.status.is_terminal() {
                return true;
            }
            if now_sec.is_finite() && entry.snapshot.updated_at.is_finite() {
                (now_sec - entry.snapshot.updated_at) <= TERMINAL_JOB_RETENTION_SECS
            } else {
                false
            }
        });
        let mut terminal_keys: Vec<(f64, String)> = map
            .iter()
            .filter(|(_, e)| e.snapshot.status.is_terminal())
            .map(|(k, e)| (e.snapshot.updated_at, k.clone()))
            .collect();
        if terminal_keys.len() > MAX_TERMINAL_JOBS {
            terminal_keys
                .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let excess = terminal_keys.len() - MAX_TERMINAL_JOBS;
            for (_, k) in terminal_keys.into_iter().take(excess) {
                map.remove(&k);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HistoricalFrameKey {
    pub camera: String,
    pub bucket_ms: u64,
    pub width: u32,
    pub mask_sig: String,
}

struct HistoricalFrameEntry {
    inserted_at: Instant,
    jpeg: Bytes,
    source: &'static str,
}

/// Bounded LRU cache + concurrency semaphore for on-demand historical timeline frames.
#[derive(Clone)]
pub struct HistoricalFrameCache {
    inner: Arc<RwLock<HistoricalFrameLru>>,
    pub extract_semaphore: Arc<Semaphore>,
}

struct HistoricalFrameLru {
    entries: HashMap<HistoricalFrameKey, HistoricalFrameEntry>,
    order: VecDeque<HistoricalFrameKey>,
    total_bytes: usize,
}

impl Default for HistoricalFrameCache {
    fn default() -> Self {
        Self::new(3)
    }
}

impl HistoricalFrameCache {
    pub fn new(max_concurrent_extractions: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HistoricalFrameLru {
                entries: HashMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
            })),
            extract_semaphore: Arc::new(Semaphore::new(max_concurrent_extractions.max(1))),
        }
    }

    pub fn get(&self, key: &HistoricalFrameKey) -> Option<(Bytes, &'static str)> {
        let now = Instant::now();
        let mut guard = self.inner.write().ok()?;
        if let Some(existing) = guard.entries.get(key) {
            if now.duration_since(existing.inserted_at) <= HISTORICAL_FRAME_TTL {
                let out = (existing.jpeg.clone(), existing.source);
                if guard.order.back() != Some(key) {
                    guard.order.retain(|k| k != key);
                    guard.order.push_back(key.clone());
                }
                return Some(out);
            }
            if let Some(expired) = guard.entries.remove(key) {
                guard.total_bytes = guard.total_bytes.saturating_sub(expired.jpeg.len());
                guard.order.retain(|k| k != key);
            }
        }
        None
    }

    pub fn insert(&self, key: HistoricalFrameKey, jpeg: Bytes, source: &'static str) {
        if jpeg.is_empty() || jpeg.len() > 2 * 1024 * 1024 {
            return;
        }
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        if let Some(prev) = guard.entries.remove(&key) {
            guard.total_bytes = guard.total_bytes.saturating_sub(prev.jpeg.len());
            guard.order.retain(|k| k != &key);
        }
        while (guard.entries.len() >= MAX_HISTORICAL_FRAME_ENTRIES
            || guard.total_bytes.saturating_add(jpeg.len()) > MAX_HISTORICAL_FRAME_CACHE_BYTES)
            && let Some(oldest_key) = guard.order.pop_front()
        {
            if let Some(evicted) = guard.entries.remove(&oldest_key) {
                guard.total_bytes = guard.total_bytes.saturating_sub(evicted.jpeg.len());
            }
        }
        guard.total_bytes = guard.total_bytes.saturating_add(jpeg.len());
        guard.order.push_back(key.clone());
        guard.entries.insert(
            key,
            HistoricalFrameEntry {
                inserted_at: Instant::now(),
                jpeg,
                source,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_job_manager_tracks_lifecycle_cancellation_scoping_and_pruning() {
        let mgr = ExportJobManager::new(2);
        let (snap, cancel) = mgr.register_queued(
            "exp_1".to_string(),
            "front_door".to_string(),
            "Incident 1".to_string(),
            1000.0,
            1060.0,
            true,
            2000.0,
        );
        assert_eq!(snap.status, ExportJobStatus::Queued);
        assert!(!cancel.load(Ordering::SeqCst));

        let running = mgr.mark_running("exp_1", 2001.0).unwrap();
        assert_eq!(running.status, ExportJobStatus::Running);

        let (p1, _) = mgr.update_progress("exp_1", 0.45, 2002.0).unwrap();
        assert_eq!(p1.progress_pct, 45.0);

        let done = mgr
            .mark_completed(
                "exp_1",
                "/exports/exp_1.mp4".to_string(),
                "/clips/exp_1.webp".to_string(),
                2005.0,
            )
            .unwrap();
        assert_eq!(done.status, ExportJobStatus::Completed);
        assert_eq!(done.progress_pct, 100.0);

        // Register a second camera job and cancel it
        let (_, cancel_2) = mgr.register_queued(
            "exp_2".to_string(),
            "backyard".to_string(),
            "Backyard Long".to_string(),
            1000.0,
            4600.0,
            false,
            2006.0,
        );
        let cancelled = mgr.cancel("exp_2", 2007.0).unwrap();
        assert_eq!(cancelled.status, ExportJobStatus::Cancelled);
        assert!(cancel_2.load(Ordering::SeqCst));

        // Scoped listing filters by camera
        let scoped = mgr.list_for_allowed_cameras(Some(&["front_door".to_string()]), 2010.0);
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "exp_1");

        // Stale terminal jobs (> 900s) prune automatically
        let after_ttl = mgr.list_for_allowed_cameras(None, 4000.0);
        assert!(after_ttl.is_empty());

        // Active queue capacity cap enforces MAX_ACTIVE_EXPORT_JOBS and rejects overflow
        for i in 0..MAX_ACTIVE_EXPORT_JOBS {
            assert!(
                mgr.try_register_queued(
                    format!("cap_{i}"),
                    "front_door".to_string(),
                    format!("Job {i}"),
                    100.0,
                    200.0,
                    true,
                    5000.0,
                )
                .is_ok()
            );
        }
        assert!(!mgr.has_active_capacity());
        assert_eq!(
            mgr.try_register_queued(
                "cap_overflow".to_string(),
                "front_door".to_string(),
                "Overflow".to_string(),
                100.0,
                200.0,
                true,
                5000.0,
            )
            .unwrap_err(),
            MAX_ACTIVE_EXPORT_JOBS
        );

        // Cancel-vs-complete race: mark_completed returns None once cancel() was invoked
        mgr.cancel("cap_0", 5001.0).unwrap();
        assert!(
            mgr.mark_completed(
                "cap_0",
                "/exports/cap_0.mp4".to_string(),
                "/clips/cap_0.webp".to_string(),
                5002.0,
            )
            .is_none(),
            "mark_completed must return None when job was cancelled concurrently"
        );
    }

    #[test]
    fn historical_frame_cache_evicts_lru_entries() {
        let cache = HistoricalFrameCache::new(2);
        for i in 0..(MAX_HISTORICAL_FRAME_ENTRIES + 8) {
            cache.insert(
                HistoricalFrameKey {
                    camera: "sala".to_string(),
                    bucket_ms: i as u64 * 1000,
                    width: 320,
                    mask_sig: String::new(),
                },
                Bytes::from(vec![0xFF, 0xD8, i as u8, 0xD9]),
                "segment",
            );
        }
        assert!(
            cache
                .get(&HistoricalFrameKey {
                    camera: "sala".to_string(),
                    bucket_ms: 0,
                    width: 320,
                    mask_sig: String::new(),
                })
                .is_none(),
            "oldest entry must be evicted once capacity is exceeded"
        );
        assert!(
            cache
                .get(&HistoricalFrameKey {
                    camera: "sala".to_string(),
                    bucket_ms: (MAX_HISTORICAL_FRAME_ENTRIES + 7) as u64 * 1000,
                    width: 320,
                    mask_sig: String::new(),
                })
                .is_some()
        );
    }
}
