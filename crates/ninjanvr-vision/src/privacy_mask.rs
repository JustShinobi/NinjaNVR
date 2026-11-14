//! Canonical privacy-mask policy parsing, normalization, SHA-256 content hashing,
//! RGB image redaction, and FFmpeg `drawbox` video filter generation.

use image::RgbImage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    PolygonMask, apply_privacy_mask_pixelated_rgb, apply_privacy_mask_rgb,
    apply_privacy_masks_from_value,
};

pub const MAX_PRIVACY_MASK_DEPTH: usize = 8;
pub const MAX_PRIVACY_POLYGONS: usize = 64;
pub const MAX_PRIVACY_POLYGON_COORDS: usize = 512;
pub const DEFAULT_PIXELATE_BLOCK_SIZE: u32 = 8;

/// Rendering mode applied when rasterizing an active privacy mask onto RGB frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivacyMaskStyle {
    SolidFill,
    Pixelate { block_size: u32 },
}

/// Validated non-degenerate polygon with `>= 3` vertices.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedMaskPolygon {
    /// Normalized `[0.0..=1.0]` vertices (at least 3 points).
    pub vertices: Vec<(f64, f64)>,
    /// Retained only when parsed with unknown `(0, 0)` source resolution from pixel coordinates
    /// so `ffmpeg_drawbox_filter()` emits exact pixel `drawbox=x=...:y=...` expressions and
    /// `redact_rgb()` normalizes against the target `RgbImage`'s `(width, height)`.
    raw_pixel_vertices: Option<Vec<(f64, f64)>>,
}

impl NormalizedMaskPolygon {
    /// Formats normalized `[0.0..=1.0]` vertices as a comma-separated string (`x1,y1,x2,y2,...`).
    pub fn to_coord_string(&self) -> String {
        let mut parts = Vec::with_capacity(self.vertices.len() * 2);
        for &(x, y) in &self.vertices {
            parts.push(format!("{x:.6}"));
            parts.push(format!("{y:.6}"));
        }
        parts.join(",")
    }

    fn normalized_for_dimensions(&self, w: u32, h: u32) -> Option<String> {
        let Some(ref raw_pts) = self.raw_pixel_vertices else {
            return Some(self.to_coord_string());
        };
        let fw = w.max(1) as f64;
        let fh = h.max(1) as f64;
        let mut pts = Vec::with_capacity(raw_pts.len());
        for &(rx, ry) in raw_pts {
            pts.push(((rx / fw).clamp(0.0, 1.0), (ry / fh).clamp(0.0, 1.0)));
        }
        if !validate_normalized_polygon(&pts) {
            return None;
        }
        let mut parts = Vec::with_capacity(pts.len() * 2);
        for (x, y) in pts {
            parts.push(format!("{x:.6}"));
            parts.push(format!("{y:.6}"));
        }
        Some(parts.join(","))
    }

    fn ffmpeg_drawbox_expr(&self) -> Option<String> {
        if let Some(ref raw_pts) = self.raw_pixel_vertices {
            let mut raw_x_min = f64::MAX;
            let mut raw_y_min = f64::MAX;
            let mut raw_x_max = f64::MIN;
            let mut raw_y_max = f64::MIN;
            for &(px, py) in raw_pts {
                raw_x_min = raw_x_min.min(px);
                raw_y_min = raw_y_min.min(py);
                raw_x_max = raw_x_max.max(px);
                raw_y_max = raw_y_max.max(py);
            }
            let x_min = raw_x_min.clamp(0.0, 65_535.0).floor() as u32;
            let y_min = raw_y_min.clamp(0.0, 65_535.0).floor() as u32;
            let x_max = raw_x_max.clamp(0.0, 65_535.0).ceil() as u32;
            let y_max = raw_y_max.clamp(0.0, 65_535.0).ceil() as u32;
            if x_max <= x_min + 1 || y_max <= y_min + 1 {
                return None;
            }
            return Some(format!(
                "drawbox=x={x_min}:y={y_min}:w={}:h={}:color=#0c0e12@1.0:t=fill",
                x_max - x_min,
                y_max - y_min
            ));
        }

        let mut x_min = f64::MAX;
        let mut y_min = f64::MAX;
        let mut x_max = f64::MIN;
        let mut y_max = f64::MIN;
        for &(px, py) in &self.vertices {
            x_min = x_min.min(px);
            y_min = y_min.min(py);
            x_max = x_max.max(px);
            y_max = y_max.max(py);
        }
        let x_min = x_min.clamp(0.0, 1.0);
        let y_min = y_min.clamp(0.0, 1.0);
        let x_max = x_max.clamp(0.0, 1.0);
        let y_max = y_max.clamp(0.0, 1.0);
        let w = x_max - x_min;
        let h = y_max - y_min;
        if w < 0.001 || h < 0.001 {
            return None;
        }
        Some(format!(
            "drawbox=x=iw*{x_min:.3}:y=ih*{y_min:.3}:w=iw*{w:.3}:h=ih*{h:.3}:color=#0c0e12@1.0:t=fill"
        ))
    }
}

