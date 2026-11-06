#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Cursor;

use anyhow::Result;
pub use image::RgbImage;
use image::{ImageFormat, Rgb};
use ninjanvr_config::{MotionConfig, StateRoiConfig};
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
        self.width().saturating_mul(self.height())
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
        self.x_min < self.x_max
            && self.y_min < self.y_max
            && other.x_min < other.x_max
            && other.y_min < other.y_max
            && self.x_min < other.x_max
            && self.x_max > other.x_min
            && self.y_min < other.y_max
            && self.y_max > other.y_min
    }

    pub fn inside(&self, container: &Self) -> bool {
        let s_xmin = self.x_min.min(self.x_max);
        let s_ymin = self.y_min.min(self.y_max);
        let s_xmax = self.x_max.max(self.x_min);
        let s_ymax = self.y_max.max(self.y_min);
        let c_xmin = container.x_min.min(container.x_max);
        let c_ymin = container.y_min.min(container.y_max);
        let c_xmax = container.x_max.max(container.x_min);
        let c_ymax = container.y_max.max(container.y_min);
        s_xmin >= c_xmin && s_ymin >= c_ymin && s_xmax <= c_xmax && s_ymax <= c_ymax
    }

    pub fn union(&self, other: &Self) -> Self {
        let a = Self::new(self.x_min, self.y_min, self.x_max, self.y_max);
        let b = Self::new(other.x_min, other.y_min, other.x_max, other.y_max);
        Self {
            x_min: a.x_min.min(b.x_min),
            y_min: a.y_min.min(b.y_min),
            x_max: a.x_max.max(b.x_max),
            y_max: a.y_max.max(b.y_max),
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
        let inter = ((ix2 - ix1) as f64) * ((iy2 - iy1) as f64);
        let self_area = (self.width() as f64) * (self.height() as f64);
        let other_area = (other.width() as f64) * (other.height() as f64);
        let union = self_area + other_area - inter;
        if union <= 0.0 {
            0.0
        } else {
            ((inter / union) as f32).clamp(0.0, 1.0)
        }
    }

    pub fn intersection_over_min(&self, other: &Self) -> f32 {
        let ix1 = self.x_min.max(other.x_min);
        let iy1 = self.y_min.max(other.y_min);
        let ix2 = self.x_max.min(other.x_max);
        let iy2 = self.y_max.min(other.y_max);
        if ix2 <= ix1 || iy2 <= iy1 {
            return 0.0;
        }
        let inter = ((ix2 - ix1) as f64) * ((iy2 - iy1) as f64);
        let self_area = (self.width() as f64) * (self.height() as f64);
        let other_area = (other.width() as f64) * (other.height() as f64);
        let min_area = self_area.min(other_area);
        if min_area <= 0.0 {
            0.0
        } else {
            ((inter / min_area) as f32).clamp(0.0, 1.0)
        }
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
                    if let Some(s) = item.as_str() {
                        if let Some(poly) = Self::parse_coord_str(s, frame_w, frame_h) {
                            polygons.push(poly);
                        }
                    } else if let Some(coords) = item.get("coordinates").and_then(|c| c.as_str())
                        && let Some(poly) = Self::parse_coord_str(coords, frame_w, frame_h)
                    {
                        polygons.push(poly);
                    }
                }
            }
            Value::Object(map) => {
                if let Some(coords) = map.get("coordinates").and_then(|c| c.as_str())
                    && let Some(poly) = Self::parse_coord_str(coords, frame_w, frame_h)
                {
                    polygons.push(poly);
                }
                for (k, v) in map {
                    if k == "coordinates" || k == "mode" || k == "style" {
                        continue;
                    }
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
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut nums = Vec::new();
        for part in trimmed.split(',') {
            let v = part.trim().parse::<f32>().ok()?;
            if !v.is_finite() {
                return None;
            }
            nums.push(v);
        }
        if nums.len() < 6 || !nums.len().is_multiple_of(2) {
            return None;
        }
        let is_normalized = nums.iter().all(|&v| (0.0..=1.0).contains(&v));
        let fw = frame_w.max(1) as f32;
        let fh = frame_h.max(1) as f32;
        let mut pts = Vec::with_capacity(nums.len() / 2);
        for &[px, py] in nums.as_chunks::<2>().0 {
            let x = if is_normalized { px * fw } else { px };
            let y = if is_normalized { py * fh } else { py };
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            pts.push((x, y));
        }
        if polygon_signed_area2(&pts).abs() <= 1e-5 {
            return None;
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

fn polygon_signed_area2(poly: &[(f32, f32)]) -> f32 {
    if poly.len() < 3 {
        return 0.0;
    }
    let mut area2 = 0.0f32;
    for i in 0..poly.len() {
        let (x1, y1) = poly[i];
        let (x2, y2) = poly[(i + 1) % poly.len()];
        area2 += x1 * y2 - x2 * y1;
    }
    if area2.is_finite() { area2 } else { 0.0 }
}

pub fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    if !x.is_finite()
        || !y.is_finite()
        || poly.len() < 3
        || poly
            .iter()
            .any(|(px, py)| !px.is_finite() || !py.is_finite())
    {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let dy = yj - yi;
        let intersect = ((yi > y) != (yj > y)) && dy != 0.0 && (x < (xj - xi) * (y - yi) / dy + xi);
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
    masked_bitmap: Vec<bool>,
    osd_tick_counts: Vec<u16>,
    auto_masked_count: usize,
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
        let safe_fw = frame_w.max(1);
        let safe_fh = frame_h.max(1);
        let max_motion_h = safe_fh.clamp(32, 1080);
        let motion_h = config.frame_height.clamp(32, max_motion_h);
        let motion_w =
            ((safe_fw as u64 * motion_h as u64) / (safe_fh as u64)).clamp(32, 1920) as u32;
        let len = (motion_w as usize) * (motion_h as usize);
        let scale_x = (safe_fw as f32) / (motion_w as f32);
        let scale_y = (safe_fh as f32) / (motion_h as f32);

        let poly_mask = PolygonMask::from_config_value(&config.mask, safe_fw, safe_fh);
        let mut masked_indices = Vec::new();
        let mut masked_bitmap = vec![false; len];
        if !poly_mask.is_empty() {
            for y in 0..motion_h {
                for x in 0..motion_w {
                    let fx = (x as f32 + 0.5) * scale_x;
                    let fy = (y as f32 + 0.5) * scale_y;
                    if poly_mask.contains_point(fx, fy) {
                        let idx = (y * motion_w + x) as usize;
                        masked_indices.push(idx);
                        masked_bitmap[idx] = true;
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
            masked_bitmap,
            osd_tick_counts: vec![0; len],
            auto_masked_count: 0,
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

    /// Returns the number of motion-grid cells automatically masked due to repetitive OSD clock ticks.
    pub fn auto_masked_cell_count(&self) -> usize {
        self.auto_masked_count
    }

    pub fn detect(&mut self, frame: &YuvFrame) -> Vec<BoundingBox> {
        if !self.config.enabled
            || self.frame_w == 0
            || self.frame_h == 0
            || frame.width < self.frame_w
            || frame.height < self.frame_h
        {
            return Vec::new();
        }
        let mw = self.motion_w as usize;
        let mh = self.motion_h as usize;
        let src_w = frame.width as usize;
        let src_h = frame.height as usize;
        let Some(expected_y_len) = src_w.checked_mul(src_h) else {
            return Vec::new();
        };
        let y_plane = frame.y_plane();
        if y_plane.len() < expected_y_len {
            return Vec::new();
        }

        // 1. Box-averaging downscale of Y-plane into `work_gray` (suppresses sensor & H.264 pixel noise)

        for my in 0..mh {
            let sy0 = (my * src_h) / mh;
            let sy1 = (((my + 1) * src_h) / mh).max(sy0 + 1).min(src_h);
            let sy_mid = (sy0 + sy1) / 2;
            let row_a = &y_plane[sy0 * src_w..(sy0 + 1) * src_w];
            let row_b = &y_plane[sy_mid * src_w..(sy_mid + 1) * src_w];
            let row_dst = &mut self.work_gray[my * mw..(my + 1) * mw];
            for (mx, dst) in row_dst.iter_mut().enumerate() {
                let sx0 = (mx * src_w) / mw;
                let sx1 = (((mx + 1) * src_w) / mw)
                    .saturating_sub(1)
                    .max(sx0)
                    .min(src_w - 1);
                let sum = (row_a[sx0] as u32)
                    + (row_a[sx1] as u32)
                    + (row_b[sx0] as u32)
                    + (row_b[sx1] as u32);
                *dst = ((sum + 2) >> 2) as u8;
            }
        }

        // 2. 3x3 Gaussian blur (`[1, 2, 1; 2, 4, 2; 1, 2, 1] / 16`, sigma=1, radius=1)
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
            let unmasked_total = (self.work_blurred.len() as u32).saturating_sub(masked_len);
            if unmasked_total > 0 {
                let total = unmasked_total.max(1);
                let p4_target = ((total * 4) / 100).max(1);
                let p96_target = ((total * 96) / 100).max(1);

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
                    let raw_fa = if self.config.frame_alpha.is_finite() {
                        self.config.frame_alpha
                    } else {
                        0.02
                    };
                    let c_alpha = raw_fa.clamp(0.005, 0.25);
                    self.smooth_min_v =
                        (1.0 - c_alpha) * self.smooth_min_v + c_alpha * (raw_min as f32);
                    self.smooth_max_v =
                        (1.0 - c_alpha) * self.smooth_max_v + c_alpha * (raw_max as f32);
                }

                let min_v = self.smooth_min_v.round().clamp(0.0, 170.0) as u8;
                // Enforce a minimum span of 85 (~3x max gain) so dark/bright/uniform rooms never amplify compression noise
                let max_v = (self.smooth_max_v.round().clamp(85.0, 255.0) as u8)
                    .max(min_v.saturating_add(85));
                if min_v < max_v {
                    let span = (max_v - min_v) as u32;
                    for px in &mut self.work_blurred {
                        let clamped = (*px).clamp(min_v, max_v);
                        *px = (((clamped - min_v) as u32 * 255) / span) as u8;
                    }
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
        let raw_da = if self.config.delta_alpha.is_finite() {
            self.config.delta_alpha
        } else {
            0.2
        };
        let delta_alpha = raw_da.clamp(0.01, 1.0);
        let thresh_val = self.config.threshold.max(1);
        let mut active_pixels = 0usize;

        for i in 0..self.work_blurred.len() {
            if self.masked_bitmap[i] {
                self.avg_frame[i] = 255.0;
                self.avg_delta[i] = 0.0;
                self.work_thresh[i] = 0;
                continue;
            }
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

        // Lightning / IR cut-filter switch rejection (computed over unmasked motion cells)
        let unmasked_len = self
            .work_blurred
            .len()
            .saturating_sub(self.masked_indices.len());
        let active_fraction = if unmasked_len > 0 {
            (active_pixels as f32) / (unmasked_len as f32)
        } else {
            0.0
        };
        let lightning_thresh = if self.config.lightning_threshold.is_finite() {
            self.config.lightning_threshold.clamp(0.05, 1.0)
        } else {
            0.8
        };
        if active_fraction >= lightning_thresh {
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

        // 9. Update background model (`delta_alpha` = 0.2 default, so static changes settle in ~1.5s)
        let bg_alpha = raw_da.clamp(0.05, 0.5);
        for (i, (avg, &px)) in self
            .avg_frame
            .iter_mut()
            .zip(self.work_blurred.iter())
            .enumerate()
        {
            if self.masked_bitmap[i] {
                *avg = 255.0;
            } else {
                *avg = (1.0 - bg_alpha) * (*avg) + bg_alpha * (px as f32);
            }
        }
        if !motion_boxes.is_empty() {
            self.motion_frame_count += 1;
        } else {
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
        let frame_area = self.frame_w.saturating_mul(self.frame_h).max(1);
        let mut boxes = Vec::new();
        for st in stats.into_iter().flatten() {
            if st.count >= min_area {
                let fx1 = ((st.x_min as f32 * self.scale_x) as u32).min(self.frame_w - 1);
                let fy1 = ((st.y_min as f32 * self.scale_y) as u32).min(self.frame_h - 1);
                let fx2 = ((st.x_max as f32 * self.scale_x).ceil() as u32).min(self.frame_w);
                let fy2 = ((st.y_max as f32 * self.scale_y).ceil() as u32).min(self.frame_h);
                if fx2 > fx1 && fy2 > fy1 {
                    let b = BoundingBox::new(fx1, fy1, fx2, fy2);
                    // Suppress isolated 1-second camera OSD clock digit ticks in top/bottom margins
                    // and automatically promote repetitive OSD cells into the active motion mask.
                    let osd_top_limit = ((self.frame_h as u64 * 12) / 100) as u32;
                    let osd_bot_limit = ((self.frame_h as u64 * 86) / 100) as u32;
                    let in_osd_band = fy2 <= osd_top_limit || fy1 >= osd_bot_limit;
                    let is_osd_clock_tick = in_osd_band
                        && b.height() <= osd_top_limit
                        && (b.area() as u64).saturating_mul(160) <= (frame_area as u64);
                    if is_osd_clock_tick {
                        for cy in st.y_min..st.y_max.min(self.motion_h) {
                            for cx in st.x_min..st.x_max.min(self.motion_w) {
                                let idx = (cy as usize) * w + (cx as usize);
                                self.osd_tick_counts[idx] =
                                    self.osd_tick_counts[idx].saturating_add(1);
                                if self.osd_tick_counts[idx] >= 2 && !self.masked_bitmap[idx] {
                                    self.masked_bitmap[idx] = true;
                                    self.masked_indices.push(idx);
                                    self.avg_frame[idx] = 255.0;
                                    self.avg_delta[idx] = 0.0;
                                    self.auto_masked_count += 1;
                                }
                            }
                        }
                    } else {
                        boxes.push(b);
                    }
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
    if frame_w == 0 || frame_h == 0 {
        return Vec::new();
    }
    let clamp_box = |b: &BoundingBox| -> Option<BoundingBox> {
        let x1 = b.x_min.min(b.x_max).min(frame_w);
        let y1 = b.y_min.min(b.y_max).min(frame_h);
        let x2 = b.x_max.max(b.x_min).min(frame_w);
        let y2 = b.y_max.max(b.y_min).min(frame_h);
        if x2 > x1 && y2 > y1 {
            Some(BoundingBox::new(x1, y1, x2, y2))
        } else {
            None
        }
    };
    let mut candidates: Vec<BoundingBox> =
        Vec::with_capacity(motion_boxes.len() + tracked_boxes.len());
    for tb in tracked_boxes {
        if let Some(cb) = clamp_box(tb) {
            candidates.push(cb);
        }
    }
    for mb in motion_boxes {
        if let Some(cb) = clamp_box(mb)
            && !candidates.iter().any(|c| cb.inside(c))
        {
            candidates.push(cb);
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
                    a.x_max.saturating_add(pad).min(frame_w),
                    a.y_max.saturating_add(pad).min(frame_h),
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
    let safe_model_size = model_size.max(4);
    let mut regions = Vec::with_capacity(candidates.len());
    for c in candidates {
        let raw_size = ((c.width().max(c.height()) as f64) * 1.2) as u32;
        let size = (raw_size.max(safe_model_size) / 4 * 4)
            .clamp(safe_model_size.min(min_dim), min_dim)
            .max(1);
        let (cx, cy) = c.center();
        let half = (size / 2) as i64;
        let max_x0 = frame_w.saturating_sub(size) as i64;
        let max_y0 = frame_h.saturating_sub(size) as i64;
        let x0 = ((cx as i64) - half).clamp(0, max_x0) as u32;
        let y0 = ((cy as i64) - half).clamp(0, max_y0) as u32;
        let reg = BoundingBox::new(
            x0,
            y0,
            x0.saturating_add(size).min(frame_w),
            y0.saturating_add(size).min(frame_h),
        );
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
    if fw < 2 || fh < 2 || model_w == 0 || model_h == 0 {
        out_tensor.fill(0);
        return;
    }
    let uv_stride = fw / 2;
    let uv_rows = fh / 2;
    let Some(y_size) = fw.checked_mul(fh) else {
        out_tensor.fill(0);
        return;
    };
    let Some(uv_size) = uv_stride.checked_mul(uv_rows) else {
        out_tensor.fill(0);
        return;
    };
    let Some(min_buf_len) = y_size.checked_add(uv_size.saturating_mul(2)) else {
        out_tensor.fill(0);
        return;
    };
    if frame.data.len() < min_buf_len {
        out_tensor.fill(0);
        return;
    }

    let mw = model_w as usize;
    let mh = model_h as usize;
    let Some(req_out_len) = mw.checked_mul(mh).and_then(|px| px.checked_mul(3)) else {
        out_tensor.fill(0);
        return;
    };
    if out_tensor.len() < req_out_len {
        out_tensor.fill(0);
        return;
    }

    let y_plane = &frame.data[..y_size];
    let u_plane = &frame.data[y_size..y_size + uv_size];
    let v_plane = &frame.data[y_size + uv_size..y_size + uv_size * 2];

    let rx0 = (region.x_min.min(region.x_max) as usize).min(fw - 1);
    let ry0 = (region.y_min.min(region.y_max) as usize).min(fh - 1);
    let rx1 = (region.x_max.max(region.x_min) as usize).clamp(rx0 + 1, fw);
    let ry1 = (region.y_max.max(region.y_min) as usize).clamp(ry0 + 1, fh);
    let rw = (rx1 - rx0).max(1);
    let rh = (ry1 - ry0).max(1);

    for ty in 0..mh {
        let sy = ry0.saturating_add((ty.saturating_mul(rh)) / mh).min(fh - 1);
        let uv_y = (sy / 2).min(uv_rows - 1);
        for tx in 0..mw {
            let sx = rx0.saturating_add((tx.saturating_mul(rw)) / mw).min(fw - 1);
            let uv_x = (sx / 2).min(uv_stride - 1);
            let y_val = y_plane[sy * fw + sx];
            let u_val = u_plane[uv_y * uv_stride + uv_x];
            let v_val = v_plane[uv_y * uv_stride + uv_x];
            let (r, g, b) = yuv_to_rgb_pixel(y_val, u_val, v_val);
            let dst_idx = (ty * mw + tx) * 3;
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
    if fw < 2 || fh < 2 {
        return RgbImage::new(320, 180);
    }
    let uv_stride = fw / 2;
    let uv_rows = fh / 2;
    let Some(y_size) = fw.checked_mul(fh) else {
        return RgbImage::new(320, 180);
    };
    let Some(uv_size) = uv_stride.checked_mul(uv_rows) else {
        return RgbImage::new(320, 180);
    };
    let Some(min_buf_len) = y_size.checked_add(uv_size.saturating_mul(2)) else {
        return RgbImage::new(320, 180);
    };
    if frame.data.len() < min_buf_len {
        return RgbImage::new(320, 180);
    }

    let region = match crop {
        Some(c) => {
            let x1 = c.x_min.min(c.x_max).min(frame.width - 1);
            let y1 = c.y_min.min(c.y_max).min(frame.height - 1);
            let x2 = c.x_max.max(c.x_min).clamp(x1 + 1, frame.width);
            let y2 = c.y_max.max(c.y_min).clamp(y1 + 1, frame.height);
            BoundingBox::new(x1, y1, x2, y2)
        }
        None => BoundingBox::new(0, 0, frame.width, frame.height),
    };
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

    for dy in 0..out_h {
        let sy = (region.y_min as usize)
            .saturating_add((dy as usize).saturating_mul(rh as usize) / (out_h as usize))
            .min(fh - 1);
        let uv_y = (sy / 2).min(uv_rows - 1);
        for dx in 0..out_w {
            let sx = (region.x_min as usize)
                .saturating_add((dx as usize).saturating_mul(rw as usize) / (out_w as usize))
                .min(fw - 1);
            let uv_x = (sx / 2).min(uv_stride - 1);
            let (r, g, b) = yuv_to_rgb_pixel(
                y_plane[sy * fw + sx],
                u_plane[uv_y * uv_stride + uv_x],
                v_plane[uv_y * uv_stride + uv_x],
            );
            img.put_pixel(dx, dy, Rgb([r, g, b]));
        }
    }

    // Draw 2px bounding boxes scaled to (out_w, out_h)
    for (raw_bbox, color) in boxes {
        let bbox = BoundingBox::new(
            raw_bbox.x_min,
            raw_bbox.y_min,
            raw_bbox.x_max,
            raw_bbox.y_max,
        );
        if !bbox.intersects(&region) {
            continue;
        }
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
                img.put_pixel(x, y1.saturating_add(t).min(out_h - 1), rgb);
                img.put_pixel(x, y2.saturating_sub(t), rgb);
            }
        }
        for y in y1..=y2 {
            for t in 0..2 {
                img.put_pixel(x1.saturating_add(t).min(out_w - 1), y, rgb);
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
    if width < 2 || height < 2 {
        return false;
    }
    let y_len = (width as usize).saturating_mul(height as usize);
    let uv_len = (width as usize / 2).saturating_mul(height as usize / 2);
    if uv_len == 0 || yuv_data.len() < y_len.saturating_add(uv_len.saturating_mul(2)) {
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
    if fw < 2 || fh < 2 {
        return "color:gray".to_string();
    }
    let uv_stride = fw / 2;
    let uv_rows = fh / 2;
    let y_size = fw.saturating_mul(fh);
    let uv_size = uv_stride.saturating_mul(uv_rows);
    if yuv_data.len() < y_size.saturating_add(uv_size.saturating_mul(2)) {
        return "color:gray".to_string();
    }

    let y_plane = &yuv_data[..y_size];
    let u_plane = &yuv_data[y_size..y_size + uv_size];
    let v_plane = &yuv_data[y_size + uv_size..y_size + uv_size * 2];

    let x1 = (bbox.x_min.min(bbox.x_max) as usize).min(fw - 1);
    let y1 = (bbox.y_min.min(bbox.y_max) as usize).min(fh - 1);
    let x2 = (bbox.x_max.max(bbox.x_min) as usize).clamp(x1 + 1, fw);
    let y2 = (bbox.y_max.max(bbox.y_min) as usize).clamp(y1 + 1, fh);

    let mut r_sum = 0u64;
    let mut g_sum = 0u64;
    let mut b_sum = 0u64;
    let mut count = 0u64;
    let step_y = ((y2 - y1) / 16).max(1);
    let step_x = ((x2 - x1) / 16).max(1);

    let mut sy = y1;
    while sy < y2 {
        let uv_y = (sy / 2).min(uv_rows - 1);
        let mut sx = x1;
        while sx < x2 {
            let uv_x = (sx / 2).min(uv_stride - 1);
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

/// Redacts all pixels inside `mask` on `img` by replacing each `block_size x block_size` tile
/// with the mean RGB value of pixels inside that tile.
pub fn apply_privacy_mask_pixelated_rgb(img: &mut RgbImage, mask: &PolygonMask, block_size: u32) {
    if mask.is_empty() {
        return;
    }
    let w = img.width();
    let h = img.height();
    let bs = block_size.clamp(2, 256);
    let mut by = 0u32;
    while by < h {
        let y_end = by.saturating_add(bs).min(h);
        let mut bx = 0u32;
        while bx < w {
            let x_end = bx.saturating_add(bs).min(w);
            let mut r_sum = 0u64;
            let mut g_sum = 0u64;
            let mut b_sum = 0u64;
            let mut count = 0u64;
            let mut any_masked = false;
            for y in by..y_end {
                let fy = y as f32;
                for x in bx..x_end {
                    let px = img.get_pixel(x, y);
                    r_sum += px[0] as u64;
                    g_sum += px[1] as u64;
                    b_sum += px[2] as u64;
                    count += 1;
                    if mask.contains_point(x as f32, fy) {
                        any_masked = true;
                    }
                }
            }
            if any_masked && count > 0 {
                let avg = Rgb([
                    (r_sum / count) as u8,
                    (g_sum / count) as u8,
                    (b_sum / count) as u8,
                ]);
                for y in by..y_end {
                    let fy = y as f32;
                    for x in bx..x_end {
                        if mask.contains_point(x as f32, fy) {
                            img.put_pixel(x, y, avg);
                        }
                    }
                }
            }
            bx = bx.saturating_add(bs);
        }
        by = by.saturating_add(bs);
    }
}

fn is_pixelate_value(v: &Value) -> bool {
    v.get("mode")
        .or_else(|| v.get("style"))
        .and_then(|m| m.as_str())
        .is_some_and(|m| m.eq_ignore_ascii_case("pixelate") || m.eq_ignore_ascii_case("pixelated"))
        || v.get("pixelate").and_then(|p| p.as_bool()) == Some(true)
}

/// Parses normalized or pixel `privacy_mask` polygon coordinates against `(img.width(), img.height())`
/// and redacts all interior pixels to `[12, 14, 18]` (or 8x8 block pixelation when `mode: "pixelate"` is set).
pub fn apply_privacy_masks_from_value(img: &mut RgbImage, mask_val: &Value) {
    if mask_val.is_null() {
        return;
    }
    let w = img.width();
    let h = img.height();
    let mask = PolygonMask::from_config_value(mask_val, w, h);
    if is_pixelate_value(mask_val) {
        let block_size = mask_val
            .get("block_size")
            .and_then(|b| b.as_u64())
            .unwrap_or(8) as u32;
        apply_privacy_mask_pixelated_rgb(img, &mask, block_size);
    } else {
        apply_privacy_mask_rgb(img, &mask);
    }
}

/// Computes a fast 64-bit horizontal difference perceptual hash (`dhash64`) of a `BoundingBox`
/// crop from the Y (luma) plane of a `YUV420p` frame by box-averaging onto a 9x8 grid and setting
/// bit `(r * 8 + c)` to `1` when `luma[r][c] > luma[r][c + 1]`.
pub fn compute_crop_dhash64(yuv: &[u8], width: u32, height: u32, bbox: BoundingBox) -> u64 {
    let fw = width as usize;
    let fh = height as usize;
    let y_size = fw.saturating_mul(fh);
    if fw == 0 || fh == 0 || yuv.len() < y_size {
        return 0;
    }
    let y_plane = &yuv[..y_size];

    let x1 = (bbox.x_min.min(bbox.x_max) as usize).min(fw - 1);
    let y1 = (bbox.y_min.min(bbox.y_max) as usize).min(fh - 1);
    let x2 = (bbox.x_max.max(bbox.x_min) as usize).clamp(x1 + 1, fw);
    let y2 = (bbox.y_max.max(bbox.y_min) as usize).clamp(y1 + 1, fh);
    let bw = (x2 - x1).max(1);
    let bh = (y2 - y1).max(1);

    let mut grid = [[0u8; 9]; 8];
    for (r, grid_row) in grid.iter_mut().enumerate() {
        let cy0 = y1 + (r * bh) / 8;
        let cy1 = (y1 + ((r + 1) * bh) / 8).max(cy0 + 1).min(fh);
        let step_y = ((cy1 - cy0) / 16).max(1);
        for (c, cell) in grid_row.iter_mut().enumerate() {
            let cx0 = x1 + (c * bw) / 9;
            let cx1 = (x1 + ((c + 1) * bw) / 9).max(cx0 + 1).min(fw);
            let step_x = ((cx1 - cx0) / 16).max(1);
            let mut sum = 0u64;
            let mut count = 0u64;
            let mut sy = cy0;
            while sy < cy1 {
                let row = &y_plane[sy * fw..(sy + 1) * fw];
                let mut sx = cx0;
                while sx < cx1 {
                    sum += row[sx] as u64;
                    count += 1;
                    sx += step_x;
                }
                sy += step_y;
            }
            *cell = (sum + count / 2).checked_div(count).unwrap_or(0) as u8;
        }
    }

    let mut hash = 0u64;
    for (r, grid_row) in grid.iter().enumerate() {
        for c in 0..8 {
            if grid_row[c] > grid_row[c + 1] {
                hash |= 1u64 << (r * 8 + c);
            }
        }
    }
    hash
}

/// Returns the Hamming distance (number of differing bits) between two 64-bit perceptual hashes.
#[inline]
pub fn hamming_distance_u64(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Draws 2px bounding boxes onto an existing `RgbImage` (e.g. a high-resolution main-stream frame).
pub fn annotate_rgb_image_with_boxes(img: &mut RgbImage, boxes: &[(BoundingBox, [u8; 3])]) {
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return;
    }
    for (bbox, color) in boxes {
        let x1 = bbox.x_min.min(bbox.x_max).min(w - 1);
        let y1 = bbox.y_min.min(bbox.y_max).min(h - 1);
        let x2 = bbox.x_max.max(bbox.x_min).min(w - 1);
        let y2 = bbox.y_max.max(bbox.y_min).min(h - 1);
        let rgb = Rgb(*color);
        for x in x1..=x2 {
            for t in 0..2 {
                img.put_pixel(x, y1.saturating_add(t).min(h - 1), rgb);
                img.put_pixel(x, y2.saturating_sub(t), rgb);
            }
        }
        for y in y1..=y2 {
            for t in 0..2 {
                img.put_pixel(x1.saturating_add(t).min(w - 1), y, rgb);
                img.put_pixel(x2.saturating_sub(t), y, rgb);
            }
        }
    }
}

/// Resolves a normalized (`0.0..=1.0`) or pixel-space `[x1, y1, x2, y2]` ROI crop array
/// into a valid `BoundingBox` clamped within `(0..frame_w, 0..frame_h)`.
pub fn resolve_roi_crop_bbox(crop: [f32; 4], frame_w: u32, frame_h: u32) -> BoundingBox {
    let safe_w = frame_w.max(1);
    let safe_h = frame_h.max(1);
    if crop.iter().any(|v| !v.is_finite()) {
        return BoundingBox::new(0, 0, safe_w, safe_h);
    }
    let fw = safe_w as f64;
    let fh = safe_h as f64;
    let is_normalized = crop.iter().all(|&v| (0.0..=1.0).contains(&v));
    let raw_x1 = if is_normalized {
        (crop[0] as f64) * fw
    } else {
        crop[0] as f64
    };
    let raw_y1 = if is_normalized {
        (crop[1] as f64) * fh
    } else {
        crop[1] as f64
    };
    let raw_x2 = if is_normalized {
        (crop[2] as f64) * fw
    } else {
        crop[2] as f64
    };
    let raw_y2 = if is_normalized {
        (crop[3] as f64) * fh
    } else {
        crop[3] as f64
    };

    let max_x_min = safe_w.saturating_sub(1);
    let max_y_min = safe_h.saturating_sub(1);
    let x_min = (raw_x1.min(raw_x2).round().clamp(0.0, max_x_min as f64) as u32).min(max_x_min);
    let y_min = (raw_y1.min(raw_y2).round().clamp(0.0, max_y_min as f64) as u32).min(max_y_min);
    let x_max =
        (raw_x1.max(raw_x2).round().clamp((x_min + 1) as f64, fw) as u32).clamp(x_min + 1, safe_w);
    let y_max =
        (raw_y1.max(raw_y2).round().clamp((y_min + 1) as f64, fh) as u32).clamp(y_min + 1, safe_h);
    BoundingBox::new(x_min, y_min, x_max, y_max)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoiCropSignature {
    pub mean_luma: f32,
    pub luma_std_dev: f32,
    pub edge_energy: f32,
    pub activity_score: f32,
}

/// Computes luminance mean, standard deviation (contrast), spatial edge energy, and
/// a normalized `activity_score` (`0.0..=1.0`) for `crop` inside a planar `YUV420p` buffer.
pub fn compute_roi_crop_signature(
    yuv_data: &[u8],
    width: u32,
    height: u32,
    crop: [f32; 4],
) -> RoiCropSignature {
    let fw = width as usize;
    let fh = height as usize;
    let y_len = fw.saturating_mul(fh);
    if fw == 0 || fh == 0 || yuv_data.len() < y_len {
        return RoiCropSignature {
            mean_luma: 0.0,
            luma_std_dev: 0.0,
            edge_energy: 0.0,
            activity_score: 0.0,
        };
    }

    let bbox = resolve_roi_crop_bbox(crop, width, height);
    let x1 = (bbox.x_min as usize).min(fw.saturating_sub(1));
    let y1 = (bbox.y_min as usize).min(fh.saturating_sub(1));
    let x2 = (bbox.x_max as usize).clamp(x1 + 1, fw);
    let y2 = (bbox.y_max as usize).clamp(y1 + 1, fh);

    let y_plane = &yuv_data[..y_len];
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut edge_sum = 0u64;
    let mut count = 0u64;

    for y in y1..y2 {
        let row = &y_plane[y * fw..(y + 1) * fw];
        let next_row = if y + 1 < y2 {
            Some(&y_plane[(y + 1) * fw..(y + 2) * fw])
        } else {
            None
        };
        for x in x1..x2 {
            let v = row[x];
            let vf = v as f64;
            sum += vf;
            sum_sq += vf * vf;
            if x + 1 < x2 {
                edge_sum += (v as i32 - row[x + 1] as i32).unsigned_abs() as u64;
            }
            if let Some(nr) = next_row {
                edge_sum += (v as i32 - nr[x] as i32).unsigned_abs() as u64;
            }
            count += 1;
        }
    }

    if count == 0 {
        return RoiCropSignature {
            mean_luma: 0.0,
            luma_std_dev: 0.0,
            edge_energy: 0.0,
            activity_score: 0.0,
        };
    }

    let mean_luma = (sum / (count as f64)) as f32;
    let variance = ((sum_sq / (count as f64)) - (mean_luma as f64) * (mean_luma as f64)).max(0.0);
    let luma_std_dev = (variance.sqrt()) as f32;
    let edge_energy = (edge_sum as f32) / (count as f32);

    let luma_norm = ((mean_luma - 55.0) / 145.0).clamp(0.0, 1.0);
    let edge_norm = (edge_energy / 20.0).clamp(0.0, 1.0);
    let contrast_norm = (luma_std_dev / 40.0).clamp(0.0, 1.0);
    let activity_score =
        (0.50 * luma_norm + 0.30 * edge_norm + 0.20 * contrast_norm).clamp(0.0, 1.0);

    RoiCropSignature {
        mean_luma,
        luma_std_dev,
        edge_energy,
        activity_score,
    }
}

/// Evaluates a configured ROI crop on a planar `YUV420p` frame and returns `(predicted_class, confidence)`
/// across `cfg.classes` (defaulting to `["closed", "open"]`).
pub fn classify_roi_crop(
    yuv_data: &[u8],
    width: u32,
    height: u32,
    cfg: &StateRoiConfig,
) -> (String, f32) {
    let classes: Vec<&str> = if cfg.classes.is_empty() {
        vec!["closed", "open"]
    } else {
        cfg.classes.iter().map(String::as_str).collect()
    };
    if classes.len() == 1 {
        return (classes[0].to_string(), 1.0);
    }

    let sig = compute_roi_crop_signature(yuv_data, width, height, cfg.crop);
    let score = if sig.activity_score.is_finite() {
        sig.activity_score.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let n = classes.len();
    let raw_idx = (score * (n as f32)).floor() as usize;
    let idx = raw_idx.min(n - 1);
    let conf = (0.55 + (score - 0.5).abs() * 0.9).clamp(0.50, 0.99);
    (classes[idx].to_string(), conf)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoiStateTransition {
    pub roi_name: String,
    pub previous_state: Option<String>,
    pub current_state: String,
    pub confidence: f32,
    pub state_changed: bool,
    pub is_anomalous: bool,
    pub anomaly_triggered: bool,
    pub anomaly_elapsed_sec: f64,
}

/// Tracks a single ROI's state with N-frame hysteresis confirmation and sustained anomaly timeout detection.
#[derive(Debug, Clone)]
pub struct RoiStateTracker {
    roi_name: String,
    config: StateRoiConfig,
    confirmed_state: String,
    pending_state: Option<String>,
    pending_count: u32,
    state_since_ts: f64,
    last_eval_ts: Option<f64>,
    had_motion_in_roi: bool,
    anomaly_alert_emitted: bool,
    initialized: bool,
}

impl RoiStateTracker {
    pub fn new(roi_name: impl Into<String>, config: StateRoiConfig) -> Self {
        let initial_state = config
            .classes
            .first()
            .cloned()
            .unwrap_or_else(|| "closed".to_string());
        Self {
            roi_name: roi_name.into(),
            config,
            confirmed_state: initial_state,
            pending_state: None,
            pending_count: 0,
            state_since_ts: 0.0,
            last_eval_ts: None,
            had_motion_in_roi: false,
            anomaly_alert_emitted: false,
            initialized: false,
        }
    }

    pub fn roi_name(&self) -> &str {
        &self.roi_name
    }

    pub fn config(&self) -> &StateRoiConfig {
        &self.config
    }

    pub fn current_state(&self) -> &str {
        &self.confirmed_state
    }

    pub fn pending_state(&self) -> Option<&str> {
        self.pending_state.as_deref()
    }

    pub fn pending_count(&self) -> u32 {
        self.pending_count
    }

    pub fn is_anomalous(&self) -> bool {
        self.config
            .anomaly_state
            .as_ref()
            .is_some_and(|a| a.eq_ignore_ascii_case(&self.confirmed_state))
    }

    pub fn anomaly_elapsed_sec(&self, now: f64) -> f64 {
        if self.is_anomalous()
            && self.initialized
            && now.is_finite()
            && self.state_since_ts.is_finite()
        {
            (now - self.state_since_ts).max(0.0)
        } else {
            0.0
        }
    }

    /// Returns `true` when the ROI crop should be evaluated at `now`:
    /// either on the first frame, when `interval_sec` has elapsed, while confirming a pending
    /// hysteresis transition, or immediately after motion inside the ROI settles.
    pub fn should_evaluate(&mut self, now: f64, motion_in_roi: bool) -> bool {
        if !now.is_finite() {
            return false;
        }
        if motion_in_roi {
            self.had_motion_in_roi = true;
        }
        let motion_settled = self.had_motion_in_roi && !motion_in_roi;
        let interval_due = match self.last_eval_ts {
            None => true,
            Some(last) if !last.is_finite() || now < last => true,
            Some(last) => (now - last) >= (self.config.interval_sec.max(1) as f64),
        };
        if motion_settled || interval_due {
            if motion_settled {
                self.had_motion_in_roi = false;
            }
            true
        } else {
            false
        }
    }

    /// Feeds a classified `candidate_state` into the N-frame hysteresis state machine at timestamp `now`.
    /// Requires `config.hysteresis` consecutive confirmations before transitioning `confirmed_state`,
    /// and triggers `anomaly_triggered = true` once when `confirmed_state == anomaly_state` for
    /// `>= config.anomaly_timeout_sec`.
    pub fn observe_state(
        &mut self,
        candidate_state: &str,
        confidence: f32,
        now: f64,
    ) -> RoiStateTransition {
        let has_prev_finite_ts = self.last_eval_ts.is_some_and(|t| t.is_finite());
        let safe_now = if now.is_finite() {
            now
        } else {
            self.last_eval_ts.filter(|t| t.is_finite()).unwrap_or(0.0)
        };
        let safe_conf = if confidence.is_finite() {
            confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if now.is_finite() {
            self.last_eval_ts = Some(now);
        }
        if !self.state_since_ts.is_finite()
            || (self.initialized && (!has_prev_finite_ts || safe_now < self.state_since_ts))
        {
            self.state_since_ts = safe_now;
        }
        let mut previous_state = None;
        let mut state_changed = false;

        if candidate_state.eq_ignore_ascii_case(&self.confirmed_state) {
            self.pending_state = None;
            self.pending_count = 0;
            if !self.initialized {
                self.initialized = true;
                self.state_since_ts = safe_now;
            }
        } else {
            if !self.initialized {
                self.initialized = true;
                self.state_since_ts = safe_now;
            }
            let same_pending = self
                .pending_state
                .as_ref()
                .is_some_and(|p| p.eq_ignore_ascii_case(candidate_state));
            if same_pending {
                self.pending_count = self.pending_count.saturating_add(1);
            } else {
                self.pending_state = Some(candidate_state.to_string());
                self.pending_count = 1;
            }

            let required = self.config.hysteresis.max(1);
            if self.pending_count >= required {
                previous_state = Some(self.confirmed_state.clone());
                self.confirmed_state = candidate_state.to_string();
                self.pending_state = None;
                self.pending_count = 0;
                self.state_since_ts = safe_now;
                self.anomaly_alert_emitted = false;
                state_changed = true;
            }
        }

        let is_anomalous = self.is_anomalous();
        let anomaly_elapsed_sec = if is_anomalous {
            (safe_now - self.state_since_ts).max(0.0)
        } else {
            self.anomaly_alert_emitted = false;
            0.0
        };

        let timeout_sec = self.config.anomaly_timeout_sec.max(1) as f64;
        let anomaly_triggered =
            if is_anomalous && !self.anomaly_alert_emitted && anomaly_elapsed_sec >= timeout_sec {
                self.anomaly_alert_emitted = true;
                true
            } else {
                false
            };

        RoiStateTransition {
            roi_name: self.roi_name.clone(),
            previous_state,
            current_state: self.confirmed_state.clone(),
            confidence: safe_conf,
            state_changed,
            is_anomalous,
            anomaly_triggered,
            anomaly_elapsed_sec,
        }
    }

    /// Evaluates a YUV420p frame if due (or when `force_eval` is true) and advances the hysteresis state machine.
    pub fn evaluate_frame(
        &mut self,
        yuv_data: &[u8],
        width: u32,
        height: u32,
        now: f64,
        motion_boxes: &[BoundingBox],
        force_eval: bool,
    ) -> Option<RoiStateTransition> {
        let roi_bbox = resolve_roi_crop_bbox(self.config.crop, width, height);
        let motion_in_roi = motion_boxes.iter().any(|mb| mb.intersects(&roi_bbox));
        if !force_eval && !self.should_evaluate(now, motion_in_roi) {
            return None;
        }
        let (candidate, confidence) = classify_roi_crop(yuv_data, width, height, &self.config);
        Some(self.observe_state(&candidate, confidence, now))
    }
}

/// Manages all configured `RoiStateTracker` instances for a single camera.
#[derive(Debug, Clone, Default)]
pub struct CameraRoiStateMonitor {
    trackers: BTreeMap<String, RoiStateTracker>,
}

impl CameraRoiStateMonitor {
    pub fn new(rois: &BTreeMap<String, StateRoiConfig>) -> Self {
        let mut trackers = BTreeMap::new();
        for (name, cfg) in rois {
            trackers.insert(
                name.clone(),
                RoiStateTracker::new(name.clone(), cfg.clone()),
            );
        }
        Self { trackers }
    }

    pub fn is_empty(&self) -> bool {
        self.trackers.is_empty()
    }

    pub fn current_states(&self) -> BTreeMap<String, String> {
        self.trackers
            .iter()
            .map(|(k, t)| (k.clone(), t.current_state().to_string()))
            .collect()
    }

    pub fn tracker_mut(&mut self, roi_name: &str) -> Option<&mut RoiStateTracker> {
        self.trackers.get_mut(roi_name)
    }

    pub fn evaluate_frame(
        &mut self,
        yuv_data: &[u8],
        width: u32,
        height: u32,
        now: f64,
        motion_boxes: &[BoundingBox],
        force_eval: bool,
    ) -> Vec<RoiStateTransition> {
        let mut out = Vec::new();
        for tracker in self.trackers.values_mut() {
            if let Some(tr) =
                tracker.evaluate_frame(yuv_data, width, height, now, motion_boxes, force_eval)
            {
                out.push(tr);
            }
        }
        out
    }
}

/// Decodes an encoded image (`JPEG`, `WebP`, `PNG`) or a raw square `N x N x 3` RGB pixel buffer
/// (such as `112x112x3`) into a `16 x 16` area-averaged `[R, G, B]` grid (`256` cells in `0.0..=1.0`).
/// Returns `None` if `data` is neither a decodable image nor a valid square RGB pixel buffer.
pub fn decode_or_extract_rgb_grid_16x16(data: &[u8]) -> Option<[[f32; 3]; 256]> {
    if data.is_empty() || data.len() > 32 * 1024 * 1024 {
        return None;
    }
    let decoded_img = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()
        .and_then(|mut reader| {
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(4096);
            limits.max_image_height = Some(4096);
            limits.max_alloc = Some(64 * 1024 * 1024);
            reader.limits(limits);
            reader.decode().ok()
        });
    let (raw_rgb, w, h): (std::borrow::Cow<'_, [u8]>, usize, usize) =
        if let Some(dyn_img) = decoded_img {
            let rgb = dyn_img.to_rgb8();
            let (iw, ih) = (rgb.width() as usize, rgb.height() as usize);
            if iw < 8 || ih < 8 {
                return None;
            }
            (std::borrow::Cow::Owned(rgb.into_raw()), iw, ih)
        } else if data.len() >= 16 * 16 * 3 && data.len().is_multiple_of(3) {
            let px_count = data.len() / 3;
            let side = (px_count as f64).sqrt().round() as usize;
            if side >= 16 && side * side == px_count {
                (std::borrow::Cow::Borrowed(data), side, side)
            } else {
                return None;
            }
        } else {
            return None;
        };

    let mut grid = [[0.0f32; 3]; 256];
    for gy in 0..16usize {
        let y0 = (gy * h) / 16;
        let y1 = (((gy + 1) * h) / 16).max(y0 + 1).min(h);
        for gx in 0..16usize {
            let x0 = (gx * w) / 16;
            let x1 = (((gx + 1) * w) / 16).max(x0 + 1).min(w);
            let mut sum_r = 0u64;
            let mut sum_g = 0u64;
            let mut sum_b = 0u64;
            let mut count = 0u64;
            for py in y0..y1 {
                let row_off = py * w * 3;
                for px in x0..x1 {
                    let idx = row_off + px * 3;
                    sum_r += raw_rgb[idx] as u64;
                    sum_g += raw_rgb[idx + 1] as u64;
                    sum_b += raw_rgb[idx + 2] as u64;
                    count += 1;
                }
            }
            let denom = (count.max(1) as f32) * 255.0;
            grid[gy * 16 + gx] = [
                (sum_r as f32) / denom,
                (sum_g as f32) / denom,
                (sum_b as f32) / denom,
            ];
        }
    }
    Some(grid)
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

        // Static scene change must settle back into avg_frame within ~12 frames (not 200+ frames)
        let mut settled = false;
        for _ in 0..14 {
            if detector.detect(&motion_frame).is_empty() {
                settled = true;
                break;
            }
        }
        assert!(settled);
    }

    #[test]
    fn simd_motion_detector_ignores_corner_osd_clock_ticks() {
        let cfg = MotionConfig::default();
        let mut detector = SimdMotionDetector::new(1280, 720, cfg);
        let base_bytes = vec![64u8; 1280 * 720 * 3 / 2];
        let bg_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: 1280,
            height: 720,
            data: Arc::new(base_bytes.clone()),
        };
        for _ in 0..32 {
            assert!(detector.detect(&bg_frame).is_empty());
        }

        // Simulate a 30x30 OSD seconds digit flipping in the top-left corner (900 px² < 0.4% frame area)
        let mut osd_bytes = base_bytes;
        for y in 12..42 {
            for x in 380..410 {
                osd_bytes[y * 1280 + x] = 240;
            }
        }
        let osd_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 2.0,
            width: 1280,
            height: 720,
            data: Arc::new(osd_bytes),
        };
        let _ = detector.detect(&osd_frame);
        assert!(detector.detect(&osd_frame).is_empty());
        let _ = detector.detect(&osd_frame);
        assert!(detector.auto_masked_cell_count() > 0);
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

        // Saturated blue frame: Y=60, U=240, V=110
        let mut blue_buf = vec![60u8; y_len + uv_len * 2];
        blue_buf[y_len..y_len + uv_len].fill(240);
        blue_buf[y_len + uv_len..].fill(110);
        assert_eq!(
            classify_dominant_color_tag(&blue_buf, w, h, bbox),
            "color:blue"
        );

        // Saturated green frame: Y=150, U=60, V=60
        let mut green_buf = vec![150u8; y_len + uv_len * 2];
        green_buf[y_len..y_len + uv_len].fill(60);
        green_buf[y_len + uv_len..].fill(60);
        assert_eq!(
            classify_dominant_color_tag(&green_buf, w, h, bbox),
            "color:green"
        );

        // Warm yellow frame: Y=200, U=30, V=150
        let mut yellow_buf = vec![200u8; y_len + uv_len * 2];
        yellow_buf[y_len..y_len + uv_len].fill(30);
        yellow_buf[y_len + uv_len..].fill(150);
        assert_eq!(
            classify_dominant_color_tag(&yellow_buf, w, h, bbox),
            "color:yellow"
        );

        // Neutral gray frame: Y=128, U=128, V=128
        let gray_buf = vec![128u8; y_len + uv_len * 2];
        assert_eq!(
            classify_dominant_color_tag(&gray_buf, w, h, bbox),
            "color:gray"
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

        // Null mask is a no-op
        let mut untouched = RgbImage::from_pixel(32, 32, Rgb([200, 100, 50]));
        apply_privacy_masks_from_value(&mut untouched, &Value::Null);
        assert_eq!(*untouched.get_pixel(16, 16), Rgb([200, 100, 50]));

        // Pixelation mode averages 8x8 tile colors inside the polygon while leaving exterior pixels intact
        let mut pix_img = RgbImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                let val = if (x + y) % 2 == 0 { 200u8 } else { 40u8 };
                pix_img.put_pixel(x, y, Rgb([val, val, val]));
            }
        }
        let pix_mask = serde_json::json!({
            "mode": "pixelate",
            "block_size": 8,
            "coordinates": "16,16,48,16,48,48,16,48"
        });
        apply_privacy_masks_from_value(&mut pix_img, &pix_mask);
        // Interior pixels (24, 24) and (25, 24) are homogenized to block mean 120
        assert_eq!(*pix_img.get_pixel(24, 24), Rgb([120, 120, 120]));
        assert_eq!(*pix_img.get_pixel(25, 24), Rgb([120, 120, 120]));
        // Exterior checkerboard pixels (2, 2) and (3, 2) remain distinct (200 vs 40)
        assert_eq!(*pix_img.get_pixel(2, 2), Rgb([200, 200, 200]));
        assert_eq!(*pix_img.get_pixel(3, 2), Rgb([40, 40, 40]));
    }

    #[test]
    fn crop_yuv420p_to_tensor_nhwc_respects_dimensions_and_rgb_vs_bgr_order() {
        let w = 64u32;
        let h = 64u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        // Saturated red YUV420p frame: Y=90, U=90, V=240 => R=255, G=10, B=10
        let mut data = vec![90u8; y_len + uv_len * 2];
        data[y_len..y_len + uv_len].fill(90);
        data[y_len + uv_len..].fill(240);
        let frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: w,
            height: h,
            data: Arc::new(data),
        };
        let region = BoundingBox::new(0, 0, w, h);
        let model_w = 16u32;
        let model_h = 12u32;
        let expected_len = (model_w * model_h * 3) as usize;

        let mut rgb_tensor = vec![0u8; expected_len];
        crop_yuv420p_to_tensor_nhwc(&frame, &region, model_w, model_h, false, &mut rgb_tensor);
        assert_eq!(rgb_tensor.len(), expected_len);
        // RGB order: R high (>200), B low (<30)
        assert!(rgb_tensor[0] > 200);
        assert!(rgb_tensor[1] < 30);
        assert!(rgb_tensor[2] < 30);

        let mut bgr_tensor = vec![0u8; expected_len];
        crop_yuv420p_to_tensor_nhwc(&frame, &region, model_w, model_h, true, &mut bgr_tensor);
        assert_eq!(bgr_tensor.len(), expected_len);
        // BGR order: B low (<30) at index 0, R high (>200) at index 2
        assert_eq!(bgr_tensor[0], rgb_tensor[2]);
        assert_eq!(bgr_tensor[1], rgb_tensor[1]);
        assert_eq!(bgr_tensor[2], rgb_tensor[0]);

        // Undersized YUV frame zeroes out destination tensor safely
        let short_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: w,
            height: h,
            data: Arc::new(vec![128u8; 10]),
        };
        crop_yuv420p_to_tensor_nhwc(
            &short_frame,
            &region,
            model_w,
            model_h,
            false,
            &mut rgb_tensor,
        );
        assert!(rgb_tensor.iter().all(|&b| b == 0));
    }

    #[test]
    fn yuv420p_to_rgb_image_scaled_and_image_encoders_produce_valid_webp_and_jpeg_headers() {
        let w = 640u32;
        let h = 360u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let mut data = vec![180u8; y_len + uv_len * 2];
        data[y_len..].fill(128);
        let frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: w,
            height: h,
            data: Arc::new(data),
        };

        // Unscaled full-frame conversion with bounding box overlay
        let bbox = BoundingBox::new(100, 60, 300, 240);
        let full_img = yuv420p_to_rgb_image(&frame, None, &[(bbox, [255, 0, 0])]);
        assert_eq!(full_img.width(), 640);
        assert_eq!(full_img.height(), 360);
        assert_eq!(*full_img.get_pixel(100, 60), Rgb([255, 0, 0]));

        // Downscaled conversion to max_height = 180 preserves 16:9 aspect ratio (320x180)
        let scaled_img =
            yuv420p_to_rgb_image_scaled(&frame, None, &[(bbox, [0, 255, 0])], Some(180));
        assert_eq!(scaled_img.width(), 320);
        assert_eq!(scaled_img.height(), 180);

        // WebP magic bytes: "RIFF" (0..4) and "WEBP" (8..12)
        let webp_bytes = encode_image_webp(&scaled_img).expect("webp encode");
        assert!(webp_bytes.len() > 12);
        assert_eq!(&webp_bytes[0..4], b"RIFF");
        assert_eq!(&webp_bytes[8..12], b"WEBP");

        // JPEG magic bytes: SOI \xFF\xD8\xFF
        let jpeg_bytes = encode_image_jpeg(&scaled_img).expect("jpeg encode");
        assert!(jpeg_bytes.len() > 4);
        assert_eq!(&jpeg_bytes[0..3], &[0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn annotate_rgb_image_with_boxes_draws_border_without_modifying_interior() {
        let mut img = RgbImage::from_pixel(200, 150, Rgb([30, 40, 50]));
        let bbox = BoundingBox::new(20, 30, 120, 110);
        annotate_rgb_image_with_boxes(&mut img, &[(bbox, [10, 220, 90])]);

        // Top-left border (2px thick)
        assert_eq!(*img.get_pixel(20, 30), Rgb([10, 220, 90]));
        assert_eq!(*img.get_pixel(21, 31), Rgb([10, 220, 90]));
        // Bottom-right border
        assert_eq!(*img.get_pixel(120, 110), Rgb([10, 220, 90]));
        // Box interior and exterior remain untouched
        assert_eq!(*img.get_pixel(70, 70), Rgb([30, 40, 50]));
        assert_eq!(*img.get_pixel(5, 5), Rgb([30, 40, 50]));
    }

    #[test]
    fn polygon_mask_from_config_value_handles_strings_arrays_pixel_coords_and_invalid_inputs() {
        // 1. Normalized string
        let m_str = PolygonMask::from_config_value(
            &serde_json::json!("0.0,0.0,0.5,0.0,0.5,0.5,0.0,0.5"),
            200,
            100,
        );
        assert!(!m_str.is_empty());
        assert!(m_str.contains_point(50.0, 25.0));
        assert!(!m_str.contains_point(150.0, 75.0));

        // 2. Array of polygon strings (normalized + pixel coordinates)
        let m_arr = PolygonMask::from_config_value(
            &serde_json::json!([
                "0.0,0.0,0.2,0.0,0.2,0.2,0.0,0.2",
                "120,60,190,60,190,95,120,95"
            ]),
            200,
            100,
        );
        assert!(!m_arr.is_empty());
        assert!(m_arr.contains_point(10.0, 10.0));
        assert!(m_arr.contains_point(150.0, 80.0));
        assert!(!m_arr.contains_point(80.0, 50.0));

        // 3. Object map with nested coordinates
        let m_obj = PolygonMask::from_config_value(
            &serde_json::json!({
                "zone_a": { "coordinates": "10,10,50,10,50,50,10,50" }
            }),
            200,
            100,
        );
        assert!(m_obj.contains_point(25.0, 25.0));

        // 4. Degenerate (< 3 points = < 6 numbers) and odd-length strings are rejected
        let m_deg = PolygonMask::from_config_value(&serde_json::json!("0.1,0.1,0.5,0.5"), 100, 100);
        assert!(m_deg.is_empty());
        assert!(!m_deg.contains_point(20.0, 20.0));

        let m_odd = PolygonMask::from_config_value(
            &serde_json::json!("0.1,0.1,0.5,0.1,0.5,0.5,0.1"),
            100,
            100,
        );
        assert!(m_odd.is_empty());
        assert!(!point_in_polygon(10.0, 10.0, &[(0.0, 0.0), (20.0, 20.0)]));
    }

    #[test]
    fn simd_motion_detector_rejects_full_frame_lightning_flash_spikes() {
        let cfg = MotionConfig {
            lightning_threshold: 0.70,
            improve_contrast: false,
            delta_alpha: 1.0,
            ..MotionConfig::default()
        };
        let mut detector = SimdMotionDetector::new(320, 180, cfg);
        let dark_bytes = vec![30u8; 320 * 180 * 3 / 2];
        let dark_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: 320,
            height: 180,
            data: Arc::new(dark_bytes),
        };

        // Warmup 30 frames on dark baseline
        for _ in 0..31 {
            assert!(detector.detect(&dark_frame).is_empty());
        }

        // Sudden full-frame IR cut-filter / lightning flash (100% of pixels jump from 30 -> 230 >= lightning_threshold 0.70)
        let flash_bytes = vec![230u8; 320 * 180 * 3 / 2];
        let flash_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 2.0,
            width: 320,
            height: 180,
            data: Arc::new(flash_bytes),
        };

        let boxes = detector.detect(&flash_frame);
        assert!(
            boxes.is_empty(),
            "full-frame luminance flash exceeding lightning_threshold must be rejected"
        );
        // Subsequent frame at the new luminance level is also quiet because avg_frame was reset to the flash level
        assert!(detector.detect(&flash_frame).is_empty());
    }

    #[test]
    fn compute_crop_dhash64_is_invariant_to_minor_noise_and_distinct_across_patterns() {
        let w = 160u32;
        let h = 120u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;

        // Pattern A: horizontal left-to-right gradient inside crop (x: 20..110, y: 16..96)
        let mut frame_a = vec![128u8; y_len + uv_len * 2];
        for y in 0..h as usize {
            for x in 0..w as usize {
                // Decreasing left-to-right so luma[r][c] > luma[r][c+1] on most columns
                frame_a[y * (w as usize) + x] =
                    (240usize.saturating_sub((x * 200) / (w as usize))) as u8;
            }
        }
        let bbox = BoundingBox::new(20, 16, 110, 96);
        let hash_a1 = compute_crop_dhash64(&frame_a, w, h, bbox);
        let hash_a2 = compute_crop_dhash64(&frame_a, w, h, bbox);
        assert_eq!(hamming_distance_u64(hash_a1, hash_a2), 0);

        // Pattern A with minor sensor/compression noise (+/- 2 luma on every 5th pixel)
        let mut frame_noisy = frame_a.clone();
        for i in (0..y_len).step_by(5) {
            frame_noisy[i] = frame_noisy[i].saturating_add(2);
        }
        let hash_noisy = compute_crop_dhash64(&frame_noisy, w, h, bbox);
        assert!(
            hamming_distance_u64(hash_a1, hash_noisy) <= 4,
            "expected minor noise hamming <= 4, got {}",
            hamming_distance_u64(hash_a1, hash_noisy)
        );

        // Pattern B: inverted left-to-right gradient (increasing left-to-right) + checkerboard
        let mut frame_b = vec![128u8; y_len + uv_len * 2];
        for y in 0..h as usize {
            for x in 0..w as usize {
                frame_b[y * (w as usize) + x] = (20 + (x * 200) / (w as usize)) as u8;
            }
        }
        let hash_b = compute_crop_dhash64(&frame_b, w, h, bbox);
        assert!(
            hamming_distance_u64(hash_a1, hash_b) >= 24,
            "expected distinct pattern hamming >= 24, got {}",
            hamming_distance_u64(hash_a1, hash_b)
        );

        // Out-of-bounds bbox and undersized buffer edge cases
        let oob_bbox = BoundingBox::new(140, 100, 9999, 9999);
        let _ = compute_crop_dhash64(&frame_a, w, h, oob_bbox);
        assert_eq!(compute_crop_dhash64(&[1, 2, 3], w, h, bbox), 0);
        assert_eq!(compute_crop_dhash64(&frame_a, 0, h, bbox), 0);
    }

    #[test]
    fn roi_state_tracker_enforces_n_frame_hysteresis_and_anomaly_timeout_recovery() {
        let cfg = StateRoiConfig {
            crop: [0.2, 0.2, 0.8, 0.8],
            classes: vec!["closed".to_string(), "open".to_string()],
            interval_sec: 5,
            hysteresis: 3,
            anomaly_state: Some("open".to_string()),
            anomaly_timeout_sec: 60,
        };
        let mut tracker = RoiStateTracker::new("gate", cfg);
        assert_eq!(tracker.current_state(), "closed");

        // 1. Initial baseline "closed" observation
        let t0 = tracker.observe_state("closed", 0.92, 100.0);
        assert_eq!(t0.current_state, "closed");
        assert!(!t0.state_changed);
        assert!(!t0.is_anomalous);
        assert!(!t0.anomaly_triggered);

        // 2. Single-frame and 2-frame flicker to "open" must NOT transition state (hysteresis = 3)
        let f1 = tracker.observe_state("open", 0.88, 105.0);
        assert_eq!(f1.current_state, "closed");
        assert!(!f1.state_changed);
        assert_eq!(tracker.pending_state(), Some("open"));
        assert_eq!(tracker.pending_count(), 1);

        let f2 = tracker.observe_state("open", 0.90, 110.0);
        assert_eq!(f2.current_state, "closed");
        assert!(!f2.state_changed);
        assert_eq!(tracker.pending_count(), 2);

        // Intervening "closed" resets pending counter to 0
        let f_reset = tracker.observe_state("closed", 0.95, 115.0);
        assert_eq!(f_reset.current_state, "closed");
        assert!(!f_reset.state_changed);
        assert_eq!(tracker.pending_count(), 0);
        assert!(tracker.pending_state().is_none());

        // 3. Three consecutive "open" frames transition CLOSED -> OPEN on the 3rd frame
        assert!(!tracker.observe_state("open", 0.89, 120.0).state_changed);
        assert!(!tracker.observe_state("open", 0.91, 125.0).state_changed);
        let tr_open = tracker.observe_state("open", 0.93, 130.0);
        assert!(tr_open.state_changed);
        assert_eq!(tr_open.previous_state.as_deref(), Some("closed"));
        assert_eq!(tr_open.current_state, "open");
        assert!(tr_open.is_anomalous);
        assert!(!tr_open.anomaly_triggered);

        // 4. Anomaly timer recovery before timeout (returns to "closed" after 40s < 60s timeout)
        let mid_open = tracker.observe_state("open", 0.93, 170.0);
        assert_eq!(mid_open.anomaly_elapsed_sec, 40.0);
        assert!(!mid_open.anomaly_triggered);

        assert!(!tracker.observe_state("closed", 0.94, 175.0).state_changed);
        assert!(!tracker.observe_state("closed", 0.94, 180.0).state_changed);
        let tr_closed = tracker.observe_state("closed", 0.95, 185.0);
        assert!(tr_closed.state_changed);
        assert_eq!(tr_closed.current_state, "closed");
        assert!(!tr_closed.is_anomalous);
        assert_eq!(tr_closed.anomaly_elapsed_sec, 0.0);

        // 5. Re-open at t=220 and sustain >= 60s -> triggers anomaly alert exactly once at t=280
        let _ = tracker.observe_state("open", 0.90, 210.0);
        let _ = tracker.observe_state("open", 0.90, 215.0);
        let reopened = tracker.observe_state("open", 0.92, 220.0);
        assert!(reopened.state_changed);
        assert_eq!(reopened.current_state, "open");
        assert!(!reopened.anomaly_triggered);

        let before_timeout = tracker.observe_state("open", 0.92, 279.0);
        assert!(!before_timeout.anomaly_triggered);

        let at_timeout = tracker.observe_state("open", 0.92, 280.0);
        assert!(at_timeout.anomaly_triggered);
        assert_eq!(at_timeout.anomaly_elapsed_sec, 60.0);

        // Subsequent observations while still open do not re-emit duplicate alert
        let after_timeout = tracker.observe_state("open", 0.92, 300.0);
        assert!(!after_timeout.anomaly_triggered);
        assert!(after_timeout.is_anomalous);
    }

    #[test]
    fn classify_roi_crop_and_monitor_evaluate_interval_and_motion_settle() {
        let w = 160u32;
        let h = 120u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;

        let mut rois = BTreeMap::new();
        rois.insert(
            "gate".to_string(),
            StateRoiConfig {
                crop: [0.25, 0.25, 0.75, 0.75],
                classes: vec!["closed".to_string(), "open".to_string()],
                interval_sec: 10,
                hysteresis: 2,
                anomaly_state: Some("open".to_string()),
                anomaly_timeout_sec: 120,
            },
        );
        let mut monitor = CameraRoiStateMonitor::new(&rois);
        assert!(!monitor.is_empty());
        assert_eq!(
            monitor.current_states().get("gate").map(String::as_str),
            Some("closed")
        );

        // Dark uniform closed frame
        let closed_yuv = vec![35u8; y_len + uv_len * 2];
        // Bright high-contrast open frame inside ROI
        let mut open_yuv = vec![35u8; y_len + uv_len * 2];
        for y in 30..90 {
            for x in 40..120 {
                open_yuv[y * (w as usize) + x] = if (x + y) % 2 == 0 { 235 } else { 180 };
            }
        }

        // First evaluation at t=100.0 runs immediately
        let tr1 = monitor.evaluate_frame(&closed_yuv, w, h, 100.0, &[], false);
        assert_eq!(tr1.len(), 1);
        assert_eq!(tr1[0].current_state, "closed");

        // At t=103.0 (< 10s interval) with no motion settle, evaluation is skipped
        let tr_skip = monitor.evaluate_frame(&open_yuv, w, h, 103.0, &[], false);
        assert!(tr_skip.is_empty());

        // Motion inside ROI at t=104.0 followed by motion settling at t=105.0 triggers immediate evaluation
        let roi_motion = vec![BoundingBox::new(50, 40, 100, 80)];
        let _ = monitor.evaluate_frame(&open_yuv, w, h, 104.0, &roi_motion, false);
        let tr_settle = monitor.evaluate_frame(&open_yuv, w, h, 105.0, &[], false);
        assert_eq!(tr_settle.len(), 1);
        assert_eq!(tr_settle[0].current_state, "closed"); // 1st of 2 hysteresis frames

        // Next interval at t=115.0 completes 2-frame hysteresis -> transitions to "open"
        let tr_confirm = monitor.evaluate_frame(&open_yuv, w, h, 115.0, &[], false);
        assert_eq!(tr_confirm.len(), 1);
        assert!(tr_confirm[0].state_changed);
        assert_eq!(tr_confirm[0].current_state, "open");
        assert_eq!(
            monitor.current_states().get("gate").map(String::as_str),
            Some("open")
        );
    }

    #[test]
    fn bounding_box_geometry_methods_normalize_and_compute_overlap_metrics() {
        // Reversed coordinates are automatically sorted on construction
        let b = BoundingBox::new(300, 400, 100, 200);
        assert_eq!(b.x_min, 100);
        assert_eq!(b.y_min, 200);
        assert_eq!(b.x_max, 300);
        assert_eq!(b.y_max, 400);
        assert_eq!(b.width(), 200);
        assert_eq!(b.height(), 200);
        assert_eq!(b.area(), 40_000);
        assert_eq!(b.center(), (200.0, 300.0));
        assert_eq!(b.bottom_center(), (200.0, 400.0));

        // Identical boxes -> IoU = 1.0, intersection_over_min = 1.0
        assert!((b.iou(&b) - 1.0).abs() < 1e-6);
        assert!((b.intersection_over_min(&b) - 1.0).abs() < 1e-6);

        // Nested box -> inside = true, intersection_over_min = 1.0, IoU = 0.25
        let inner = BoundingBox::new(150, 250, 250, 350);
        assert!(inner.inside(&b));
        assert!(!b.inside(&inner));
        assert!(b.intersects(&inner));
        assert!((inner.intersection_over_min(&b) - 1.0).abs() < 1e-6);
        assert!((inner.iou(&b) - 0.25).abs() < 1e-6);
        assert_eq!(inner.union(&b), b);

        // Edge-touching / disjoint boxes -> intersects = false, IoU = 0.0
        let touching = BoundingBox::new(300, 200, 400, 400);
        assert!(!b.intersects(&touching));
        assert_eq!(b.iou(&touching), 0.0);
        assert_eq!(b.intersection_over_min(&touching), 0.0);

        // Zero-area degenerate box -> IoU = 0.0, intersection_over_min = 0.0
        let zero = BoundingBox::new(150, 250, 150, 250);
        assert_eq!(zero.area(), 0);
        assert_eq!(zero.iou(&b), 0.0);
        assert_eq!(zero.intersection_over_min(&b), 0.0);

        // Normalized [x, y, w, h] and zero-dimension frame safety
        let norm = b.to_normalized(1000, 1000);
        assert!((norm[0] - 0.1).abs() < 1e-6);
        assert!((norm[1] - 0.2).abs() < 1e-6);
        assert!((norm[2] - 0.2).abs() < 1e-6);
        assert!((norm[3] - 0.2).abs() < 1e-6);
        let norm_zero = b.to_normalized(0, 0);
        assert_eq!(norm_zero, [100.0, 200.0, 200.0, 200.0]);
    }

    #[test]
    fn compute_inference_regions_merges_proximate_boxes_and_clamps_square_crops() {
        assert!(compute_inference_regions(1920, 1080, 320, &[], &[]).is_empty());

        // Motion box completely inside a tracked box is deduplicated into a single crop
        let tracked = [BoundingBox::new(100, 100, 260, 260)];
        let inside_motion = [BoundingBox::new(120, 120, 200, 200)];
        let regs_dedup = compute_inference_regions(1920, 1080, 320, &inside_motion, &tracked);
        assert_eq!(regs_dedup.len(), 1);
        assert_eq!(regs_dedup[0].width(), 320);
        assert_eq!(regs_dedup[0].height(), 320);

        // Two proximate motion boxes within pad (320 / 8 = 40px) merge into 1 region,
        // while a distant box near the bottom-right corner produces a second clamped region
        let motion = [
            BoundingBox::new(100, 100, 180, 180),
            BoundingBox::new(205, 110, 270, 190),
            BoundingBox::new(1820, 980, 1915, 1075),
        ];
        let regs = compute_inference_regions(1920, 1080, 320, &motion, &[]);
        assert_eq!(regs.len(), 2);
        for r in &regs {
            assert!(r.width() >= 320);
            assert_eq!(r.width(), r.height());
            assert_eq!(r.width() % 4, 0);
            assert!(r.x_max <= 1920);
            assert!(r.y_max <= 1080);
        }
    }

    #[test]
    fn regression_odd_and_zero_yuv_dimensions_never_panic_across_vision_functions() {
        for (w, h) in [
            (0u32, 0u32),
            (0, 180),
            (320, 0),
            (1, 1),
            (2, 1),
            (1, 4),
            (641, 2),
            (641, 361),
        ] {
            let y_len = (w as usize) * (h as usize);
            let uv_len = y_len / 4;
            let buf = vec![128u8; y_len + uv_len * 2];
            let frame = YuvFrame {
                camera: Arc::from("cam"),
                timestamp: 1.0,
                width: w,
                height: h,
                data: Arc::new(buf.clone()),
            };
            let region = BoundingBox::new(0, 0, w.max(10), h.max(10));
            let mut tensor = vec![255u8; 16 * 16 * 3];
            crop_yuv420p_to_tensor_nhwc(&frame, &region, 16, 16, false, &mut tensor);

            let img = yuv420p_to_rgb_image_scaled(
                &frame,
                Some(region),
                &[(region, [255, 0, 0])],
                Some(90),
            );
            assert!(img.width() > 0 && img.height() > 0);

            let _ = classify_dominant_color_tag(&buf, w, h, region);
            let _ = is_monochrome_ir(&buf, w, h);
            let _ = check_frame_tamper(&buf, w, h);
            let _ = compute_crop_dhash64(&buf, w, h, region);

            let mut det = SimdMotionDetector::new(w, h, MotionConfig::default());
            let _ = det.detect(&frame);
        }
        assert!(
            compute_inference_regions(0, 0, 320, &[BoundingBox::new(0, 0, 50, 50)], &[]).is_empty()
        );
    }

    #[test]
    fn regression_polygon_mask_rejects_nan_inf_collinear_and_malformed_tokens() {
        // Malformed token or NaN/Inf must reject the entire polygon instead of shifting coordinates
        for invalid in [
            "0.1,0.1,NaN,0.1,0.5,0.5,0.1,0.5",
            "0.1,0.1,inf,0.1,0.5,0.5,0.1,0.5",
            "0.1,0.1,bad,0.1,0.5,0.5,0.1,0.5",
            "10,10,20,20,30,30",
        ] {
            let m = PolygonMask::from_config_value(&serde_json::json!(invalid), 100, 100);
            assert!(
                m.is_empty(),
                "expected invalid polygon '{invalid}' to be rejected"
            );
        }

        // point_in_polygon safely handles non-finite queries and vertices and negative small dy (-1e-7)
        let sq = [(10.0, 10.0), (90.0, 10.0), (90.0, 90.0), (10.0, 90.0)];
        assert!(!point_in_polygon(f32::NAN, 50.0, &sq));
        assert!(!point_in_polygon(50.0, f32::INFINITY, &sq));
        assert!(!point_in_polygon(
            50.0,
            50.0,
            &[(10.0, 10.0), (f32::NAN, 10.0), (50.0, 50.0)]
        ));

        // Pixelated privacy mask with extreme block_size (u32::MAX) must not overflow
        let mut img = RgbImage::from_pixel(32, 32, Rgb([200, 100, 50]));
        let valid_mask = PolygonMask::from_config_value(
            &serde_json::json!("0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"),
            32,
            32,
        );
        apply_privacy_mask_pixelated_rgb(&mut img, &valid_mask, u32::MAX);
        assert_eq!(*img.get_pixel(16, 16), Rgb([200, 100, 50]));
    }

    #[test]
    fn regression_roi_crop_and_state_tracker_resist_nan_and_backward_clock_jumps() {
        let nan_bbox = resolve_roi_crop_bbox([f32::NAN, 0.2, 0.8, 0.8], 640, 360);
        assert_eq!(nan_bbox, BoundingBox::new(0, 0, 640, 360));

        // Negative pixel crop is not misclassified as normalized (0..=1)
        let neg_bbox = resolve_roi_crop_bbox([-20.0, -10.0, 100.0, 80.0], 640, 360);
        assert_eq!(neg_bbox, BoundingBox::new(0, 0, 100, 80));

        let cfg = StateRoiConfig {
            crop: [0.2, 0.2, 0.8, 0.8],
            classes: vec!["closed".to_string(), "open".to_string()],
            interval_sec: 10,
            hysteresis: 1,
            anomaly_state: Some("open".to_string()),
            anomaly_timeout_sec: 30,
        };
        let mut tracker = RoiStateTracker::new("door", cfg);
        assert!(!tracker.should_evaluate(f64::NAN, false));
        let _ = tracker.observe_state("open", f32::NAN, 1000.0);
        // Clock jumps backward to 500.0 -> should_evaluate immediately recovers instead of stalling 500s
        assert!(tracker.should_evaluate(500.0, false));
        let tr = tracker.observe_state("open", 0.9, 500.0);
        assert!(tr.confidence.is_finite());
        let tr_timeout = tracker.observe_state("open", 0.9, 531.0);
        assert!(tr_timeout.anomaly_triggered);
    }

    #[test]
    fn regression_extreme_dimensions_oversized_crops_and_bright_contrast_caps_stay_safe() {
        // 1. resolve_roi_crop_bbox at u32::MAX and >2^24 dimensions never overflows x_min + 1 or returns 0-width box
        let max_bbox = resolve_roi_crop_bbox([1.0, 1.0, 1.0, 1.0], u32::MAX, u32::MAX);
        assert_eq!(max_bbox.x_min, u32::MAX - 1);
        assert_eq!(max_bbox.x_max, u32::MAX);
        assert_eq!(max_bbox.width(), 1);
        assert_eq!(max_bbox.height(), 1);

        let f32_gap_bbox = resolve_roi_crop_bbox([1.0, 1.0, 1.0, 1.0], 16_777_220, 33_554_439);
        assert!(f32_gap_bbox.width() >= 1);
        assert!(f32_gap_bbox.height() >= 1);
        assert!(f32_gap_bbox.x_max <= 16_777_220);
        assert!(f32_gap_bbox.y_max <= 33_554_439);

        // 2. yuv420p_to_rgb_image_scaled clamps oversized crop (e.g. u32::MAX) to frame dimensions instead of OOM/panic
        let w = 64u32;
        let h = 48u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: w,
            height: h,
            data: Arc::new(vec![128u8; y_len + uv_len * 2]),
        };
        let huge_crop = BoundingBox::new(0, 0, u32::MAX, u32::MAX);
        let img = yuv420p_to_rgb_image_scaled(&frame, Some(huge_crop), &[], None);
        assert_eq!(img.width(), w);
        assert_eq!(img.height(), h);

        // 3. crop_yuv420p_to_tensor_nhwc safely handles overflowing model dimensions without panicking
        let mut small_tensor = [255u8; 12];
        crop_yuv420p_to_tensor_nhwc(
            &frame,
            &huge_crop,
            u32::MAX,
            u32::MAX,
            false,
            &mut small_tensor,
        );
        assert_eq!(small_tensor, [0u8; 12]);

        // 4. SimdMotionDetector enforces 3x max contrast gain cap in bright scenes (luma 250..252) and threshold=0 floor
        let cfg = MotionConfig {
            threshold: 0,
            improve_contrast: true,
            ..MotionConfig::default()
        };
        let mut det = SimdMotionDetector::new(64, 48, cfg);
        let mut bright_buf = vec![250u8; y_len + uv_len * 2];
        bright_buf[y_len..].fill(128);
        let bright_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: w,
            height: h,
            data: Arc::new(bright_buf.clone()),
        };
        for _ in 0..32 {
            assert!(det.detect(&bright_frame).is_empty());
        }
        // 1-unit compression ripple (250 -> 251) in bright scene must not trigger false motion
        bright_buf[10 * 64 + 10] = 251;
        let ripple_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 2.0,
            width: w,
            height: h,
            data: Arc::new(bright_buf),
        };
        let _ = det.detect(&ripple_frame);
        assert!(det.detect(&ripple_frame).is_empty());
    }

    #[test]
    fn regression_disabled_motion_masked_lightning_flash_inverted_boxes_and_nan_roi_init() {
        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let dark_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 1.0,
            width: w,
            height: h,
            data: Arc::new(vec![30u8; y_len + uv_len * 2]),
        };
        let flash_frame = YuvFrame {
            camera: Arc::from("cam"),
            timestamp: 2.0,
            width: w,
            height: h,
            data: Arc::new(vec![230u8; y_len + uv_len * 2]),
        };

        // 1. MotionConfig { enabled: false } suppresses motion detection completely
        let disabled_cfg = MotionConfig {
            enabled: false,
            delta_alpha: 1.0,
            ..MotionConfig::default()
        };
        let mut disabled_det = SimdMotionDetector::new(w, h, disabled_cfg);
        for _ in 0..35 {
            assert!(disabled_det.detect(&dark_frame).is_empty());
        }
        assert!(disabled_det.detect(&flash_frame).is_empty());

        // 2. Lightning flash rejection works even when 50% of the frame is covered by a motion mask
        let masked_flash_cfg = MotionConfig {
            enabled: true,
            lightning_threshold: 0.80,
            improve_contrast: false,
            delta_alpha: 1.0,
            mask: serde_json::json!("0.0,0.0,1.0,0.0,1.0,0.5,0.0,0.5"),
            ..MotionConfig::default()
        };
        let mut masked_det = SimdMotionDetector::new(w, h, masked_flash_cfg);
        for _ in 0..31 {
            assert!(masked_det.detect(&dark_frame).is_empty());
        }
        assert!(
            masked_det.detect(&flash_frame).is_empty(),
            "full-frame flash across unmasked region must still be rejected when 50% of frame is masked"
        );

        // 3. Inverted or zero-area BoundingBox never falsely intersects, and out-of-bounds inference boxes clamp safely
        let inverted = BoundingBox {
            x_min: 300,
            y_min: 200,
            x_max: 100,
            y_max: 50,
        };
        let zero_box = BoundingBox::new(150, 100, 150, 100);
        let container = BoundingBox::new(0, 0, 640, 360);
        assert!(!inverted.intersects(&container));
        assert!(!zero_box.intersects(&container));
        assert!(inverted.inside(&container));

        let oob_motion = [BoundingBox::new(5000, 5000, 6000, 6000)];
        assert!(compute_inference_regions(640, 360, 320, &oob_motion, &[]).is_empty());

        // 4. RoiStateTracker initialized with a NaN timestamp does not falsely trigger timeout on first finite timestamp
        let roi_cfg = StateRoiConfig {
            crop: [0.2, 0.2, 0.8, 0.8],
            classes: vec!["closed".to_string(), "open".to_string()],
            interval_sec: 10,
            hysteresis: 1,
            anomaly_state: Some("open".to_string()),
            anomaly_timeout_sec: 30,
        };
        let mut roi_tracker = RoiStateTracker::new("garage", roi_cfg);
        let tr_nan = roi_tracker.observe_state("open", 0.95, f64::NAN);
        assert!(!tr_nan.anomaly_triggered);
        let tr_first_valid = roi_tracker.observe_state("open", 0.95, 1_700_000_000.0);
        assert!(
            !tr_first_valid.anomaly_triggered,
            "first finite timestamp after NaN must anchor state_since_ts instead of triggering immediately"
        );
        assert_eq!(tr_first_valid.anomaly_elapsed_sec, 0.0);
        let tr_after_timeout = roi_tracker.observe_state("open", 0.95, 1_700_000_031.0);
        assert!(tr_after_timeout.anomaly_triggered);
    }
}
