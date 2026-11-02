use std::io::Cursor;

use anyhow::Result;
use image::{ImageFormat, Rgb, RgbImage};
use ninjanvr_config::MotionConfig;
use ninjanvr_media::YuvFrame;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x_min: u32,
    pub y_min: u32,
    pub x_max: u32,
    pub y_max: u32,
}

impl BoundingBox {
    pub fn new(x_min: u32, y_min: u32, x_max: u32, y_max: u32) -> Self {
        Self {
            x_min: x_min.min(x_max),
            y_min: y_min.min(y_max),
            x_max: x_max.max(x_min),
            y_max: y_max.max(y_min),
        }
    }

    pub fn width(&self) -> u32 {
        self.x_max.saturating_sub(self.x_min)
    }

    pub fn height(&self) -> u32 {
        self.y_max.saturating_sub(self.y_min)
    }

    pub fn area(&self) -> u32 {
        self.width() * self.height()
    }

    pub fn center(&self) -> (f32, f32) {
        (
            (self.x_min as f32 + self.x_max as f32) * 0.5,
            (self.y_min as f32 + self.y_max as f32) * 0.5,
        )
    }

    pub fn bottom_center(&self) -> (f32, f32) {
        (
            (self.x_min as f32 + self.x_max as f32) * 0.5,
            self.y_max as f32,
        )
    }

    pub fn intersects(&self, other: &Self) -> bool {
        self.x_min < other.x_max
            && self.x_max > other.x_min
            && self.y_min < other.y_max
            && self.y_max > other.y_min
    }

    pub fn inside(&self, container: &Self) -> bool {
        self.x_min >= container.x_min
            && self.y_min >= container.y_min
            && self.x_max <= container.x_max
            && self.y_max <= container.y_max
    }

    pub fn union(&self, other: &Self) -> Self {
        Self {
            x_min: self.x_min.min(other.x_min),
            y_min: self.y_min.min(other.y_min),
            x_max: self.x_max.max(other.x_max),
            y_max: self.y_max.max(other.y_max),
        }
    }

    pub fn iou(&self, other: &Self) -> f32 {
        let ix1 = self.x_min.max(other.x_min);
        let iy1 = self.y_min.max(other.y_min);
        let ix2 = self.x_max.min(other.x_max);
        let iy2 = self.y_max.min(other.y_max);
        if ix2 <= ix1 || iy2 <= iy1 {
            return 0.0;
        }
        let inter = ((ix2 - ix1) as f32) * ((iy2 - iy1) as f32);
        let union = (self.area() as f32) + (other.area() as f32) - inter;
        if union <= 0.0 { 0.0 } else { inter / union }
    }

    pub fn to_normalized(&self, frame_w: u32, frame_h: u32) -> [f64; 4] {
        let fw = frame_w.max(1) as f64;
        let fh = frame_h.max(1) as f64;
        [
            (self.x_min as f64) / fw,
            (self.y_min as f64) / fh,
            (self.width() as f64) / fw,
            (self.height() as f64) / fh,
        ]
    }