/// Active normalized privacy mask specification containing `>= 1` non-degenerate polygons and style.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedMaskSpec {
    pub polygons: Vec<NormalizedMaskPolygon>,
    pub style: PrivacyMaskStyle,
}

/// Canonical three-state privacy mask policy.
#[derive(Debug, Clone, PartialEq)]
pub enum PrivacyMaskPolicy {
    Disabled,
    Active(NormalizedMaskSpec),
    InvalidFailClosed(&'static str),
}

#[derive(Debug, Clone, Copy, Default)]
struct StyleAccumulator {
    pixelate: bool,
    block_size: Option<u32>,
}

impl StyleAccumulator {
    fn into_style(self) -> PrivacyMaskStyle {
        if self.pixelate {
            PrivacyMaskStyle::Pixelate {
                block_size: self.block_size.unwrap_or(DEFAULT_PIXELATE_BLOCK_SIZE),
            }
        } else {
            PrivacyMaskStyle::SolidFill
        }
    }
}

fn validate_normalized_polygon(pts: &[(f64, f64)]) -> bool {
    if pts.len() < 3 {
        return false;
    }
    let mut x_min = f64::MAX;
    let mut y_min = f64::MAX;
    let mut x_max = f64::MIN;
    let mut y_max = f64::MIN;
    let mut area2 = 0.0f64;
    for i in 0..pts.len() {
        let (x1, y1) = pts[i];
        let (x2, y2) = pts[(i + 1) % pts.len()];
        if !x1.is_finite() || !y1.is_finite() {
            return false;
        }
        x_min = x_min.min(x1);
        y_min = y_min.min(y1);
        x_max = x_max.max(x1);
        y_max = y_max.max(y1);
        area2 += x1 * y2 - x2 * y1;
    }
    area2.is_finite() && (x_max - x_min) >= 0.001 && (y_max - y_min) >= 0.001 && area2.abs() > 1e-5
}

fn expand_2pt_bbox(pts: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    if pts.len() == 2 {
        let (x1, y1) = pts[0];
        let (x2, y2) = pts[1];
        vec![(x1, y1), (x2, y1), (x2, y2), (x1, y2)]
    } else {
        pts
    }
}

fn parse_coords_slice(
    nums: &[f64],
    src_w: u32,
    src_h: u32,
) -> Result<NormalizedMaskPolygon, &'static str> {
    if nums.len() < 4 {
        return Err("insufficient_coordinates");
    }
    if !nums.len().is_multiple_of(2) {
        return Err("odd_coordinate_count");
    }
    if nums.len() > MAX_PRIVACY_POLYGON_COORDS {
        return Err("polygon_vertex_overflow");
    }
    if nums.iter().any(|v| !v.is_finite()) {
        return Err("non_finite_coordinate");
    }

    let has_explicit_resolution = src_w > 0 && src_h > 0;
    if has_explicit_resolution && nums.iter().any(|&v| v < 0.0) {
        return Err("negative_coordinate");
    }

    let mut raw_x_min = f64::MAX;
    let mut raw_y_min = f64::MAX;
    let mut raw_x_max = f64::MIN;
    let mut raw_y_max = f64::MIN;
    for &[rx, ry] in nums.as_chunks::<2>().0 {
        raw_x_min = raw_x_min.min(rx);
        raw_y_min = raw_y_min.min(ry);
        raw_x_max = raw_x_max.max(rx);
        raw_y_max = raw_y_max.max(ry);
    }

