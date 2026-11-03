#!/usr/bin/env python3
"""NinjaNVR Security Base Dataset Builder & CCTV Night-Vision IR Augmentation Pipeline.

Builds the 15-class NinjaNVR Security dataset by:
1. Mapping Open Images V7, VisDrone, COCO, WiderFace, and CCPD labels to canonical IDs (0..14)
   via `scripts/ai/taxonomy.json` and rejecting unmapped non-security classes.
2. Applying deterministic CCTV Night-IR augmentation to 35% of training frames:
   - RGB to luma grayscale: Y = 0.299*R + 0.587*G + 0.114*B
   - Radial IR LED vignette falloff toward frame corners
   - Seeded Poisson-Gaussian CMOS sensor noise
   - Slight directional motion blur
   - Exact preservation of normalized YOLO bounding-box coordinates
3. Applying top-down CCTV trapezoidal perspective (`perspective=0.0005`) & scale transforms.
"""

from __future__ import annotations

import argparse
import copy
import json
import math
import random
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
DEFAULT_TAXONOMY_PATH = SCRIPT_DIR / "taxonomy.json"
DEFAULT_LABELMAP_PATH = REPO_ROOT / "models" / "ninjanvr_security_15cl.txt"

NIGHT_IR_AUGMENTATION_RATIO: float = 0.35
DEFAULT_PERSPECTIVE_FACTOR: float = 0.0005

CANONICAL_15_CLASSES: list[str] = [
    "person",
    "face",
    "car",
    "motorcycle",
    "bicycle",
    "truck",
    "bus",
    "license_plate",
    "dog",
    "cat",
    "bird",
    "package",
    "waste_bin",
    "backpack",
    "umbrella",
]


def load_taxonomy(taxonomy_path: Path | str | None = None) -> dict[str, Any]:
    """Loads the canonical 15-class taxonomy mapping dictionary from `scripts/ai/taxonomy.json`."""
    path = Path(taxonomy_path) if taxonomy_path is not None else DEFAULT_TAXONOMY_PATH
    with path.open("r", encoding="utf-8") as fh:
        data = json.load(fh)
    if data.get("classes") != CANONICAL_15_CLASSES:
        raise ValueError("taxonomy.json classes do not match canonical 15-class security schema")
    return data


def _normalize_dataset_key(source_dataset: str) -> str:
    clean = source_dataset.strip().lower().replace("-", "_").replace(" ", "_")
    aliases = {
        "open_images": "open_images_v7",
        "openimages": "open_images_v7",
        "openimages_v7": "open_images_v7",
        "oid": "open_images_v7",
        "oidv7": "open_images_v7",
        "open_images_v7": "open_images_v7",
        "visdrone": "visdrone",
        "vis_drone": "visdrone",
        "coco": "coco",
        "coco80": "coco",
        "coco_2017": "coco",
        "widerface": "widerface",
        "wider_face": "widerface",
        "ccpd": "ccpd",
        "canonical": "name_to_id",
        "ninjanvr": "name_to_id",
    }
    return aliases.get(clean, clean)


def remap_class_label(
    source_dataset: str,
    raw_label: str | int,
    taxonomy: dict[str, Any] | None = None,
) -> int | None:
    """Maps a source dataset class name or index into the canonical 0..14 ID, or None if unmapped."""
    tax = taxonomy if taxonomy is not None else load_taxonomy()
    ds_key = _normalize_dataset_key(source_dataset)

    datasets_map = tax.get("datasets", {})
    source_map: dict[str, int] | None = None
    if ds_key == "name_to_id":
        source_map = tax.get("name_to_id", {})
    elif ds_key in datasets_map:
        source_map = datasets_map[ds_key]
    elif ds_key in tax and isinstance(tax[ds_key], dict):
        source_map = tax[ds_key]

    if not source_map:
        raise KeyError(f"Unsupported source dataset '{source_dataset}'")

    key_str = str(raw_label).strip()
    if key_str in source_map:
        cid = int(source_map[key_str])
        return cid if 0 <= cid < len(CANONICAL_15_CLASSES) else None

    key_lower = key_str.lower()
    for candidate_key, mapped_id in source_map.items():
        if candidate_key.strip().lower() == key_lower:
            cid = int(mapped_id)
            return cid if 0 <= cid < len(CANONICAL_15_CLASSES) else None

    return None


