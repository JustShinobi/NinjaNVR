#!/usr/bin/env python3
"""Unit test suite for NinjaNVR [AI-17] 15-Class Security Model Factory & Fine-Tuning Pipeline."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from build_base_dataset import (  # noqa: E402
    CANONICAL_15_CLASSES,
    apply_perspective_and_scale,
    apply_synthetic_night_ir,
    build_base_dataset,
    load_taxonomy,
    remap_class_label,
    remap_dataset_annotations,
    rgb_frame_to_luma_grid,
    rgb_to_luma,
    should_apply_night_ir,
)
from train_ninjanvr_model import (  # noqa: E402
    auto_select_batch_size,
    build_nvr_upload_payload,
    build_training_plan,
    extract_and_validate_dataset_zip,
    write_exported_model_artifacts,
)


class TestAiModelFactoryPipeline(unittest.TestCase):
    def test_canonical_15_class_labelmap_and_taxonomy_remapping(self) -> None:
        labelmap_path = REPO_ROOT / "models" / "ninjanvr_security_15cl.txt"
        self.assertTrue(labelmap_path.exists(), "models/ninjanvr_security_15cl.txt must exist")
        lines = [ln.strip() for ln in labelmap_path.read_text(encoding="utf-8").splitlines() if ln.strip()]
        self.assertEqual(lines, CANONICAL_15_CLASSES)
        self.assertEqual(len(lines), 15)

        tax = load_taxonomy()
        self.assertEqual(tax["num_classes"], 15)
        self.assertEqual(tax["classes"], CANONICAL_15_CLASSES)
        for idx, name in enumerate(CANONICAL_15_CLASSES):
            self.assertEqual(tax["id_to_name"][str(idx)], name)
            self.assertEqual(tax["name_to_id"][name], idx)

        # Open Images V7 remapping (0..14)
        self.assertEqual(remap_class_label("open_images_v7", "Person", tax), 0)
        self.assertEqual(remap_class_label("open_images_v7", "Human face", tax), 1)
        self.assertEqual(remap_class_label("open_images_v7", "Car", tax), 2)
        self.assertEqual(remap_class_label("open_images_v7", "Motorcycle", tax), 3)
        self.assertEqual(remap_class_label("open_images_v7", "Bicycle", tax), 4)
        self.assertEqual(remap_class_label("open_images_v7", "Truck", tax), 5)
        self.assertEqual(remap_class_label("open_images_v7", "Bus", tax), 6)
        self.assertEqual(remap_class_label("open_images_v7", "Vehicle registration plate", tax), 7)
        self.assertEqual(remap_class_label("open_images_v7", "Dog", tax), 8)
        self.assertEqual(remap_class_label("open_images_v7", "Cat", tax), 9)
        self.assertEqual(remap_class_label("open_images_v7", "Bird", tax), 10)
        self.assertEqual(remap_class_label("open_images_v7", "Box", tax), 11)
        self.assertEqual(remap_class_label("open_images_v7", "Shipping container (parcel)", tax), 11)
        self.assertEqual(remap_class_label("open_images_v7", "Waste container", tax), 12)
        self.assertEqual(remap_class_label("open_images_v7", "Backpack", tax), 13)
        self.assertEqual(remap_class_label("open_images_v7", "Umbrella", tax), 14)

        # VisDrone remapping
        self.assertEqual(remap_class_label("visdrone", "pedestrian", tax), 0)
        self.assertEqual(remap_class_label("visdrone", "people", tax), 0)
        self.assertEqual(remap_class_label("visdrone", "car", tax), 2)
        self.assertEqual(remap_class_label("visdrone", "van", tax), 2)
        self.assertEqual(remap_class_label("visdrone", "motor", tax), 3)
        self.assertEqual(remap_class_label("visdrone", "bicycle", tax), 4)
        self.assertEqual(remap_class_label("visdrone", "truck", tax), 5)
        self.assertEqual(remap_class_label("visdrone", "bus", tax), 6)

        # COCO, WiderFace, and CCPD remapping
        self.assertEqual(remap_class_label("coco", "person", tax), 0)
        self.assertEqual(remap_class_label("coco", "dog", tax), 8)
        self.assertEqual(remap_class_label("coco", "umbrella", tax), 14)
        self.assertEqual(remap_class_label("widerface", "face", tax), 1)
        self.assertEqual(remap_class_label("ccpd", "license_plate", tax), 7)

        # Unmapped classes must be rejected (return None)
        for unmapped in ("giraffe", "toothbrush", "zebra", "toilet", "fork"):
            self.assertIsNone(remap_class_label("coco", unmapped, tax))
            self.assertIsNone(remap_class_label("open_images_v7", unmapped, tax))
            self.assertIsNone(remap_class_label("visdrone", unmapped, tax))

        remapped_boxes = remap_dataset_annotations(
            "open_images_v7",
            [
                {"label": "Person", "cx": 0.5, "cy": 0.5, "w": 0.2, "h": 0.6},
                {"label": "giraffe", "cx": 0.3, "cy": 0.3, "w": 0.4, "h": 0.4},
                {"label": "toothbrush", "cx": 0.1, "cy": 0.1, "w": 0.05, "h": 0.05},
                {"label": "Vehicle registration plate", "cx": 0.6, "cy": 0.8, "w": 0.12, "h": 0.06},
            ],
            tax,
        )
        self.assertEqual(len(remapped_boxes), 2)
        self.assertEqual(remapped_boxes[0]["class_id"], 0)
        self.assertEqual(remapped_boxes[0]["label"], "person")
        self.assertEqual(remapped_boxes[1]["class_id"], 7)
        self.assertEqual(remapped_boxes[1]["label"], "license_plate")

    def test_synthetic_night_ir_transform_luma_vignette_bounds_and_bbox_preservation(self) -> None:
        # 1. Exact luma formula: Y = 0.299R + 0.587G + 0.114B
        r, g, b = 200, 100, 50
        expected_y = 0.299 * 200 + 0.587 * 100 + 0.114 * 50  # 59.8 + 58.7 + 5.7 = 124.2
        self.assertAlmostEqual(rgb_to_luma(r, g, b), expected_y, places=5)

        uniform_rgb = [[[r, g, b] for _ in range(9)] for _ in range(9)]
        luma_grid = rgb_frame_to_luma_grid(uniform_rgb)
        self.assertEqual(luma_grid[4][4], 124)

        # 2. Radial vignette falloff (center pixel brighter than corner pixel when noise=0)
        bboxes = [
            {"class_id": 0, "label": "person", "cx": 0.45, "cy": 0.52, "w": 0.22, "h": 0.64},
            {"class_id": 7, "label": "license_plate", "cx": 0.72, "cy": 0.81, "w": 0.11, "h": 0.05},
        ]
        ir_clean, preserved_boxes = apply_synthetic_night_ir(
            uniform_rgb,
            bboxes,
            vignette_strength=0.40,
            noise_std=0.0,
            poisson_scale=0.0,
            motion_blur_kernel=1,
            seed=42,
        )
        self.assertEqual(preserved_boxes, bboxes, "bounding boxes must be preserved identically")
        center_px = ir_clean[4][4][0]
        corner_px = ir_clean[0][0][0]
        self.assertEqual(center_px, 124)
        self.assertLess(corner_px, center_px, "radial IR vignette must attenuate corner intensity")

        # 3. Full noisy + motion-blurred Night-IR stays strictly within [0, 255] and monochrome R==G==B
        ir_noisy, preserved_noisy = apply_synthetic_night_ir(
            uniform_rgb,
            bboxes,
            vignette_strength=0.35,
            noise_std=6.0,
            poisson_scale=0.2,
            motion_blur_kernel=3,
            seed=99,
        )
        self.assertEqual(preserved_noisy, bboxes)
        for row in ir_noisy:
            for px in row:
                self.assertEqual(len(px), 3)
                self.assertEqual(px[0], px[1])
                self.assertEqual(px[1], px[2])
                self.assertGreaterEqual(px[0], 0)
                self.assertLessEqual(px[0], 255)

        # 4. 35% of frames selected deterministically across 100 frames
        selected = sum(1 for i in range(100) if should_apply_night_ir(i, 0.35))
        self.assertEqual(selected, 35)

        # 5. Perspective & scale transform preserves valid normalized [0, 1] bounding boxes
        _, warped_boxes = apply_perspective_and_scale(
            uniform_rgb,
            bboxes,
            perspective=0.0005,
            scale=0.95,
        )
        self.assertEqual(len(warped_boxes), 2)
        for wb in warped_boxes:
            self.assertTrue(0.0 <= wb["cx"] <= 1.0)
            self.assertTrue(0.0 <= wb["cy"] <= 1.0)
            self.assertTrue(0.0 < wb["w"] <= 1.0)
            self.assertTrue(0.0 < wb["h"] <= 1.0)

    def test_train_ninjanvr_model_batch_scaling_freeze_10_and_zip_validation(self) -> None:
        # 1. VRAM batch-size auto-selection
        self.assertEqual(auto_select_batch_size(96.0), 128)
        self.assertEqual(auto_select_batch_size(40.0), 64)
        self.assertEqual(auto_select_batch_size(16.0), 32)
        self.assertEqual(auto_select_batch_size(15.0), 16)
        self.assertEqual(auto_select_batch_size(0.0, device="cpu"), 8)

        # 2. Finetune plan defaults to freeze=10, opset=17, and [1, 19, N] output tensor
        plan_ft_640 = build_training_plan(
            mode="finetune",
            dataset_path="ninjanvr-dataset.zip",
            imgsz=640,
            vram_gb=16.0,
            nvr_url="http://127.0.0.1:5000",
            nvr_token="test-token",
        )
        self.assertEqual(plan_ft_640["mode"], "finetune")
        self.assertEqual(plan_ft_640["freeze"], 10)
        self.assertEqual(plan_ft_640["batch"], 32)
        self.assertEqual(plan_ft_640["export"]["opset"], 17)
        self.assertEqual(plan_ft_640["export"]["input_shape"], [1, 3, 640, 640])
        self.assertEqual(plan_ft_640["export"]["output_shape"], [1, 19, 8400])
        self.assertTrue(plan_ft_640["upload"]["enabled"])

        plan_base_320 = build_training_plan(
            mode="base",
            imgsz=320,
            vram_gb=96.0,
        )
        self.assertEqual(plan_base_320["mode"], "base")
        self.assertEqual(plan_base_320["freeze"], 0)
        self.assertEqual(plan_base_320["batch"], 128)
        self.assertEqual(plan_base_320["export"]["input_shape"], [1, 3, 320, 320])
        self.assertEqual(plan_base_320["export"]["output_shape"], [1, 19, 2100])

        # 3. Extract & validate ninjanvr-dataset.zip + run CLI --dry-run
        with tempfile.TemporaryDirectory(prefix="ninjanvr_test_ai_") as tmp:
            tmp_path = Path(tmp)
            ds_build_dir = tmp_path / "built_base"
            build_summary = build_base_dataset(ds_build_dir, num_samples=10, night_ir_ratio=0.35)
            self.assertEqual(build_summary["num_classes"], 15)
            self.assertEqual(build_summary["total_samples"], 10)

            zip_path = tmp_path / "ninjanvr-dataset.zip"
            with zipfile.ZipFile(zip_path, "w") as zf:
                for file_path in ds_build_dir.rglob("*"):
                    if file_path.is_file():
                        zf.write(file_path, file_path.relative_to(ds_build_dir).as_posix())
                # Also include an empty hard-negative label file
                zf.writestr("labels/train/hard_neg_01.txt", "")

            extracted_info = extract_and_validate_dataset_zip(zip_path, tmp_path / "extracted")
            self.assertEqual(extracted_info["num_classes"], 15)
            self.assertGreaterEqual(extracted_info["train_images"], 1)
            self.assertEqual(extracted_info["hard_negatives"], 1)
            extracted_yaml_text = Path(extracted_info["data_yaml"]).read_text(encoding="utf-8")
            self.assertIn(
                f"path: {(tmp_path / 'extracted').resolve()}",
                extracted_yaml_text,
            )

            out_runs = tmp_path / "runs"
            cmd = [
                sys.executable,
                str(SCRIPT_DIR / "train_ninjanvr_model.py"),
                "--mode",
                "finetune",
                "--dataset",
                str(zip_path),
                "--output-dir",
                str(out_runs),
                "--imgsz",
                "320",
                "--vram-gb",
                "96",
                "--dry-run",
            ]
            proc = subprocess.run(cmd, capture_output=True, text=True, check=True)
            cli_out = json.loads(proc.stdout)
            self.assertTrue(cli_out["dry_run"])
            self.assertEqual(cli_out["batch"], 128)
            self.assertEqual(cli_out["freeze"], 10)
            self.assertEqual(cli_out["export"]["output_shape"], [1, 19, 2100])
            self.assertTrue(Path(cli_out["artifacts"]["onnx_path"]).exists())
            self.assertTrue(Path(cli_out["artifacts"]["openvino_xml_path"]).exists())
            self.assertTrue(Path(cli_out["artifacts"]["openvino_bin_path"]).exists())
            self.assertTrue(Path(cli_out["artifacts"]["labelmap_path"]).exists())

            upload_payload = build_nvr_upload_payload(
                cli_out["artifacts"]["onnx_path"],
                cli_out["artifacts"]["labelmap_path"],
                imgsz=320,
            )
            self.assertEqual(upload_payload["filename"], "ninjanvr_security_15cl.onnx")
            self.assertEqual(upload_payload["model_type"], "yolo-generic")
            self.assertEqual(upload_payload["width"], 320)
            self.assertEqual(upload_payload["height"], 320)

    def test_colab_notebook_nbformat_v4_schema_and_4_cells(self) -> None:
        nb_path = REPO_ROOT / "notebooks" / "NinjaNVR_Model_Factory.ipynb"
        self.assertTrue(nb_path.exists(), "notebooks/NinjaNVR_Model_Factory.ipynb must exist")
        nb = json.loads(nb_path.read_text(encoding="utf-8"))

        self.assertEqual(nb.get("nbformat"), 4)
        self.assertIsInstance(nb.get("nbformat_minor"), int)
        cells = nb.get("cells", [])
        self.assertEqual(len(cells), 4, "Notebook must contain 4 structured cells")

        combined_source = ""
        for cell in cells:
            self.assertEqual(cell.get("cell_type"), "code")
            self.assertIn("source", cell)
            self.assertIn("metadata", cell)
            src_text = "".join(cell["source"]) if isinstance(cell["source"], list) else str(cell["source"])
            combined_source += src_text + "\n"

        # Verify required capabilities across the 4 cells
        self.assertIn("G4", combined_source)
        self.assertIn("A100", combined_source)
        self.assertIn("L4", combined_source)
        self.assertIn("T4", combined_source)
        self.assertIn("128", combined_source)
        self.assertIn("0.299", combined_source)
        self.assertIn("freeze", combined_source)
        self.assertIn(".onnx", combined_source)
        self.assertIn(".xml", combined_source)


if __name__ == "__main__":
    unittest.main()