    let is_unit_norm = nums.iter().all(|&v| (0.0..=1.0).contains(&v));
    let is_overhang_norm = !has_explicit_resolution
        && raw_x_min >= -1.0
        && raw_y_min >= -1.0
        && raw_x_max <= 2.0
        && raw_y_max <= 2.0;

    if is_unit_norm || is_overhang_norm {
        let mut pts = Vec::with_capacity(nums.len() / 2);
        for &[rx, ry] in nums.as_chunks::<2>().0 {
            pts.push((rx.clamp(0.0, 1.0), ry.clamp(0.0, 1.0)));
        }
        let expanded = expand_2pt_bbox(pts);
        if !validate_normalized_polygon(&expanded) {
            return Err("degenerate_polygon");
        }
        return Ok(NormalizedMaskPolygon {
            vertices: expanded,
            raw_pixel_vertices: None,
        });
    }

    if raw_x_max <= 2.0 || raw_y_max <= 2.0 {
        return Err("out_of_bounds_coordinates");
    }

    if !has_explicit_resolution {
        let mut clamped_px = Vec::with_capacity(nums.len() / 2);
        for &[rx, ry] in nums.as_chunks::<2>().0 {
            clamped_px.push((rx.clamp(0.0, 65_535.0), ry.clamp(0.0, 65_535.0)));
        }
        let expanded_px = expand_2pt_bbox(clamped_px);
        let mut px_min = f64::MAX;
        let mut py_min = f64::MAX;
        let mut px_max = f64::MIN;
        let mut py_max = f64::MIN;
        let mut area2 = 0.0f64;
        for i in 0..expanded_px.len() {
            let (x1, y1) = expanded_px[i];
            let (x2, y2) = expanded_px[(i + 1) % expanded_px.len()];
            px_min = px_min.min(x1);
            py_min = py_min.min(y1);
            px_max = px_max.max(x1);
            py_max = py_max.max(y1);
            area2 += x1 * y2 - x2 * y1;
        }
        if (px_max.ceil() as u32) <= (px_min.floor() as u32) + 1
            || (py_max.ceil() as u32) <= (py_min.floor() as u32) + 1
            || area2.abs() <= 1.0
        {
            return Err("degenerate_polygon");
        }
        let norm_fallback: Vec<(f64, f64)> = expanded_px
            .iter()
            .map(|&(x, y)| ((x / 1280.0).clamp(0.0, 1.0), (y / 720.0).clamp(0.0, 1.0)))
            .collect();
        return Ok(NormalizedMaskPolygon {
            vertices: norm_fallback,
            raw_pixel_vertices: Some(expanded_px),
        });
    }

    let fw = src_w as f64;
    let fh = src_h as f64;
    let mut norm_pts = Vec::with_capacity(nums.len() / 2);
    for &[rx, ry] in nums.as_chunks::<2>().0 {
        norm_pts.push(((rx / fw).clamp(0.0, 1.0), (ry / fh).clamp(0.0, 1.0)));
    }
    let expanded = expand_2pt_bbox(norm_pts);
    if !validate_normalized_polygon(&expanded) {
        return Err("degenerate_polygon");
    }
    Ok(NormalizedMaskPolygon {
        vertices: expanded,
        raw_pixel_vertices: None,
    })
}

fn is_style_metadata_key(key: &str) -> bool {
    matches!(key, "mode" | "style" | "pixelate" | "block_size")
}

fn is_empty_disabled_zone_container(key: &str, val: &Value) -> bool {
    matches!(key, "zones" | "points" | "coordinates")
        && match val {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            Value::Array(arr) => arr.is_empty(),
            _ => false,
        }
}