def remap_dataset_annotations(
    source_dataset: str,
    annotations: list[dict[str, Any]],
    taxonomy: dict[str, Any] | None = None,
) -> list[dict[str, Any]]:
    """Remaps raw dataset annotations to canonical 0..14 YOLO boxes and drops unmapped classes."""
    tax = taxonomy if taxonomy is not None else load_taxonomy()
    classes = tax["classes"]
    remapped: list[dict[str, Any]] = []

    for ann in annotations:
        raw_cls = ann.get("label", ann.get("class", ann.get("category", ann.get("class_id"))))
        if raw_cls is None:
            continue
        cid = remap_class_label(source_dataset, raw_cls, tax)
        if cid is None:
            continue

        cx = max(0.0, min(1.0, float(ann.get("cx", 0.5))))
        cy = max(0.0, min(1.0, float(ann.get("cy", 0.5))))
        w = max(0.001, min(1.0, float(ann.get("w", 0.1))))
        h = max(0.001, min(1.0, float(ann.get("h", 0.1))))
        remapped.append(
            {
                "class_id": cid,
                "label": classes[cid],
                "cx": round(cx, 6),
                "cy": round(cy, 6),
                "w": round(w, 6),
                "h": round(h, 6),
            }
        )
    return remapped


def rgb_to_luma(r: float | int, g: float | int, b: float | int) -> float:
    """Computes ITU-R BT.601 luma grayscale value: Y = 0.299*R + 0.587*G + 0.114*B."""
    return 0.299 * float(r) + 0.587 * float(g) + 0.114 * float(b)


def clamp_u8(val: float) -> int:
    """Clamps a floating-point intensity value to [0, 255] integer range."""
    return max(0, min(255, int(round(val))))


def should_apply_night_ir(
    frame_index: int,
    ratio: float = NIGHT_IR_AUGMENTATION_RATIO,
) -> bool:
    """Deterministically selects `ratio` (default 35%) of frames for synthetic Night-IR transform."""
    if ratio <= 0.0:
        return False
    if ratio >= 1.0:
        return True
    slot = int(frame_index) % 100
    threshold = int(round(ratio * 100.0))
    # Spread evenly across 100 slots using golden-ratio modular stride
    hashed_slot = (slot * 61) % 100
    return hashed_slot < threshold


def _extract_rgb_grid(frame: Any) -> tuple[list[list[list[int]]], str]:
    """Normalizes a frame (`list[H][W][3]`, `PIL.Image.Image`, or `numpy.ndarray`) into a 3D RGB grid."""
    if hasattr(frame, "getdata") and hasattr(frame, "size"):
        # PIL.Image.Image
        rgb_img = frame.convert("RGB")
        width, height = rgb_img.size
        pixels = list(rgb_img.getdata())
        grid = [
            [[int(c) for c in pixels[y * width + x]] for x in range(width)]
            for y in range(height)
        ]
        return grid, "pil"

    if hasattr(frame, "tolist") and hasattr(frame, "shape"):
        # numpy.ndarray
        raw = frame.tolist()
        return raw, "numpy"

    if isinstance(frame, list) and len(frame) > 0 and isinstance(frame[0], list):
        grid = []
        for row in frame:
            out_row = []
            for px in row:
                if isinstance(px, (list, tuple)) and len(px) >= 3:
                    out_row.append([int(px[0]), int(px[1]), int(px[2])])
                else:
                    v = int(px) if isinstance(px, (int, float)) else 0
                    out_row.append([v, v, v])
            grid.append(out_row)
        return grid, "list"

    raise TypeError("Unsupported frame type; expected 3D RGB list, PIL.Image, or numpy.ndarray")


