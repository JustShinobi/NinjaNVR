//! Low-res preview frames for the Live event strip flipbook.
//!
//! Frames are sampled from the substream frames the vision pipeline has
//! already decoded, so producing them costs a downscale and a small JPEG
//! encode — no extra decoder, no ffmpeg process, no recording read. This
//! mirrors the preview-frame cache of other NVRs, with an in-memory
//! per-camera time index so event lookups are a range query instead of a
//! directory scan.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Height of a stored preview frame (width keeps the detect aspect ratio).
pub const PREVIEW_FRAME_HEIGHT: u32 = 180;
/// JPEG quality for preview frames (small files, fine for a 166 px card).
pub const PREVIEW_FRAME_JPEG_QUALITY: u8 = 70;
/// How long preview frames are kept; older events fall back to their thumbnail.
pub const PREVIEW_FRAME_RETENTION_SECS: u64 = 3 * 3600;
/// At most this many frames are returned for one review item.
pub const PREVIEW_FRAMES_PER_REVIEW: usize = 16;
/// Write at least one frame this often, even when the scene is idle.
const PREVIEW_KEEPALIVE_SECS: f64 = 30.0;

/// Decides which decoded frames become preview frames.
///
/// While something is happening: at most 1 frame/s (2 frame/s with a moving
/// vehicle, which crosses the frame quickly). While idle: one frame every
/// 30 s so a timeline never has long blank stretches.
#[derive(Debug, Default, Clone, Copy)]
pub struct PreviewFrameSampler {
    last_output: Option<f64>,
}

impl PreviewFrameSampler {
    pub fn should_write(
        &mut self,
        now: f64,
        motion: bool,
        active_objects: bool,
        fast_object: bool,
    ) -> bool {
        if !now.is_finite() {
            return false;
        }
        let Some(last) = self.last_output else {
            self.last_output = Some(now);
            return true;
        };
        let since = now - last;
        if since < 0.0 {
            // Clock went backwards: resynchronise instead of stalling.
            self.last_output = Some(now);
            return true;
        }
        let min_gap = if fast_object { 0.5 } else { 1.0 };
        let write = if active_objects || motion {
            since >= min_gap
        } else {
            since >= PREVIEW_KEEPALIVE_SECS
        };
        if write {
            self.last_output = Some(now);
        }
        write
    }
}