fn collect_mask_polygons(
    val: &Value,
    src_w: u32,
    src_h: u32,
    depth: usize,
    polys: &mut Vec<NormalizedMaskPolygon>,
    style: &mut StyleAccumulator,
) -> Result<(), &'static str> {
    if depth > MAX_PRIVACY_MASK_DEPTH {
        return Err("depth_overflow");
    }
    if polys.len() >= MAX_PRIVACY_POLYGONS {
        return Err("polygon_count_overflow");
    }

    match val {
        Value::Null | Value::Bool(false) => {
            if depth == 0 {
                Ok(())
            } else {
                Err("empty_nested_mask")
            }
        }
        Value::Bool(true) => Err("boolean_mask_invalid"),
        Value::Number(_) => Err("numeric_scalar_mask_invalid"),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return if depth == 0 {
                    Ok(())
                } else {
                    Err("empty_nested_mask")
                };
            }
            if trimmed == "invalid_depth_overflow" {
                return Err("depth_overflow");
            }
            if trimmed.starts_with("invalid_") {
                return Err("invalid_privacy_mask");
            }
            let mut nums = Vec::new();
            for part in trimmed.split(',') {
                if nums.len() >= MAX_PRIVACY_POLYGON_COORDS {
                    return Err("polygon_vertex_overflow");
                }
                let Ok(v) = part.trim().parse::<f64>() else {
                    return Err("malformed_coordinate_token");
                };
                if !v.is_finite() {
                    return Err("non_finite_coordinate");
                }
                nums.push(v);
            }
            let poly = parse_coords_slice(&nums, src_w, src_h)?;
            push_unique_polygon(polys, poly)?;
            Ok(())
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                return if depth == 0 {
                    Ok(())
                } else {
                    Err("empty_nested_mask")
                };
            }
            if arr.iter().all(Value::is_number) {
                if arr.len() > MAX_PRIVACY_POLYGON_COORDS {
                    return Err("polygon_vertex_overflow");
                }
                let mut nums = Vec::with_capacity(arr.len());
                for item in arr {
                    let Some(n) = item.as_f64() else {
                        return Err("non_finite_coordinate");
                    };
                    if !n.is_finite() {
                        return Err("non_finite_coordinate");
                    }
                    nums.push(n);
                }
                let poly = parse_coords_slice(&nums, src_w, src_h)?;
                push_unique_polygon(polys, poly)?;
                return Ok(());
            }
            if arr.iter().all(|v| {
                v.as_array()
                    .is_some_and(|p| p.len() == 2 && p[0].is_number() && p[1].is_number())
            }) {
                if arr.len() < 2 {
                    return Err("insufficient_coordinates");
                }
                if arr.len() * 2 > MAX_PRIVACY_POLYGON_COORDS {
                    return Err("polygon_vertex_overflow");
                }
                let mut nums = Vec::with_capacity(arr.len() * 2);
                for item in arr {
                    let Some(pair) = item.as_array() else {
                        return Err("malformed_vertex_pair");
                    };
                    let (Some(x), Some(y)) = (pair[0].as_f64(), pair[1].as_f64()) else {
                        return Err("non_finite_coordinate");
                    };
                    if !x.is_finite() || !y.is_finite() {
                        return Err("non_finite_coordinate");
                    }
                    nums.push(x);
                    nums.push(y);
                }
                let poly = parse_coords_slice(&nums, src_w, src_h)?;
                push_unique_polygon(polys, poly)?;
                return Ok(());
            }
            if arr.len() > MAX_PRIVACY_POLYGONS {
                return Err("polygon_count_overflow");
            }
            for item in arr {
                let before = polys.len();
                let mut item_polys = Vec::new();
                collect_mask_polygons(item, src_w, src_h, depth + 1, &mut item_polys, style)?;
                if item_polys.is_empty() {
                    return Err("empty_nested_mask");
                }
                for p in item_polys {
                    push_unique_polygon(polys, p)?;
                }
                let _ = before;
            }
            Ok(())
        }
        Value::Object(map) => {
            if map.is_empty() {
                return if depth == 0 {
                    Ok(())
                } else {
                    Err("empty_nested_mask")
                };
            }
            for (k, v) in map {
                match k.as_str() {
                    "mode" | "style" => {
                        let Some(s) = v.as_str() else {
                            return Err("invalid_style_field");
                        };
                        if s.eq_ignore_ascii_case("pixelate") || s.eq_ignore_ascii_case("pixelated")
                        {
                            style.pixelate = true;
                        }
                    }
                    "pixelate" => {
                        let Some(b) = v.as_bool() else {
                            return Err("invalid_pixelate_field");
                        };
                        if b {
                            style.pixelate = true;
                        }
                    }
                    "block_size" => {
                        let Some(n) = v
                            .as_u64()
                            .or_else(|| {
                                v.as_f64()
                                    .filter(|f| f.is_finite() && *f >= 1.0)
                                    .map(|f| f as u64)
                            })
                            .filter(|&b| b >= 1)
                        else {
                            return Err("invalid_block_size");
                        };
                        style.block_size = Some(n.min(u32::MAX as u64) as u32);
                    }
                    _ => {}
                }
            }

            let non_meta_keys: Vec<&String> = map
                .keys()
                .filter(|k| !is_style_metadata_key(k.as_str()))
                .collect();

            if non_meta_keys.is_empty() {
                return if depth == 0 {
                    Ok(())
                } else {
                    Err("empty_nested_mask")
                };
            }

            if depth == 0
                && non_meta_keys.iter().all(|k| {
                    map.get(*k)
                        .is_some_and(|v| is_empty_disabled_zone_container(k, v))
                })
            {
                return Ok(());
            }

            let has_explicit_zones = non_meta_keys.iter().any(|k| !k.starts_with("_norm_poly_"));

            let mut obj_added_any = false;
            for k in non_meta_keys {
                if has_explicit_zones && k.starts_with("_norm_poly_") {
                    continue;
                }
                let Some(v) = map.get(k) else {
                    continue;
                };
                let mut child_polys = Vec::new();
                collect_mask_polygons(v, src_w, src_h, depth + 1, &mut child_polys, style)?;
                if child_polys.is_empty() {
                    return Err("empty_nested_mask");
                }
                obj_added_any = true;
                for p in child_polys {
                    push_unique_polygon(polys, p)?;
                }
            }

            if !obj_added_any {
                return Err("empty_nested_mask");
            }
            Ok(())
        }
    }
}