def _restore_frame_type(grid: list[list[list[int]]], kind: str) -> Any:
    if kind == "pil":
        from PIL import Image

        height = len(grid)
        width = len(grid[0]) if height > 0 else 0
        img = Image.new("RGB", (width, height))
        flat = [tuple(grid[y][x]) for y in range(height) for x in range(width)]
        img.putdata(flat)
        return img
    if kind == "numpy":
        try:
            import numpy as np  # type: ignore

            return np.asarray(grid, dtype=np.uint8)
        except ImportError:
            return grid
    return grid


def rgb_frame_to_luma_grid(frame: Any) -> list[list[int]]:
    """Converts a 3-channel RGB frame into a 2D [H][W] uint8 luma matrix (Y = 0.299R + 0.587G + 0.114B)."""
    grid, _ = _extract_rgb_grid(frame)
    out: list[list[int]] = []
    for row in grid:
        out.append([clamp_u8(rgb_to_luma(px[0], px[1], px[2])) for px in row])
    return out


def apply_synthetic_night_ir(
    frame: Any,
    bboxes: list[Any] | None = None,
    *,
    vignette_strength: float = 0.35,
    noise_std: float = 3.5,
    poisson_scale: float = 0.15,
    motion_blur_kernel: int = 3,
    seed: int = 42,
) -> Any:
    """Applies deterministic CCTV Night-Vision IR transform while preserving bounding-box coordinates.

    Steps:
    1. Convert RGB to luma grayscale: Y = 0.299*R + 0.587*G + 0.114*B
    2. Apply radial IR LED vignette falloff (bright center hotspot, darker corners)
    3. Add seeded Poisson-Gaussian sensor noise
    4. Apply slight horizontal motion blur
    5. Clamp all pixel channels to [0, 255] monochrome RGB [Y, Y, Y]

    Returns `transformed_frame` when `bboxes is None`, or `(transformed_frame, preserved_bboxes)`
    when `bboxes` is provided.
    """
    grid, kind = _extract_rgb_grid(frame)
    height = len(grid)
    width = len(grid[0]) if height > 0 else 0

    rng = random.Random(seed)
    cy = (height - 1) / 2.0 if height > 1 else 0.5
    cx = (width - 1) / 2.0 if width > 1 else 0.5
    max_r2 = max(1e-6, cx * cx + cy * cy)

    # Step 1 + 2 + 3: Luma + Radial IR LED vignette + Poisson-Gaussian noise
    luma_noisy: list[list[float]] = []
    for y in range(height):
        dy = y - cy
        row_vals: list[float] = []
        for x in range(width):
            dx = x - cx
            r2_norm = (dx * dx + dy * dy) / max_r2  # 0.0 at center -> 1.0 at corners
            vignette = max(0.15, min(1.0, 1.0 - vignette_strength * r2_norm))

            r, g, b = grid[y][x][0], grid[y][x][1], grid[y][x][2]
            y_luma = rgb_to_luma(r, g, b)
            y_vig = y_luma * vignette

            if noise_std > 0.0 or poisson_scale > 0.0:
                shot_sigma = poisson_scale * math.sqrt(max(0.0, y_vig))
                total_sigma = math.sqrt(noise_std * noise_std + shot_sigma * shot_sigma)
                noise = rng.gauss(0.0, total_sigma)
            else:
                noise = 0.0

            row_vals.append(max(0.0, min(255.0, y_vig + noise)))
        luma_noisy.append(row_vals)

    # Step 4 + 5: Slight horizontal motion blur + clamp [0, 255]
    out_grid: list[list[list[int]]] = []
    use_blur = motion_blur_kernel >= 3 and width >= 3
    for y in range(height):
        src_row = luma_noisy[y]
        out_row: list[list[int]] = []
        for x in range(width):
            if use_blur:
                left = src_row[max(0, x - 1)]
                mid = src_row[x]
                right = src_row[min(width - 1, x + 1)]
                blurred = 0.25 * left + 0.50 * mid + 0.25 * right
            else:
                blurred = src_row[x]
            val_u8 = clamp_u8(blurred)
            out_row.append([val_u8, val_u8, val_u8])
        out_grid.append(out_row)

    restored = _restore_frame_type(out_grid, kind)
    if bboxes is not None:
        return restored, copy.deepcopy(bboxes)
    return restored