/// Camera ids become directory names: only accept the same safe charset the
/// rest of the server uses for ids.
fn is_safe_camera_dir(camera: &str) -> bool {
    !camera.is_empty()
        && camera.len() <= 64
        && camera != "."
        && camera != ".."
        && camera
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Parses a stored frame file name (`"<unix ms>.jpg"`).
pub fn parse_preview_frame_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(".jpg")?;
    if digits.is_empty() || digits.len() > 16 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// Picks `limit` entries spread evenly across `items` (first and last kept).
pub fn spread_evenly(items: &[u64], limit: usize) -> Vec<u64> {
    if limit == 0 || items.is_empty() {
        return Vec::new();
    }
    if items.len() <= limit {
        return items.to_vec();
    }
    if limit == 1 {
        return vec![items[items.len() / 2]];
    }
    let last = items.len() - 1;
    (0..limit).map(|i| items[i * last / (limit - 1)]).collect()
}

/// On-disk preview frames (`clips/preview_frames/<camera>/<unix ms>.jpg`) plus
/// an in-memory index of their timestamps per camera.
#[derive(Clone)]
pub struct PreviewFrameStore {
    root: PathBuf,
    index: Arc<RwLock<HashMap<String, BTreeSet<u64>>>>,
}

impl PreviewFrameStore {
    pub fn new(media_dir: &Path) -> Self {
        Self {
            root: media_dir.join("clips").join("preview_frames"),
            index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn camera_dir(&self, camera: &str) -> Option<PathBuf> {
        is_safe_camera_dir(camera).then(|| self.root.join(camera))
    }

    /// Path of a stored frame, if the camera id is safe.
    pub fn frame_path(&self, camera: &str, ts_ms: u64) -> Option<PathBuf> {
        Some(self.camera_dir(camera)?.join(format!("{ts_ms}.jpg")))
    }

    /// Writes a frame atomically (temp file + rename) and indexes it.
    pub fn write_frame(&self, camera: &str, ts_ms: u64, jpeg: &[u8]) -> std::io::Result<()> {
        let Some(dir) = self.camera_dir(camera) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe camera id",
            ));
        };
        std::fs::create_dir_all(&dir)?;
        let meta = std::fs::symlink_metadata(&dir)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preview dir is not a directory",
            ));
        }
        let final_path = dir.join(format!("{ts_ms}.jpg"));
        if let Ok(fmeta) = std::fs::symlink_metadata(&final_path)
            && (fmeta.file_type().is_symlink() || fmeta.is_dir())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to overwrite symlink or directory at preview frame path",
            ));
        }
        let nonce: u64 = rand::random();
        let tmp_path = dir.join(format!(".{ts_ms}.{nonce:016x}.tmp"));
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(nix::libc::O_NOFOLLOW);
            }
            let mut tmp_file = opts.open(&tmp_path)?;
            if let Err(err) = tmp_file.write_all(jpeg) {
                drop(tmp_file);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(err);
            }
        }
        if let Err(err) = std::fs::rename(&tmp_path, &final_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        self.insert(camera, ts_ms);
        Ok(())
    }

    pub fn insert(&self, camera: &str, ts_ms: u64) {
        if let Ok(mut idx) = self.index.write() {
            idx.entry(camera.to_string()).or_default().insert(ts_ms);
        }
    }

    pub fn contains(&self, camera: &str, ts_ms: u64) -> bool {
        self.index
            .read()
            .is_ok_and(|idx| idx.get(camera).is_some_and(|set| set.contains(&ts_ms)))
    }

    /// Frame timestamps within `[start_ms, end_ms]`, spread to at most `limit`.
    pub fn range(&self, camera: &str, start_ms: u64, end_ms: u64, limit: usize) -> Vec<u64> {
        if end_ms < start_ms {
            return Vec::new();
        }
        let hits: Vec<u64> = self
            .index
            .read()
            .map(|idx| {
                idx.get(camera)
                    .map(|set| set.range(start_ms..=end_ms).copied().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        spread_evenly(&hits, limit)
    }

    /// Rebuilds the index from disk (startup) and removes stray temp files.
    pub fn load_from_disk(&self) -> usize {
        let mut loaded = 0;
        let Ok(cams) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        let mut fresh: HashMap<String, BTreeSet<u64>> = HashMap::new();
        for cam in cams.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(cam.path()) else {
                continue;
            };
            let name = cam.file_name().to_string_lossy().into_owned();
            if !meta.is_dir() || meta.file_type().is_symlink() || !is_safe_camera_dir(&name) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(cam.path()) else {
                continue;
            };
            let set = fresh.entry(name).or_default();
            for f in files.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if fname.starts_with('.') && fname.ends_with(".tmp") {
                    let _ = std::fs::remove_file(f.path());
                } else if let Some(ts) = parse_preview_frame_name(&fname) {
                    set.insert(ts);
                    loaded += 1;
                }
            }
        }
        if let Ok(mut idx) = self.index.write() {
            *idx = fresh;
        }
        loaded
    }

    /// Deletes frames older than `now_ms - retention_ms`. Returns how many went.
    pub fn prune(&self, now_ms: u64, retention_ms: u64) -> usize {
        let cutoff = now_ms.saturating_sub(retention_ms);
        let stale: Vec<(String, Vec<u64>)> = match self.index.write() {
            Ok(mut idx) => idx
                .iter_mut()
                .map(|(cam, set)| {
                    let keep = set.split_off(&cutoff);
                    let old: Vec<u64> = std::mem::replace(set, keep).into_iter().collect();
                    (cam.clone(), old)
                })
                .filter(|(_, old)| !old.is_empty())
                .collect(),
            Err(_) => return 0,
        };
        let mut removed = 0;
        for (cam, old) in stale {
            for ts in old {
                if let Some(path) = self.frame_path(&cam, ts)
                    && std::fs::remove_file(&path).is_ok()
                {
                    removed += 1;
                }
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let n: u64 = rand::random();
        std::env::temp_dir().join(format!("ninjanvr-preview-{tag}-{n:016x}"))
    }

    #[test]
    fn sampler_writes_first_frame_then_throttles_by_activity() {
        let mut s = PreviewFrameSampler::default();
        assert!(
            s.should_write(100.0, false, false, false),
            "first frame is always kept"
        );
        assert!(
            !s.should_write(100.5, true, false, false),
            "motion: at most 1 fps"
        );
        assert!(s.should_write(101.0, true, false, false));
        assert!(
            !s.should_write(101.4, false, true, true),
            "vehicle: at most 2 fps"
        );
        assert!(s.should_write(101.5, false, true, true));
        assert!(
            !s.should_write(120.0, false, false, false),
            "idle: keepalive only"
        );
        assert!(
            s.should_write(131.5, false, false, false),
            "idle keepalive after 30 s"
        );
    }

    #[test]
    fn sampler_resyncs_on_clock_jumps_and_ignores_nan() {
        let mut s = PreviewFrameSampler::default();
        assert!(!s.should_write(f64::NAN, true, true, true));
        assert!(s.should_write(500.0, false, false, false));
        assert!(
            s.should_write(400.0, false, false, false),
            "backwards clock resyncs"
        );
        assert!(!s.should_write(400.2, true, false, false));
    }

    #[test]
    fn frame_names_and_camera_ids_are_strict() {
        assert_eq!(
            parse_preview_frame_name("1793984364123.jpg"),
            Some(1793984364123)
        );
        for bad in [
            "",
            ".jpg",
            "12a.jpg",
            "../1.jpg",
            "1.png",
            "12345678901234567.jpg",
            "-1.jpg",
        ] {
            assert_eq!(parse_preview_frame_name(bad), None, "{bad}");
        }
        let store = PreviewFrameStore::new(Path::new("/media/x"));
        assert!(store.frame_path("sala", 1).is_some());
        for bad in ["", "..", ".", "a/b", "a b", "../etc"] {
            assert!(store.frame_path(bad, 1).is_none(), "{bad}");
        }
    }

    #[test]
    fn spread_evenly_keeps_ends_and_respects_limit() {
        let items: Vec<u64> = (0..100).collect();
        let picked = spread_evenly(&items, 5);
        assert_eq!(picked, vec![0, 24, 49, 74, 99]);
        assert_eq!(spread_evenly(&items[..3], 5), vec![0, 1, 2]);
        assert_eq!(spread_evenly(&items, 1), vec![50]);
        assert!(spread_evenly(&items, 0).is_empty());
        assert!(spread_evenly(&[], 4).is_empty());
    }

    #[test]
    fn store_writes_indexes_ranges_reloads_and_prunes() {
        let media = tmp_root("store");
        let store = PreviewFrameStore::new(&media);
        for ts in [1_000u64, 2_000, 3_000, 9_000] {
            store.write_frame("sala", ts, b"jpg").unwrap();
        }
        store.write_frame("quarto", 2_500, b"jpg").unwrap();
        assert!(store.write_frame("../x", 1, b"jpg").is_err());

        assert_eq!(store.range("sala", 1_500, 3_000, 16), vec![2_000, 3_000]);
        assert!(store.range("sala", 3_000, 1_000, 16).is_empty());
        assert!(store.range("unknown", 0, u64::MAX, 16).is_empty());
        assert!(store.contains("quarto", 2_500));

        // A fresh store rebuilds its index from disk and drops stray temp files.
        std::fs::write(store.root().join("sala").join(".7.tmp"), b"x").unwrap();
        std::fs::write(
            store.root().join("sala").join(".7.0123456789abcdef.tmp"),
            b"x",
        )
        .unwrap();
        #[cfg(unix)]
        {
            let outside = media.join("outside.txt");
            std::fs::write(&outside, b"DO_NOT_OVERWRITE").unwrap();
            let sym_target = store.root().join("sala").join("5555.jpg");
            std::os::unix::fs::symlink(&outside, &sym_target).unwrap();
            assert!(
                store.write_frame("sala", 5555, b"evil_jpeg").is_err(),
                "write_frame must refuse to overwrite a symlinked frame target"
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"DO_NOT_OVERWRITE");
            let _ = std::fs::remove_file(&sym_target);
        }
        let reloaded = PreviewFrameStore::new(&media);
        assert_eq!(reloaded.load_from_disk(), 5);
        assert_eq!(
            reloaded.range("sala", 0, u64::MAX, 16),
            vec![1_000, 2_000, 3_000, 9_000]
        );
        assert!(!store.root().join("sala").join(".7.tmp").exists());

        // Pruning removes old files from disk and from the index (cutoff 3_000 is kept).
        assert_eq!(reloaded.prune(10_000, 7_000), 3);
        assert_eq!(reloaded.range("sala", 0, u64::MAX, 16), vec![3_000, 9_000]);
        assert!(!reloaded.contains("quarto", 2_500));
        assert!(!store.frame_path("sala", 1_000).unwrap().exists());
        assert!(store.frame_path("sala", 9_000).unwrap().exists());
        let _ = std::fs::remove_dir_all(&media);
    }
}