fn push_unique_polygon(
    polys: &mut Vec<NormalizedMaskPolygon>,
    poly: NormalizedMaskPolygon,
) -> Result<(), &'static str> {
    let coord_s = poly.to_coord_string();
    let already_present = polys.iter().any(|existing| {
        existing.raw_pixel_vertices == poly.raw_pixel_vertices
            && existing.to_coord_string() == coord_s
    });
    if !already_present {
        if polys.len() >= MAX_PRIVACY_POLYGONS {
            return Err("polygon_count_overflow");
        }
        polys.push(poly);
    }
    Ok(())
}

impl PrivacyMaskPolicy {
    /// Parses and validates `mask_val` against `(src_w, src_h)` (or unscaled when `src_w == 0 || src_h == 0`).
    pub fn from_value(mask_val: &Value, src_w: u32, src_h: u32) -> Self {
        let mut polys = Vec::new();
        let mut style = StyleAccumulator::default();
        match collect_mask_polygons(mask_val, src_w, src_h, 0, &mut polys, &mut style) {
            Ok(()) if polys.is_empty() => Self::Disabled,
            Ok(()) => Self::Active(NormalizedMaskSpec {
                polygons: polys,
                style: style.into_style(),
            }),
            Err(reason) => Self::InvalidFailClosed(reason),
        }
    }

    /// Parses an optional `mask_val`, treating `None` as [`PrivacyMaskPolicy::Disabled`].
    pub fn from_optional_value(mask_val: Option<&Value>, src_w: u32, src_h: u32) -> Self {
        match mask_val {
            None => Self::Disabled,
            Some(val) => Self::from_value(val, src_w, src_h),
        }
    }