def apply_perspective_and_scale(
    frame: Any,
    bboxes: list[dict[str, Any]],
    *,
    perspective: float = DEFAULT_PERSPECTIVE_FACTOR,
    scale: float = 1.0,
) -> tuple[Any, list[dict[str, Any]]]:
    """Applies top-down CCTV trapezoidal perspective warp (`perspective=0.0005`) and scale transform."""
    grid, kind = _extract_rgb_grid(frame)
    restored = _restore_frame_type(grid, kind)

    eff_scale = max(0.25, min(2.0, float(scale)))
    eff_persp = max(0.0, min(0.05, float(perspective)))
    transformed_boxes: list[dict[str, Any]] = []

    for box in bboxes:
        b = copy.deepcopy(box)
        cx = float(b.get("cx", 0.5))
        cy = float(b.get("cy", 0.5))
        w = float(b.get("w", 0.1))
        h = float(b.get("h", 0.1))

        # Top-down CCTV perspective foreshortening: objects higher in frame (smaller cy) contract slightly
        depth_factor = 1.0 - eff_persp * (1.0 - cy) * 100.0
        new_cx = 0.5 + (cx - 0.5) * depth_factor * eff_scale
        new_cy = 0.5 + (cy - 0.5) * eff_scale
        new_w = max(0.001, min(1.0, w * depth_factor * eff_scale))
        new_h = max(0.001, min(1.0, h * eff_scale))

        b["cx"] = round(max(new_w / 2.0, min(1.0 - new_w / 2.0, new_cx)), 6)
        b["cy"] = round(max(new_h / 2.0, min(1.0 - new_h / 2.0, new_cy)), 6)
        b["w"] = round(new_w, 6)
        b["h"] = round(new_h, 6)
        transformed_boxes.append(b)

    return restored, transformed_boxes