    /// Projects a bounding box from `(src_w, src_h)` (e.g., 640x360 detect substream)
    /// onto `(dst_w, dst_h)` (e.g., 1920x1080 or 3840x2160 main stream).
    pub fn project_to_resolution(
        &self,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> BoundingBox {
        let sw = src_w.max(1) as f32;
        let sh = src_h.max(1) as f32;
        let dw = dst_w.max(1) as f32;
        let dh = dst_h.max(1) as f32;
        let x_min = ((self.x_min as f32 / sw) * dw).round().clamp(0.0, dw) as u32;
        let y_min = ((self.y_min as f32 / sh) * dh).round().clamp(0.0, dh) as u32;
        let x_max = ((self.x_max as f32 / sw) * dw).round().clamp(0.0, dw) as u32;
        let y_max = ((self.y_max as f32 / sh) * dh).round().clamp(0.0, dh) as u32;
        Self::new(x_min, y_min, x_max, y_max)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PolygonMask {
    polygons: Vec<Vec<(f32, f32)>>,
}

impl PolygonMask {
    pub fn from_config_value(val: &Value, frame_w: u32, frame_h: u32) -> Self {
        let mut polygons = Vec::new();
        match val {
            Value::String(s) => {
                if let Some(poly) = Self::parse_coord_str(s, frame_w, frame_h) {
                    polygons.push(poly);
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    if let Some(s) = item.as_str()
                        && let Some(poly) = Self::parse_coord_str(s, frame_w, frame_h)
                    {
                        polygons.push(poly);
                    }
                }
            }
            Value::Object(map) => {
                for v in map.values() {
                    if let Some(s) = v.as_str() {
                        if let Some(poly) = Self::parse_coord_str(s, frame_w, frame_h) {
                            polygons.push(poly);
                        }
                    } else if let Some(coords) = v.get("coordinates").and_then(|c| c.as_str())
                        && let Some(poly) = Self::parse_coord_str(coords, frame_w, frame_h)
                    {
                        polygons.push(poly);
                    }
                }
            }
            _ => {}
        }
        Self { polygons }
    }

    fn parse_coord_str(s: &str, frame_w: u32, frame_h: u32) -> Option<Vec<(f32, f32)>> {
        let nums: Vec<f32> = s
            .split(',')
            .filter_map(|p| p.trim().parse::<f32>().ok())
            .collect();
        if nums.len() < 6 || !nums.len().is_multiple_of(2) {
            return None;
        }
        let is_normalized = nums.iter().all(|&v| (0.0..=1.0).contains(&v));
        let mut pts = Vec::with_capacity(nums.len() / 2);
        for &[px, py] in nums.as_chunks::<2>().0 {
            let x = if is_normalized {
                px * (frame_w as f32)
            } else {
                px
            };
            let y = if is_normalized {
                py * (frame_h as f32)
            } else {
                py
            };
            pts.push((x, y));
        }
        Some(pts)
    }

    pub fn is_empty(&self) -> bool {
        self.polygons.is_empty()
    }

    pub fn contains_point(&self, x: f32, y: f32) -> bool {
        self.polygons
            .iter()
            .any(|poly| point_in_polygon(x, y, poly))
    }
}

pub fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let intersect =
            ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / ((yj - yi) + 1e-7) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Zero-OpenCV Y-plane motion detector using O(1) 256-bin histogram contrast,
/// EMA background modeling, separable 3x3 morphological dilation, and Two-Pass Connected Components Labeling.
pub struct SimdMotionDetector {
    frame_w: u32,
    frame_h: u32,
    motion_w: u32,
    motion_h: u32,
    scale_x: f32,
    scale_y: f32,
    config: MotionConfig,
    masked_indices: Vec<usize>,
    avg_frame: Vec<f32>,
    avg_delta: Vec<f32>,
    work_gray: Vec<u8>,
    work_blurred: Vec<u8>,
    work_thresh: Vec<u8>,
    work_dilated: Vec<u8>,
    labels: Vec<u32>,
    frame_counter: u64,
    motion_frame_count: u32,
    smooth_min_v: f32,
    smooth_max_v: f32,
}

impl SimdMotionDetector {
    pub fn new(frame_w: u32, frame_h: u32, config: MotionConfig) -> Self {
        let motion_h = config.frame_height.clamp(32, frame_h.max(32));
        let motion_w =
            ((frame_w as u64 * motion_h as u64) / (frame_h.max(1) as u64)).max(32) as u32;
        let len = (motion_w * motion_h) as usize;
        let scale_x = (frame_w as f32) / (motion_w as f32);
        let scale_y = (frame_h as f32) / (motion_h as f32);

        let poly_mask = PolygonMask::from_config_value(&config.mask, frame_w, frame_h);
        let mut masked_indices = Vec::new();
        if !poly_mask.is_empty() {
            for y in 0..motion_h {
                for x in 0..motion_w {
                    let fx = (x as f32 + 0.5) * scale_x;
                    let fy = (y as f32 + 0.5) * scale_y;
                    if poly_mask.contains_point(fx, fy) {
                        masked_indices.push((y * motion_w + x) as usize);
                    }
                }
            }
        }

        Self {
            frame_w,
            frame_h,
            motion_w,
            motion_h,
            scale_x,
            scale_y,
            config,
            masked_indices,
            avg_frame: vec![0.0; len],
            avg_delta: vec![0.0; len],
            work_gray: vec![0; len],
            work_blurred: vec![0; len],
            work_thresh: vec![0; len],
            work_dilated: vec![0; len],
            labels: vec![0; len],
            frame_counter: 0,
            motion_frame_count: 0,
            smooth_min_v: 16.0,
            smooth_max_v: 235.0,
        }
    }

    pub fn detect(&mut self, frame: &YuvFrame) -> Vec<BoundingBox> {
        let y_plane = frame.y_plane();
        if y_plane.len() < (self.frame_w * self.frame_h) as usize {
            return Vec::new();
        }

        // 1. Box-averaging downscale of Y-plane into `work_gray` (suppresses sensor & H.264 pixel noise)
        let mw = self.motion_w as usize;
        let mh = self.motion_h as usize;
        let fw = self.frame_w as usize;
        let fh = self.frame_h as usize;

        for my in 0..mh {
            let sy0 = (my * fh) / mh;
            let sy1 = (((my + 1) * fh) / mh).max(sy0 + 1).min(fh);
            let sy_mid = (sy0 + sy1) / 2;
            let row_a = &y_plane[sy0 * fw..(sy0 + 1) * fw];
            let row_b = &y_plane[sy_mid * fw..(sy_mid + 1) * fw];
            let row_dst = &mut self.work_gray[my * mw..(my + 1) * mw];
            for (mx, dst) in row_dst.iter_mut().enumerate() {
                let sx0 = (mx * fw) / mw;
                let sx1 = (((mx + 1) * fw) / mw)
                    .saturating_sub(1)
                    .max(sx0)
                    .min(fw - 1);
                let sum = (row_a[sx0] as u32)
                    + (row_a[sx1] as u32)
                    + (row_b[sx0] as u32)
                    + (row_b[sx1] as u32);
                *dst = ((sum + 2) >> 2) as u8;
            }
        }

        // 2. 3x3 Gaussian blur (`[1, 2, 1; 2, 4, 2; 1, 2, 1] / 16`, matching Frigate's gaussian_filter sigma=1, radius=1)
        self.gaussian_blur_3x3();

        // 3. Apply motion mask first so masked pixels do not skew contrast stretching
        for &idx in &self.masked_indices {
            self.work_blurred[idx] = 255;
        }

        // 4. O(1) 256-bin histogram contrast stretching (P4..P96) with EMA-smoothed bounds and max gain cap
        if self.config.improve_contrast {
            let mut hist = [0u32; 256];
            for &px in &self.work_blurred {
                hist[px as usize] += 1;
            }
            let masked_len = self.masked_indices.len() as u32;
            hist[255] = hist[255].saturating_sub(masked_len);
            let total = (self.work_blurred.len() as u32)
                .saturating_sub(masked_len)
                .max(1);
            let p4_target = (total * 4) / 100;
            let p96_target = (total * 96) / 100;

            let mut acc = 0u32;
            let mut raw_min = 0u8;
            let mut raw_max = 255u8;
            for (val, &cnt) in hist.iter().enumerate() {
                acc += cnt;
                if acc >= p4_target {
                    raw_min = val as u8;
                    break;
                }
            }
            acc = 0;
            for (val, &cnt) in hist.iter().enumerate() {
                acc += cnt;
                if acc >= p96_target {
                    raw_max = val as u8;
                    break;
                }
            }

            if self.frame_counter == 0 {
                self.smooth_min_v = raw_min as f32;
                self.smooth_max_v = raw_max as f32;
            } else {
                self.smooth_min_v = 0.90 * self.smooth_min_v + 0.10 * (raw_min as f32);
                self.smooth_max_v = 0.90 * self.smooth_max_v + 0.10 * (raw_max as f32);
            }

            let min_v = self.smooth_min_v.round().clamp(0.0, 254.0) as u8;
            // Enforce a minimum span of 85 (~3x max gain) so dark/uniform rooms never amplify compression noise
            let max_v =
                (self.smooth_max_v.round().clamp(1.0, 255.0) as u8).max(min_v.saturating_add(85));
            if min_v < max_v {
                let span = (max_v - min_v) as u32;
                for px in &mut self.work_blurred {
                    let clamped = (*px).clamp(min_v, max_v);
                    *px = (((clamped - min_v) as u32 * 255) / span) as u8;
                }
            }
            for &idx in &self.masked_indices {
                self.work_blurred[idx] = 255;
            }
        }

        // 5. Warmup baseline for first 30 frames
        if self.frame_counter < 30 {
            let alpha = if self.frame_counter == 0 { 1.0 } else { 0.15 };
            for (avg, &px) in self.avg_frame.iter_mut().zip(self.work_blurred.iter()) {
                *avg = (1.0 - alpha) * (*avg) + alpha * (px as f32);
            }
            self.frame_counter += 1;
            return Vec::new();
        }
        self.frame_counter += 1;

        // 6. Compute frameDelta, EMA avg_delta, and threshold
        let delta_alpha = self.config.delta_alpha.clamp(0.01, 1.0);
        let thresh_val = self.config.threshold;
        let mut active_pixels = 0usize;

        for i in 0..self.work_blurred.len() {
            let px = self.work_blurred[i];
            let avg_u8 = self.avg_frame[i].round().clamp(0.0, 255.0) as u8;
            let frame_delta = px.abs_diff(avg_u8);
            self.avg_delta[i] =
                (1.0 - delta_alpha) * self.avg_delta[i] + delta_alpha * (frame_delta as f32);

            if frame_delta >= thresh_val {
                let avg_d = self.avg_delta[i].round().clamp(0.0, 255.0) as u8;
                if avg_d >= thresh_val {
                    self.work_thresh[i] = 255;
                    active_pixels += 1;
                } else {
                    self.work_thresh[i] = 0;
                }
            } else {
                self.work_thresh[i] = 0;
            }
        }

        // Lightning / IR cut-filter switch rejection
        let active_fraction = (active_pixels as f32) / (self.work_blurred.len().max(1) as f32);
        if active_fraction >= self.config.lightning_threshold {
            for (avg, &px) in self.avg_frame.iter_mut().zip(self.work_blurred.iter()) {
                *avg = px as f32;
            }
            self.avg_delta.fill(0.0);
            return Vec::new();
        }

        // 7. Separable 3x3 morphological dilation to bridge adjacent motion pixels
        self.dilate_3x3();

        // 8. Connected Components Labeling & BoundingBox extraction
        let motion_boxes = self.extract_connected_boxes();

        // 9. Update background model
        let f_alpha = self.config.frame_alpha.clamp(0.001, 0.5);
        if !motion_boxes.is_empty() {
            self.motion_frame_count += 1;
            if self.motion_frame_count >= 10 {
                for (avg, &px) in self.avg_frame.iter_mut().zip(self.work_blurred.iter()) {
                    *avg = (1.0 - f_alpha) * (*avg) + f_alpha * (px as f32);
                }
            }
        } else {
            for (avg, &px) in self.avg_frame.iter_mut().zip(self.work_blurred.iter()) {
                *avg = (1.0 - f_alpha) * (*avg) + f_alpha * (px as f32);
            }
            self.motion_frame_count = 0;
        }

        motion_boxes
    }

    fn gaussian_blur_3x3(&mut self) {
        let w = self.motion_w as usize;
        let h = self.motion_h as usize;
        for y in 0..h {
            let ym1 = y.saturating_sub(1);
            let yp1 = (y + 1).min(h - 1);
            for x in 0..w {
                let xm1 = x.saturating_sub(1);
                let xp1 = (x + 1).min(w - 1);
                let sum = (self.work_gray[ym1 * w + xm1] as u32)
                    + (self.work_gray[ym1 * w + x] as u32) * 2
                    + (self.work_gray[ym1 * w + xp1] as u32)
                    + (self.work_gray[y * w + xm1] as u32) * 2
                    + (self.work_gray[y * w + x] as u32) * 4
                    + (self.work_gray[y * w + xp1] as u32) * 2
                    + (self.work_gray[yp1 * w + xm1] as u32)
                    + (self.work_gray[yp1 * w + x] as u32) * 2
                    + (self.work_gray[yp1 * w + xp1] as u32);
                self.work_blurred[y * w + x] = ((sum + 8) >> 4) as u8;
            }
        }
    }

    fn dilate_3x3(&mut self) {
        let w = self.motion_w as usize;
        let h = self.motion_h as usize;
        self.work_dilated.fill(0);

        for y in 0..h {
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(h - 1);
            for x in 0..w {
                let x0 = x.saturating_sub(1);
                let x1 = (x + 1).min(w - 1);
                let mut hit = 0u8;
                'window: for wy in y0..=y1 {
                    let row = &self.work_thresh[wy * w..wy * w + w];
                    for &v in &row[x0..=x1] {
                        if v != 0 {
                            hit = 255;
                            break 'window;
                        }
                    }
                }
                self.work_dilated[y * w + x] = hit;
            }
        }
    }

    fn extract_connected_boxes(&mut self) -> Vec<BoundingBox> {
        let w = self.motion_w as usize;
        let h = self.motion_h as usize;
        self.labels.fill(0);

        let mut parent = vec![0u32];
        let mut next_label = 1u32;

        fn find_root(parent: &mut [u32], mut x: u32) -> u32 {
            while parent[x as usize] != x {
                let p = parent[x as usize];
                parent[x as usize] = parent[p as usize];
                x = p;
            }
            x
        }

        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                if self.work_dilated[idx] == 0 {
                    continue;
                }
                let up = if y > 0 {
                    self.labels[(y - 1) * w + x]
                } else {
                    0
                };
                let left = if x > 0 {
                    self.labels[y * w + (x - 1)]
                } else {
                    0
                };

                match (up, left) {
                    (0, 0) => {
                        let l = next_label;
                        next_label += 1;
                        parent.push(l);
                        self.labels[idx] = l;
                    }
                    (u, 0) => self.labels[idx] = u,
                    (0, l) => self.labels[idx] = l,
                    (u, l) => {
                        let ru = find_root(&mut parent, u);
                        let rl = find_root(&mut parent, l);
                        let root = ru.min(rl);
                        parent[ru as usize] = root;
                        parent[rl as usize] = root;
                        self.labels[idx] = root;
                    }
                }
            }
        }

        if next_label == 1 {
            return Vec::new();
        }

        #[derive(Clone, Copy)]
        struct CompStats {
            count: u32,
            x_min: u32,
            y_min: u32,
            x_max: u32,
            y_max: u32,
        }
        let mut stats: Vec<Option<CompStats>> = vec![None; next_label as usize];

        for y in 0..h {
            for x in 0..w {
                let l = self.labels[y * w + x];
                if l == 0 {
                    continue;
                }
                let root = find_root(&mut parent, l) as usize;
                let ux = x as u32;
                let uy = y as u32;
                match &mut stats[root] {
                    Some(st) => {
                        st.count += 1;
                        st.x_min = st.x_min.min(ux);
                        st.y_min = st.y_min.min(uy);
                        st.x_max = st.x_max.max(ux + 1);
                        st.y_max = st.y_max.max(uy + 1);
                    }
                    None => {
                        stats[root] = Some(CompStats {
                            count: 1,
                            x_min: ux,
                            y_min: uy,
                            x_max: ux + 1,
                            y_max: uy + 1,
                        });
                    }
                }
            }
        }

        let min_area = self.config.contour_area;
        let mut boxes = Vec::new();
        for st in stats.into_iter().flatten() {
            if st.count >= min_area {
                let fx1 = ((st.x_min as f32 * self.scale_x) as u32).min(self.frame_w - 1);
                let fy1 = ((st.y_min as f32 * self.scale_y) as u32).min(self.frame_h - 1);
                let fx2 = ((st.x_max as f32 * self.scale_x).ceil() as u32).min(self.frame_w);
                let fy2 = ((st.y_max as f32 * self.scale_y).ceil() as u32).min(self.frame_h);
                if fx2 > fx1 && fy2 > fy1 {
                    boxes.push(BoundingBox::new(fx1, fy1, fx2, fy2));
                }
            }
        }
        boxes
    }
}