    /// Returns `true` when the policy is either [`PrivacyMaskPolicy::Active`] or [`PrivacyMaskPolicy::InvalidFailClosed`].
    #[inline]
    pub fn is_configured(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Returns `true` when the policy failed validation and must fail closed.
    #[inline]
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::InvalidFailClosed(_))
    }

    /// Converts the policy into a canonical `serde_json::Value` compatible with `PolygonMask`,
    /// `apply_privacy_masks_from_value`, and `build_privacy_drawbox_filter`.
    pub fn to_normalized_value(&self) -> Value {
        match self {
            Self::Disabled => Value::Null,
            Self::InvalidFailClosed(reason) => {
                if *reason == "depth_overflow" {
                    Value::String("invalid_depth_overflow".to_string())
                } else {
                    Value::String("invalid_privacy_mask".to_string())
                }
            }
            Self::Active(spec) => {
                let poly_strings: Vec<String> = spec
                    .polygons
                    .iter()
                    .map(NormalizedMaskPolygon::to_coord_string)
                    .collect();
                match spec.style {
                    PrivacyMaskStyle::SolidFill => {
                        Value::Array(poly_strings.into_iter().map(Value::String).collect())
                    }
                    PrivacyMaskStyle::Pixelate { block_size } => {
                        let mut out = serde_json::Map::new();
                        out.insert("mode".to_string(), Value::String("pixelate".to_string()));
                        out.insert("pixelate".to_string(), Value::Bool(true));
                        out.insert(
                            "block_size".to_string(),
                            Value::Number(serde_json::Number::from(block_size)),
                        );
                        out.insert(
                            "zones".to_string(),
                            Value::Array(poly_strings.iter().cloned().map(Value::String).collect()),
                        );
                        for (idx, poly_s) in poly_strings.into_iter().enumerate() {
                            out.insert(format!("_norm_poly_{idx}"), Value::String(poly_s));
                        }
                        Value::Object(out)
                    }
                }
            }
        }
    }

    /// Computes a deterministic SHA-256 hex digest of the canonical policy representation.
    pub fn content_hash(&self) -> String {
        let canonical = match self {
            Self::Disabled => Vec::new(),
            Self::InvalidFailClosed(reason) => format!("invalid:{reason}").into_bytes(),
            Self::Active(spec) => {
                let mut bytes = serde_json::to_vec(&self.to_normalized_value()).unwrap_or_default();
                if spec.polygons.iter().any(|p| p.raw_pixel_vertices.is_some()) {
                    bytes.extend_from_slice(b"|raw_px:");
                    for (idx, poly) in spec.polygons.iter().enumerate() {
                        if let Some(ref raw_pts) = poly.raw_pixel_vertices {
                            bytes.extend_from_slice(format!("{idx}:").as_bytes());
                            for &(rx, ry) in raw_pts {
                                bytes.extend_from_slice(format!("{rx:.3},{ry:.3};").as_bytes());
                            }
                        }
                    }
                }
                bytes
            }
        };
        let digest = Sha256::digest(&canonical);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Redacts a cropped `rgb` sub-image whose full-frame normalized `[x, y, w, h]` region is
    /// `norm_crop_xywh`, projecting full-frame mask polygons into the crop's local pixel space
    /// (or falling back to full-frame [`Self::redact_rgb`] if `norm_crop_xywh` is out of bounds).
    pub fn redact_rgb_crop(&self, rgb: &mut RgbImage, norm_crop_xywh: [f64; 4]) -> bool {
        if rgb.width() == 0 || rgb.height() == 0 {
            return false;
        }
        let [rx, ry, rw, rh] = norm_crop_xywh;
        let valid_crop = norm_crop_xywh.iter().all(|v| v.is_finite())
            && (0.0..1.0).contains(&rx)
            && (0.0..1.0).contains(&ry)
            && rw >= 0.005
            && rh >= 0.005
            && rx + rw <= 1.001
            && ry + rh <= 1.001;
        if !valid_crop {
            return self.redact_rgb(rgb);
        }
        match self {
            Self::InvalidFailClosed(_) => false,
            Self::Disabled => true,
            Self::Active(spec) => {
                let tw = rgb.width() as f64;
                let th = rgb.height() as f64;
                let mut projected = Vec::with_capacity(spec.polygons.len());
                for p in &spec.polygons {
                    let mut pts = Vec::with_capacity(p.vertices.len());
                    for &(nx, ny) in &p.vertices {
                        let px = (((nx - rx) / rw) * tw) as f32;
                        let py = (((ny - ry) / rh) * th) as f32;
                        if !px.is_finite() || !py.is_finite() {
                            return false;
                        }
                        pts.push((px, py));
                    }
                    projected.push(pts);
                }
                let poly_mask = PolygonMask::from_pixel_polygons(projected);
                if poly_mask.is_empty() {
                    return false;
                }
                match spec.style {
                    PrivacyMaskStyle::SolidFill => {
                        apply_privacy_mask_rgb(rgb, &poly_mask);
                    }
                    PrivacyMaskStyle::Pixelate { block_size } => {
                        apply_privacy_mask_pixelated_rgb(rgb, &poly_mask, block_size);
                    }
                }
                true
            }
        }
    }

    /// Redacts `rgb` in-place according to the policy, failing closed (`false`) if `InvalidFailClosed`,
    /// `rgb` has zero width/height, or no non-degenerate polygon covers `rgb`'s coordinate space.
    pub fn redact_rgb(&self, rgb: &mut RgbImage) -> bool {
        if rgb.width() == 0 || rgb.height() == 0 {
            return false;
        }
        match self {
            Self::InvalidFailClosed(_) => false,
            Self::Disabled => true,
            Self::Active(spec) => {
                let has_raw_px = spec.polygons.iter().any(|p| p.raw_pixel_vertices.is_some());
                let val_for_img = if has_raw_px {
                    let mut poly_strings = Vec::with_capacity(spec.polygons.len());
                    for p in &spec.polygons {
                        let Some(s) = p.normalized_for_dimensions(rgb.width(), rgb.height()) else {
                            return false;
                        };
                        poly_strings.push(s);
                    }
                    match spec.style {
                        PrivacyMaskStyle::SolidFill => {
                            Value::Array(poly_strings.into_iter().map(Value::String).collect())
                        }
                        PrivacyMaskStyle::Pixelate { block_size } => {
                            let mut out = serde_json::Map::new();
                            out.insert("mode".to_string(), Value::String("pixelate".to_string()));
                            out.insert("pixelate".to_string(), Value::Bool(true));
                            out.insert(
                                "block_size".to_string(),
                                Value::Number(serde_json::Number::from(block_size)),
                            );
                            for (idx, poly_s) in poly_strings.into_iter().enumerate() {
                                out.insert(format!("_norm_poly_{idx}"), Value::String(poly_s));
                            }
                            Value::Object(out)
                        }
                    }
                } else {
                    self.to_normalized_value()
                };

                let poly_mask =
                    PolygonMask::from_config_value(&val_for_img, rgb.width(), rgb.height());
                if poly_mask.is_empty() {
                    return false;
                }
                match spec.style {
                    PrivacyMaskStyle::SolidFill => {
                        apply_privacy_mask_rgb(rgb, &poly_mask);
                    }
                    PrivacyMaskStyle::Pixelate { block_size } => {
                        apply_privacy_mask_pixelated_rgb(rgb, &poly_mask, block_size);
                    }
                }
                let _ = apply_privacy_masks_from_value;
                true
            }
        }
    }

    /// Compiles an FFmpeg `drawbox` video filter chain (`Ok(None)` when disabled, `Err` when invalid).
    pub fn ffmpeg_drawbox_filter(&self) -> Result<Option<String>, &'static str> {
        match self {
            Self::Disabled => Ok(None),
            Self::InvalidFailClosed(reason) => Err(reason),
            Self::Active(spec) => {
                let mut filters = Vec::with_capacity(spec.polygons.len());
                for poly in &spec.polygons {
                    let Some(expr) = poly.ffmpeg_drawbox_expr() else {
                        return Err("degenerate_drawbox");
                    };
                    filters.push(expr);
                }
                if filters.is_empty() {
                    Err("empty_drawbox_filter")
                } else {
                    Ok(Some(filters.join(",")))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;
    use serde_json::json;

    #[test]
    fn disabled_variants_are_unconfigured_and_noop_on_rgb_and_drawbox() {
        for empty in [
            json!(null),
            json!(false),
            json!(""),
            json!("   "),
            json!([]),
            json!({}),
            json!({ "mode": "pixelate", "block_size": 12 }),
            json!({ "mode": "pixelate", "zones": [] }),
            json!({ "coordinates": "" }),
        ] {
            let policy = PrivacyMaskPolicy::from_value(&empty, 640, 360);
            assert_eq!(
                policy,
                PrivacyMaskPolicy::Disabled,
                "expected Disabled for {empty}"
            );
            assert!(!policy.is_configured());
            assert!(!policy.is_invalid());
            assert_eq!(policy.ffmpeg_drawbox_filter(), Ok(None));

            let mut img = RgbImage::from_pixel(32, 32, Rgb([210, 220, 230]));
            assert!(policy.redact_rgb(&mut img));
            assert_eq!(*img.get_pixel(16, 16), Rgb([210, 220, 230]));
        }
    }

    #[test]
    fn two_point_boxes_polygons_numeric_arrays_and_pixelate_roundtrip_identically() {
        let cases = [
            json!("160,90,480,270"),
            json!([160, 90, 480, 270]),
            json!([[160, 90], [480, 90], [480, 270], [160, 270]]),
            json!("0.25,0.25,0.75,0.75"),
            json!({
                "zones": ["160,90,480,90,480,270,160,270"]
            }),
        ];

        let expected_hash = PrivacyMaskPolicy::from_value(&cases[0], 640, 360).content_hash();
        for case in &cases {
            let policy = PrivacyMaskPolicy::from_value(case, 640, 360);
            assert!(
                policy.is_configured() && !policy.is_invalid(),
                "case={case}"
            );
            assert_eq!(
                policy.content_hash(),
                expected_hash,
                "hash mismatch for {case}"
            );

            let vf = policy
                .ffmpeg_drawbox_filter()
                .expect("valid filter")
                .expect("active filter");
            assert_eq!(
                vf,
                "drawbox=x=iw*0.250:y=ih*0.250:w=iw*0.500:h=ih*0.500:color=#0c0e12@1.0:t=fill"
            );

            // Idempotent re-parse from to_normalized_value()
            let norm = policy.to_normalized_value();
            let reparsed = PrivacyMaskPolicy::from_value(&norm, 1920, 1080);
            assert_eq!(reparsed.content_hash(), expected_hash);

            let mut img = RgbImage::from_pixel(100, 100, Rgb([240, 240, 240]));
            assert!(policy.redact_rgb(&mut img));
            assert_eq!(*img.get_pixel(50, 50), Rgb([12, 14, 18]));
            assert_eq!(*img.get_pixel(10, 10), Rgb([240, 240, 240]));
        }

        // Pixelate style preserves mode & block_size across normalization and RGB redaction
        let pix_policy = PrivacyMaskPolicy::from_value(
            &json!({
                "mode": "pixelate",
                "block_size": 8,
                "door": "16,16,48,48"
            }),
            64,
            64,
        );
        assert_ne!(pix_policy.content_hash(), expected_hash);
        let mut checker = RgbImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                let v = if (x + y) % 2 == 0 { 200u8 } else { 40u8 };
                checker.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        assert!(pix_policy.redact_rgb(&mut checker));
        assert_eq!(*checker.get_pixel(24, 24), Rgb([120, 120, 120]));
        assert_eq!(*checker.get_pixel(25, 24), Rgb([120, 120, 120]));
        assert_eq!(*checker.get_pixel(4, 4), Rgb([200, 200, 200]));
    }

    #[test]
    fn malformed_collinear_negative_and_deep_masks_fail_closed() {
        let mut deep = json!("0.1,0.1,0.5,0.5");
        for _ in 0..10 {
            deep = json!([deep]);
        }

        let invalid_cases = [
            json!(true),
            json!(42),
            json!("0.1,0.2"),
            json!("0.1,0.2,0.3"),
            json!("-10,10,100,100"),
            json!("0.1,0.1,NaN,0.5"),
            json!("10,10,20,20,30,30"),
            json!("0.1000,0.1000,0.1002,0.5000"),
            json!(["0.1,0.1,0.5,0.5", "bad_zone"]),
            json!([""]),
            json!({ "window": "0.1,0.1,0.5,0.5", "door": [] }),
            json!({ "mode": 123, "zones": ["0.1,0.1,0.5,0.5"] }),
            deep,
        ];

        for bad in &invalid_cases {
            let policy = PrivacyMaskPolicy::from_value(bad, 640, 360);
            assert!(
                policy.is_invalid() && policy.is_configured(),
                "expected InvalidFailClosed for {bad}, got {policy:?}"
            );
            assert!(policy.ffmpeg_drawbox_filter().is_err());

            let mut img = RgbImage::from_pixel(32, 32, Rgb([250, 250, 250]));
            assert!(!policy.redact_rgb(&mut img));
        }

        let valid = PrivacyMaskPolicy::from_value(&json!("0.1,0.1,0.5,0.5"), 640, 360);
        let mut zero_img = RgbImage::new(0, 0);
        assert!(!valid.redact_rgb(&mut zero_img));
    }

    #[test]
    fn raw_pixel_masks_parsed_without_resolution_produce_distinct_content_hashes() {
        let p_1080p = PrivacyMaskPolicy::from_value(&json!("1300,800,1850,1050"), 0, 0);
        let p_4k = PrivacyMaskPolicy::from_value(&json!("2000,1200,3400,2000"), 0, 0);
        assert!(matches!(p_1080p, PrivacyMaskPolicy::Active(_)));
        assert!(matches!(p_4k, PrivacyMaskPolicy::Active(_)));
        assert_ne!(
            p_1080p.content_hash(),
            p_4k.content_hash(),
            "distinct 1080p/4K pixel masks parsed at (0, 0) must never collide on SHA-256 content_hash"
        );
    }
}