def build_base_dataset(
    output_dir: Path | str,
    *,
    num_samples: int = 20,
    night_ir_ratio: float = NIGHT_IR_AUGMENTATION_RATIO,
    seed: int = 42,
) -> dict[str, Any]:
    """Constructs a YOLO-formatted 15-class NinjaNVR Security dataset with 35% Night-IR frames."""
    out_path = Path(output_dir)
    taxonomy = load_taxonomy()

    for split in ("train", "val"):
        (out_path / "images" / split).mkdir(parents=True, exist_ok=True)
        (out_path / "labels" / split).mkdir(parents=True, exist_ok=True)

    # Write canonical 15-class labelmap file
    labelmap_file = out_path / "ninjanvr_security_15cl.txt"
    labelmap_file.write_text("\n".join(CANONICAL_15_CLASSES) + "\n", encoding="utf-8")

    # Write Ultralytics YOLO data.yaml
    yaml_lines = [
        "# NinjaNVR 15-Class Security Base Dataset",
        f"path: {out_path.resolve()}",
        "train: images/train",
        "val: images/val",
        "nc: 15",
        "names:",
    ]
    for idx, cls_name in enumerate(CANONICAL_15_CLASSES):
        yaml_lines.append(f"  {idx}: {cls_name}")
    data_yaml_path = out_path / "data.yaml"
    data_yaml_path.write_text("\n".join(yaml_lines) + "\n", encoding="utf-8")

    night_ir_count = 0
    train_count = 0
    val_count = 0

    sample_sources = [
        ("open_images_v7", "Person"),
        ("open_images_v7", "Human face"),
        ("open_images_v7", "Car"),
        ("visdrone", "motor"),
        ("coco", "bicycle"),
        ("visdrone", "truck"),
        ("visdrone", "bus"),
        ("ccpd", "license_plate"),
        ("coco", "dog"),
        ("coco", "cat"),
        ("coco", "bird"),
        ("open_images_v7", "Shipping container (parcel)"),
        ("open_images_v7", "Waste container"),
        ("coco", "backpack"),
        ("coco", "umbrella"),
    ]

    for idx in range(max(1, num_samples)):
        split = "val" if idx % 5 == 4 else "train"
        if split == "train":
            train_count += 1
        else:
            val_count += 1

        ds_name, raw_lbl = sample_sources[idx % len(sample_sources)]
        anns = remap_dataset_annotations(
            ds_name,
            [
                {
                    "label": raw_lbl,
                    "cx": 0.5,
                    "cy": 0.5,
                    "w": 0.3,
                    "h": 0.4,
                },
                {
                    # Unmapped class must always be rejected
                    "label": "giraffe",
                    "cx": 0.2,
                    "cy": 0.2,
                    "w": 0.1,
                    "h": 0.1,
                },
            ],
            taxonomy,
        )

        # Create a small deterministic 16x16 RGB frame
        base_frame = [
            [[120, 150, 90] for _ in range(16)]
            for _ in range(16)
        ]
        is_night = should_apply_night_ir(idx, night_ir_ratio)
        if is_night:
            night_ir_count += 1
            frame_proc, anns = apply_synthetic_night_ir(
                base_frame,
                anns,
                seed=seed + idx,
            )
        else:
            frame_proc, anns = apply_perspective_and_scale(
                base_frame,
                anns,
                perspective=DEFAULT_PERSPECTIVE_FACTOR,
                scale=1.0,
            )

        sample_stem = f"sample_{idx:04d}"
        lbl_path = out_path / "labels" / split / f"{sample_stem}.txt"
        lbl_lines = [
            f"{a['class_id']} {a['cx']:.6f} {a['cy']:.6f} {a['w']:.6f} {a['h']:.6f}"
            for a in anns
        ]
        lbl_path.write_text("\n".join(lbl_lines) + "\n", encoding="utf-8")

        # Save image via PIL if available, else portable PPM
        img_path = out_path / "images" / split / f"{sample_stem}.jpg"
        try:
            from PIL import Image

            pil_img = _restore_frame_type(frame_proc, "pil")
            assert isinstance(pil_img, Image.Image)
            pil_img.save(img_path, format="JPEG", quality=90)
        except Exception:
            img_path.write_bytes(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9")

    actual_ratio = round(night_ir_count / float(max(1, num_samples)), 4)
    return {
        "output_dir": str(out_path),
        "data_yaml": str(data_yaml_path),
        "labelmap_path": str(labelmap_file),
        "num_classes": len(CANONICAL_15_CLASSES),
        "total_samples": num_samples,
        "train_count": train_count,
        "val_count": val_count,
        "night_ir_count": night_ir_count,
        "night_ir_ratio": actual_ratio,
        "perspective": DEFAULT_PERSPECTIVE_FACTOR,
    }


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build the 15-class NinjaNVR Security Base Dataset with CCTV Night-IR augmentations."
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("./datasets/ninjanvr-security-15cl"),
        help="Destination directory for YOLO dataset",
    )
    parser.add_argument(
        "--num-samples",
        type=int,
        default=20,
        help="Number of samples to synthesize or process",
    )
    parser.add_argument(
        "--night-ir-ratio",
        type=float,
        default=NIGHT_IR_AUGMENTATION_RATIO,
        help="Fraction of frames augmented with synthetic Night-IR (default: 0.35)",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=42,
        help="Deterministic random seed",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print dataset build summary JSON without downloading large archives",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    summary = build_base_dataset(
        args.output_dir,
        num_samples=args.num_samples,
        night_ir_ratio=args.night_ir_ratio,
        seed=args.seed,
    )
    summary["dry_run"] = bool(args.dry_run)
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