/// Clusters motion boxes and tracked object boxes into square inference crop regions (`>= model_size`).
pub fn compute_inference_regions(
    frame_w: u32,
    frame_h: u32,
    model_size: u32,
    motion_boxes: &[BoundingBox],
    tracked_boxes: &[BoundingBox],
) -> Vec<BoundingBox> {
    let mut candidates: Vec<BoundingBox> =
        Vec::with_capacity(motion_boxes.len() + tracked_boxes.len());
    candidates.extend_from_slice(tracked_boxes);
    for mb in motion_boxes {
        if !candidates.iter().any(|c| mb.inside(c)) {
            candidates.push(*mb);
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    // Iteratively merge overlapping or proximate boxes
    let pad = (model_size / 8).max(16);
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..candidates.len() {
            for j in (i + 1)..candidates.len() {
                let a = candidates[i];
                let b = candidates[j];
                let expanded_a = BoundingBox::new(
                    a.x_min.saturating_sub(pad),
                    a.y_min.saturating_sub(pad),
                    (a.x_max + pad).min(frame_w),
                    (a.y_max + pad).min(frame_h),
                );
                if expanded_a.intersects(&b) {
                    candidates[i] = a.union(&b);
                    candidates.swap_remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }

    candidates.sort_by_key(|b| std::cmp::Reverse(b.width().max(b.height())));

    let min_dim = frame_w.min(frame_h).max(1);
    let mut regions = Vec::with_capacity(candidates.len());
    for c in candidates {
        let raw_size = ((c.width().max(c.height()) as f32) * 1.2) as u32;
        let size = (raw_size.max(model_size) / 4 * 4).clamp(model_size.min(min_dim), min_dim);
        let (cx, cy) = c.center();
        let half = (size / 2) as i32;
        let x0 = ((cx as i32) - half).clamp(0, (frame_w.saturating_sub(size)) as i32) as u32;
        let y0 = ((cy as i32) - half).clamp(0, (frame_h.saturating_sub(size)) as i32) as u32;
        let reg = BoundingBox::new(x0, y0, (x0 + size).min(frame_w), (y0 + size).min(frame_h));
        if !regions.iter().any(|r: &BoundingBox| reg.inside(r)) {
            regions.push(reg);
        }
    }
    regions
}

#[inline]
fn yuv_to_rgb_pixel(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = (y as i32) - 16;
    let d = (u as i32) - 128;
    let e = (v as i32) - 128;
    let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
    let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
    let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
    (r, g, b)
}

/// Crops `region` from a planar `YUV420p` frame and resizes into a `[model_h * model_w * 3]` `u8`
/// NHWC tensor in BGR or RGB pixel order.
pub fn crop_yuv420p_to_tensor_nhwc(
    frame: &YuvFrame,
    region: &BoundingBox,
    model_w: u32,
    model_h: u32,
    bgr_order: bool,
    out_tensor: &mut [u8],
) {
    let fw = frame.width as usize;
    let fh = frame.height as usize;
    let y_size = fw * fh;
    let uv_size = y_size / 4;
    if frame.data.len() < y_size + uv_size * 2 {
        out_tensor.fill(0);
        return;
    }

    let y_plane = &frame.data[..y_size];
    let u_plane = &frame.data[y_size..y_size + uv_size];
    let v_plane = &frame.data[y_size + uv_size..y_size + uv_size * 2];
    let uv_stride = fw / 2;

    let rw = region.width().max(1) as usize;
    let rh = region.height().max(1) as usize;
    let rx0 = region.x_min as usize;
    let ry0 = region.y_min as usize;
    let mw = model_w as usize;
    let mh = model_h as usize;

    for ty in 0..mh {
        let sy = (ry0 + (ty * rh) / mh).min(fh - 1);
        let uv_y = sy / 2;
        for tx in 0..mw {
            let sx = (rx0 + (tx * rw) / mw).min(fw - 1);
            let uv_x = sx / 2;
            let y_val = y_plane[sy * fw + sx];
            let u_val = u_plane[uv_y * uv_stride + uv_x];
            let v_val = v_plane[uv_y * uv_stride + uv_x];
            let (r, g, b) = yuv_to_rgb_pixel(y_val, u_val, v_val);
            let dst_idx = (ty * mw + tx) * 3;
            if dst_idx + 2 < out_tensor.len() {
                if bgr_order {
                    out_tensor[dst_idx] = b;
                    out_tensor[dst_idx + 1] = g;
                    out_tensor[dst_idx + 2] = r;
                } else {
                    out_tensor[dst_idx] = r;
                    out_tensor[dst_idx + 1] = g;
                    out_tensor[dst_idx + 2] = b;
                }
            }
        }
    }
}

/// Converts a `YuvFrame` (or a cropped sub-region) to an `RgbImage` (optionally downscaled to `max_dim`)
/// and draws bounding boxes.
pub fn yuv420p_to_rgb_image(
    frame: &YuvFrame,
    crop: Option<BoundingBox>,
    boxes: &[(BoundingBox, [u8; 3])],
) -> RgbImage {
    yuv420p_to_rgb_image_scaled(frame, crop, boxes, None)
}

pub fn yuv420p_to_rgb_image_scaled(
    frame: &YuvFrame,
    crop: Option<BoundingBox>,
    boxes: &[(BoundingBox, [u8; 3])],
    max_height: Option<u32>,
) -> RgbImage {
    let fw = frame.width as usize;
    let fh = frame.height as usize;
    let y_size = fw * fh;
    let uv_size = y_size / 4;
    if frame.data.len() < y_size + uv_size * 2 {
        return RgbImage::new(320, 180);
    }

    let region = crop.unwrap_or_else(|| BoundingBox::new(0, 0, frame.width, frame.height));
    let rw = region.width().max(1);
    let rh = region.height().max(1);
    let (out_w, out_h) = match max_height {
        Some(mh) if rh > mh && mh > 0 => {
            let ow = ((rw as u64 * mh as u64) / (rh as u64)).max(1) as u32;
            (ow, mh)
        }
        _ => (rw, rh),
    };

    let mut img = RgbImage::new(out_w, out_h);

    let y_plane = &frame.data[..y_size];
    let u_plane = &frame.data[y_size..y_size + uv_size];
    let v_plane = &frame.data[y_size + uv_size..y_size + uv_size * 2];
    let uv_stride = fw / 2;

    for dy in 0..out_h {
        let sy =
            (region.y_min as usize + (dy as usize * rh as usize) / (out_h as usize)).min(fh - 1);
        let uv_y = sy / 2;
        for dx in 0..out_w {
            let sx = (region.x_min as usize + (dx as usize * rw as usize) / (out_w as usize))
                .min(fw - 1);
            let uv_x = sx / 2;
            let (r, g, b) = yuv_to_rgb_pixel(
                y_plane[sy * fw + sx],
                u_plane[uv_y * uv_stride + uv_x],
                v_plane[uv_y * uv_stride + uv_x],
            );
            img.put_pixel(dx, dy, Rgb([r, g, b]));
        }
    }

    // Draw 2px bounding boxes scaled to (out_w, out_h)
    for (bbox, color) in boxes {
        let rx1 = bbox.x_min.saturating_sub(region.x_min);
        let ry1 = bbox.y_min.saturating_sub(region.y_min);
        let rx2 = bbox.x_max.saturating_sub(region.x_min);
        let ry2 = bbox.y_max.saturating_sub(region.y_min);
        let x1 = ((rx1 as u64 * out_w as u64) / (rw as u64)).min((out_w - 1) as u64) as u32;
        let y1 = ((ry1 as u64 * out_h as u64) / (rh as u64)).min((out_h - 1) as u64) as u32;
        let x2 = ((rx2 as u64 * out_w as u64) / (rw as u64)).min((out_w - 1) as u64) as u32;
        let y2 = ((ry2 as u64 * out_h as u64) / (rh as u64)).min((out_h - 1) as u64) as u32;
        let rgb = Rgb(*color);
        for x in x1..=x2 {
            for t in 0..2 {
                img.put_pixel(x, (y1 + t).min(out_h - 1), rgb);
                img.put_pixel(x, y2.saturating_sub(t), rgb);
            }
        }
        for y in y1..=y2 {
            for t in 0..2 {
                img.put_pixel((x1 + t).min(out_w - 1), y, rgb);
                img.put_pixel(x2.saturating_sub(t), y, rgb);
            }
        }
    }

    img
}

pub fn encode_image_webp(img: &RgbImage) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    img.write_to(&mut cursor, ImageFormat::WebP)?;
    Ok(cursor.into_inner())
}

pub fn encode_image_jpeg(img: &RgbImage) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    img.write_to(&mut cursor, ImageFormat::Jpeg)?;
    Ok(cursor.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameIntegrityReport {
    pub y_variance: f32,
    pub uniform_ratio: f32,
    pub is_tampered_or_blind: bool,
}

/// Evaluates spatial luminance variance, modal pixel uniformity, and edge gradient energy on a YUV420p
/// frame (excluding top/bottom 12% OSD timestamp margins) to detect camera tampering (hand/cloth/tape
/// occlusion, spray paint, IR lens washout) or solid-color encoder hangs.
pub fn check_frame_tamper(yuv_data: &[u8], width: u32, height: u32) -> FrameIntegrityReport {
    let w = width as usize;
    let h = height as usize;
    let y_len = w.saturating_mul(h);
    if y_len == 0 || yuv_data.len() < y_len {
        return FrameIntegrityReport {
            y_variance: 0.0,
            uniform_ratio: 1.0,
            is_tampered_or_blind: true,
        };
    }
    let y_plane = &yuv_data[..y_len];
    let row_start = (h * 12) / 100;
    let row_end = (h * 88) / 100;
    let slice_start = (row_start * w).min(y_len.saturating_sub(1));
    let slice_end = (row_end * w).max(slice_start + 1).min(y_len);
    let roi = &y_plane[slice_start..slice_end];
    let step = (roi.len() / 2048).max(1);
    let mut hist = [0u32; 256];
    let mut sum: f64 = 0.0;
    let mut sum_sq: f64 = 0.0;
    let mut grad_sum: u64 = 0;
    let mut count: u32 = 0;

    let mut idx = 0usize;
    while idx < roi.len() {
        let v = roi[idx];
        hist[v as usize] += 1;
        let vf = v as f64;
        sum += vf;
        sum_sq += vf * vf;
        if idx + 1 < roi.len() {
            grad_sum += (v as i32 - roi[idx + 1] as i32).unsigned_abs() as u64;
        }
        count += 1;
        idx += step;
    }

    if count == 0 {
        return FrameIntegrityReport {
            y_variance: 0.0,
            uniform_ratio: 1.0,
            is_tampered_or_blind: true,
        };
    }

    let mean = sum / (count as f64);
    let variance = ((sum_sq / (count as f64)) - mean * mean).max(0.0) as f32;
    let edge_gradient = (grad_sum as f32) / (count as f32);

    let mut modal_val = 0usize;
    let mut modal_count = 0u32;
    for (val, &c) in hist.iter().enumerate() {
        if c > modal_count {
            modal_count = c;
            modal_val = val;
        }
    }

    let low = modal_val.saturating_sub(4);
    let high = (modal_val + 4).min(255);
    let near_modal: u32 = hist[low..=high].iter().sum();
    let uniform_ratio = (near_modal as f32) / (count as f32);

    let low16 = modal_val.saturating_sub(16);
    let high16 = (modal_val + 16).min(255);
    let near_modal_16: u32 = hist[low16..=high16].iter().sum();
    let uniform_ratio_16 = (near_modal_16 as f32) / (count as f32);

    let is_tampered_or_blind = uniform_ratio >= 0.85
        || variance < 2.0
        || (edge_gradient < 1.8 && uniform_ratio_16 >= 0.55 && variance < 450.0);

    FrameIntegrityReport {
        y_variance: variance,
        uniform_ratio: uniform_ratio.max(if is_tampered_or_blind {
            uniform_ratio_16
        } else {
            uniform_ratio
        }),
        is_tampered_or_blind,
    }
}

/// Detects Infrared (IR) / monochrome night mode directly from a YUV420p buffer's `U` and `V`
/// chroma planes with near-zero CPU overhead. Returns `true` when the mean chroma deviation
/// `|U - 128| + |V - 128| < 3.0`.
pub fn is_monochrome_ir(yuv_data: &[u8], width: u32, height: u32) -> bool {
    let y_len = (width as usize).saturating_mul(height as usize);
    let uv_len = y_len / 4;
    if uv_len == 0 || yuv_data.len() < y_len + uv_len * 2 {
        return false;
    }
    let u_plane = &yuv_data[y_len..y_len + uv_len];
    let v_plane = &yuv_data[y_len + uv_len..y_len + uv_len * 2];
    let step = (uv_len / 512).max(1);
    let mut total_dev: u64 = 0;
    let mut samples: u64 = 0;
    let mut idx = 0usize;
    while idx < uv_len {
        let u_dev = (u_plane[idx] as i32 - 128).unsigned_abs() as u64;
        let v_dev = (v_plane[idx] as i32 - 128).unsigned_abs() as u64;
        total_dev += u_dev + v_dev;
        samples += 1;
        idx += step;
    }
    if samples == 0 {
        return false;
    }
    (total_dev as f64 / samples as f64) < 3.0
}

/// Extracts a semantic dominant color attribute tag (`"color:white"`, `"color:black"`, `"color:gray"`,
/// `"color:red"`, `"color:blue"`, `"color:green"`, `"color:yellow"`) from a YUV420p bounding box crop.
pub fn classify_dominant_color_tag(
    yuv_data: &[u8],
    width: u32,
    height: u32,
    bbox: BoundingBox,
) -> String {
    let fw = width as usize;
    let fh = height as usize;
    let y_size = fw.saturating_mul(fh);
    let uv_size = y_size / 4;
    if fw == 0 || fh == 0 || yuv_data.len() < y_size + uv_size * 2 {
        return "color:gray".to_string();
    }

    let y_plane = &yuv_data[..y_size];
    let u_plane = &yuv_data[y_size..y_size + uv_size];
    let v_plane = &yuv_data[y_size + uv_size..y_size + uv_size * 2];
    let uv_stride = fw / 2;

    let x1 = (bbox.x_min as usize).min(fw - 1);
    let y1 = (bbox.y_min as usize).min(fh - 1);
    let x2 = (bbox.x_max as usize).clamp(x1 + 1, fw);
    let y2 = (bbox.y_max as usize).clamp(y1 + 1, fh);

    let mut r_sum = 0u64;
    let mut g_sum = 0u64;
    let mut b_sum = 0u64;
    let mut count = 0u64;
    let step_y = ((y2 - y1) / 16).max(1);
    let step_x = ((x2 - x1) / 16).max(1);

    let mut sy = y1;
    while sy < y2 {
        let uv_y = (sy / 2).min(fh / 2 - 1);
        let mut sx = x1;
        while sx < x2 {
            let uv_x = (sx / 2).min(uv_stride.saturating_sub(1));
            let (r, g, b) = yuv_to_rgb_pixel(
                y_plane[sy * fw + sx],
                u_plane[uv_y * uv_stride + uv_x],
                v_plane[uv_y * uv_stride + uv_x],
            );
            r_sum += r as u64;
            g_sum += g as u64;
            b_sum += b as u64;
            count += 1;
            sx += step_x;
        }
        sy += step_y;
    }

    if count == 0 {
        return "color:gray".to_string();
    }

    let r = (r_sum / count) as i32;
    let g = (g_sum / count) as i32;
    let b = (b_sum / count) as i32;
    let max_c = r.max(g).max(b);
    let min_c = r.min(g).min(b);
    let luma = (r * 299 + g * 587 + b * 114) / 1000;

    if max_c - min_c < 28 {
        if luma > 185 {
            "color:white".to_string()
        } else if luma < 55 {
            "color:black".to_string()
        } else {
            "color:gray".to_string()
        }
    } else if r > g + 35 && r > b + 35 {
        "color:red".to_string()
    } else if b > r + 35 && b > g + 25 {
        "color:blue".to_string()
    } else if g > r + 30 && g > b + 30 {
        "color:green".to_string()
    } else if r > 160 && g > 140 && b < 95 {
        "color:yellow".to_string()
    } else {
        "color:gray".to_string()
    }
}

/// Permanently redacts all pixels inside `mask` on `img` with matte tactical dark `[12, 14, 18]`.
pub fn apply_privacy_mask_rgb(img: &mut RgbImage, mask: &PolygonMask) {
    if mask.is_empty() {
        return;
    }
    let w = img.width();
    let h = img.height();
    let redact_px = Rgb([12, 14, 18]);
    for y in 0..h {
        let fy = y as f32;
        for x in 0..w {
            if mask.contains_point(x as f32, fy) {
                img.put_pixel(x, y, redact_px);
            }
        }
    }
}

/// Parses normalized or pixel `privacy_mask` polygon coordinates against `(img.width(), img.height())`
/// and redacts all interior pixels to `[12, 14, 18]`.
pub fn apply_privacy_masks_from_value(img: &mut RgbImage, mask_val: &Value) {
    if mask_val.is_null() {
        return;
    }
    let mask = PolygonMask::from_config_value(mask_val, img.width(), img.height());
    apply_privacy_mask_rgb(img, &mask);
}

/// Draws 2px bounding boxes onto an existing `RgbImage` (e.g. a high-resolution main-stream frame).
pub fn annotate_rgb_image_with_boxes(img: &mut RgbImage, boxes: &[(BoundingBox, [u8; 3])]) {
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return;
    }
    for (bbox, color) in boxes {
        let x1 = bbox.x_min.min(w - 1);
        let y1 = bbox.y_min.min(h - 1);
        let x2 = bbox.x_max.min(w - 1);
        let y2 = bbox.y_max.min(h - 1);
        let rgb = Rgb(*color);
        for x in x1..=x2 {
            for t in 0..2 {
                img.put_pixel(x, (y1 + t).min(h - 1), rgb);
                img.put_pixel(x, y2.saturating_sub(t), rgb);
            }
        }
        for y in y1..=y2 {
            for t in 0..2 {
                img.put_pixel((x1 + t).min(w - 1), y, rgb);
                img.put_pixel(x2.saturating_sub(t), y, rgb);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn simd_motion_detector_detects_moving_object_after_warmup() {
        let cfg = MotionConfig::default();
        let mut detector = SimdMotionDetector::new(320, 180, cfg);
        let frame_bytes = vec![64u8; 320 * 180 * 3 / 2];
        let bg_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: 320,
            height: 180,
            data: Arc::new(frame_bytes.clone()),
        };

        for _ in 0..32 {
            assert!(detector.detect(&bg_frame).is_empty());
        }

        // Inject a bright moving block at (100..160, 50..120) in Y-plane
        let mut active_bytes = frame_bytes;
        for y in 50..120 {
            for x in 100..160 {
                active_bytes[y * 320 + x] = 230;
            }
        }
        let motion_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 2.0,
            width: 320,
            height: 180,
            data: Arc::new(active_bytes),
        };

        // Trigger two consecutive frames so avg_delta crosses threshold
        let _ = detector.detect(&motion_frame);
        let boxes = detector.detect(&motion_frame);
        assert!(!boxes.is_empty());
        let regions = compute_inference_regions(320, 180, 160, &boxes, &[]);
        assert!(!regions.is_empty());
    }

    #[test]
    fn polygon_mask_ray_casting_works() {
        let poly = PolygonMask::from_config_value(
            &serde_json::json!("0.1,0.1,0.5,0.1,0.5,0.5,0.1,0.5"),
            100,
            100,
        );
        assert!(poly.contains_point(25.0, 25.0));
        assert!(!poly.contains_point(75.0, 75.0));
    }

    #[test]
    fn bounding_box_projects_accurately_from_substream_to_1080p_and_4k() {
        let sub_box = BoundingBox::new(160, 90, 320, 270);
        let hd_box = sub_box.project_to_resolution(640, 360, 1920, 1080);
        assert_eq!(hd_box, BoundingBox::new(480, 270, 960, 810));

        let uhd_box = sub_box.project_to_resolution(640, 360, 3840, 2160);
        assert_eq!(uhd_box, BoundingBox::new(960, 540, 1920, 1620));
    }

    #[test]
    fn monochrome_ir_detector_distinguishes_color_vs_grayscale_yuv420p() {
        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;

        // Grayscale IR frame: U = 128, V = 128
        let mut ir_buf = vec![90u8; y_len + uv_len * 2];
        ir_buf[y_len..].fill(128);
        assert!(is_monochrome_ir(&ir_buf, w, h));

        // Color daytime frame: U = 96, V = 165
        let mut color_buf = vec![140u8; y_len + uv_len * 2];
        color_buf[y_len..y_len + uv_len].fill(96);
        color_buf[y_len + uv_len..].fill(165);
        assert!(!is_monochrome_ir(&color_buf, w, h));
    }

    #[test]
    fn check_frame_tamper_detects_solid_blind_feed_and_passes_structured_night_scene() {
        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;

        // 1. Solid black / spray-painted occluded frame
        let blind_buf = vec![16u8; y_len + uv_len * 2];
        let rep_blind = check_frame_tamper(&blind_buf, w, h);
        assert!(rep_blind.is_tampered_or_blind);
        assert!(rep_blind.uniform_ratio >= 0.99);

        // 2. Normal dark night scene with visible spatial structure (buildings, streetlamp gradient)
        let mut night_scene = vec![128u8; y_len + uv_len * 2];
        for (i, px) in night_scene.iter_mut().enumerate().take(y_len) {
            *px = ((i * 17) % 90 + 15) as u8;
        }
        let rep_normal = check_frame_tamper(&night_scene, w, h);
        assert!(!rep_normal.is_tampered_or_blind);
        assert!(rep_normal.y_variance > 20.0);
        assert!(rep_normal.uniform_ratio < 0.30);
    }

    #[test]
    fn classify_dominant_color_tag_identifies_white_black_and_red_crops() {
        let w = 64u32;
        let h = 64u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let bbox = BoundingBox::new(8, 8, 56, 56);

        // Bright white frame: Y=235, U=128, V=128
        let mut white_buf = vec![235u8; y_len + uv_len * 2];
        white_buf[y_len..].fill(128);
        assert_eq!(
            classify_dominant_color_tag(&white_buf, w, h, bbox),
            "color:white"
        );

        // Dark black frame: Y=20, U=128, V=128
        let mut black_buf = vec![20u8; y_len + uv_len * 2];
        black_buf[y_len..].fill(128);
        assert_eq!(
            classify_dominant_color_tag(&black_buf, w, h, bbox),
            "color:black"
        );

        // Saturated red frame: Y=90, U=90, V=240
        let mut red_buf = vec![90u8; y_len + uv_len * 2];
        red_buf[y_len..y_len + uv_len].fill(90);
        red_buf[y_len + uv_len..].fill(240);
        assert_eq!(
            classify_dominant_color_tag(&red_buf, w, h, bbox),
            "color:red"
        );
    }

    #[test]
    fn apply_privacy_mask_rgb_redacts_polygon_interior_to_matte_black() {
        let mut img = RgbImage::from_pixel(100, 100, Rgb([240, 240, 240]));
        let mask_val = serde_json::json!(["0.200,0.200,0.800,0.200,0.800,0.800,0.200,0.800"]);
        apply_privacy_masks_from_value(&mut img, &mask_val);

        // Interior pixel (50, 50) is permanently redacted to [12, 14, 18]
        assert_eq!(*img.get_pixel(50, 50), Rgb([12, 14, 18]));
        // Exterior pixel (5, 5) remains untouched [240, 240, 240]
        assert_eq!(*img.get_pixel(5, 5), Rgb([240, 240, 240]));
    }
}
