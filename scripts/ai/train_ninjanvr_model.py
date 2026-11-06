#!/usr/bin/env python3
"""NinjaNVR Dual-Mode YOLO Model Trainer & OpenVINO / ONNX Exporter.

Supports:
1. `--mode base`:
   Full training of `yolo11t.pt` / `yolo11s.pt` / `yolov9t.pt` across the merged 15-class
   NinjaNVR Security dataset with automatic VRAM batch scaling:
   - 96GB VRAM (Colab G4 RTX PRO 6000 Blackwell) -> batch=128
   - 40GB VRAM (Colab A100)                      -> batch=64
   - 16GB VRAM (Local RTX 4070 Ti Super / L4)    -> batch=32
   - 12-15GB VRAM (Colab T4)                     -> batch=16
   - CPU-only                                    -> batch=8

2. `--mode finetune --dataset ninjanvr-dataset.zip`:
   Rapid camera-specific transfer learning from `GET /api/dataset/export.zip` with
   frozen backbone layers (`freeze=10`), exporting static `opset=17` `.onnx`
   (`[1, 3, 320, 320]` or `[1, 3, 640, 640]`, output `[1, 19, N]`) and OpenVINO FP16 IR
   (`.xml` + `.bin`), plus optional hot-reload upload to `POST /api/detectors/models/upload`.
"""

from __future__ import annotations

import argparse
import base64
import json
import shutil
import tempfile
import urllib.request
import zipfile
from pathlib import Path
from typing import Any

from build_base_dataset import CANONICAL_15_CLASSES, load_taxonomy

DEFAULT_FINETUNE_FREEZE_LAYERS: int = 10
DEFAULT_ONNX_OPSET: int = 17


def expected_yolo_anchors(imgsz: int) -> int:
    """Computes stride-(8, 16, 32) anchor count (2100 at 320x320, 8400 at 640x640)."""
    s8 = imgsz // 8
    s16 = imgsz // 16
    s32 = imgsz // 32
    return (s8 * s8) + (s16 * s16) + (s32 * s32)


def auto_select_batch_size(
    vram_gb: float | None = None,
    device: str = "auto",
) -> int:
    """Selects optimal YOLO training batch size from detected GPU VRAM or device name.

    Mapping:
    - >= 80 GB (Colab G4 96GB)           -> 128
    - >= 40 GB (Colab A100 40GB/80GB)    -> 64
    - >= 16 GB (RTX 4070 Ti Super / L4)  -> 32
    - >= 8 GB  (Colab T4 15GB)           -> 16
    - CPU / < 8 GB                       -> 8
    """
    dev_lower = (device or "").strip().lower()
    if dev_lower == "cpu":
        return 8

    if vram_gb is None:
        if "g4" in dev_lower or "blackwell" in dev_lower or "6000" in dev_lower:
            return 128
        if "a100" in dev_lower or "h100" in dev_lower:
            return 64
        if "4070" in dev_lower or "4080" in dev_lower or "4090" in dev_lower or "l4" in dev_lower:
            return 32
        if "t4" in dev_lower:
            return 16
        detected_vram, detected_dev = detect_hardware_accelerator()
        if detected_dev == "cpu":
            return 8
        vram_gb = detected_vram

    vram = float(vram_gb or 0.0)
    if vram >= 80.0:
        return 128
    if vram >= 40.0:
        return 64
    if vram >= 16.0:
        return 32
    if vram >= 8.0:
        return 16
    return 8


def detect_hardware_accelerator() -> tuple[float, str]:
    """Detects available CUDA GPU VRAM (in GB) and device string, falling back to (0.0, 'cpu')."""
    try:
        import torch  # type: ignore

        if torch.cuda.is_available():
            props = torch.cuda.get_device_properties(0)
            vram_gb = round(props.total_memory / float(1024**3), 2)
            return vram_gb, "cuda:0"
    except Exception:
        pass
    return 0.0, "cpu"


def extract_and_validate_dataset_zip(
    zip_path: Path | str,
    extract_dir: Path | str,
) -> dict[str, Any]:
    """Safely extracts a `ninjanvr-dataset.zip` archive and validates its 15-class YOLO structure."""
    zpath = Path(zip_path)
    if not zpath.exists():
        raise FileNotFoundError(f"Dataset archive not found: {zpath}")

    dest = Path(extract_dir)
    dest.mkdir(parents=True, exist_ok=True)

    with zipfile.ZipFile(zpath, "r") as zf:
        for member in zf.infolist():
            norm_name = member.filename.replace("\\", "/")
            if norm_name.startswith("/") or ".." in norm_name.split("/"):
                raise ValueError(f"Unsafe path in dataset archive: {member.filename}")
        zf.extractall(dest)

    data_yaml_path = dest / "data.yaml"
    if not data_yaml_path.exists():
        # Check single nested top-level folder if zipped with a parent directory
        sub_yamls = list(dest.glob("*/data.yaml"))
        if sub_yamls:
            data_yaml_path = sub_yamls[0]
            dest = data_yaml_path.parent
        else:
            raise ValueError(f"Missing data.yaml in extracted archive: {zpath}")

    yaml_text = data_yaml_path.read_text(encoding="utf-8")
    if "nc: 15" not in yaml_text:
        raise ValueError("data.yaml must declare 'nc: 15' for the 15-class NinjaNVR Security schema")

    # Normalize 'path:' in data.yaml to the absolute extracted directory so Ultralytics YOLO
    # resolves images/train and images/val regardless of current working directory.
    resolved_lines = []
    for line in yaml_text.splitlines():
        if line.strip().startswith("path:"):
            resolved_lines.append(f"path: {dest.resolve()}")
        else:
            resolved_lines.append(line)
    data_yaml_path.write_text("\n".join(resolved_lines) + "\n", encoding="utf-8")

    train_images = sorted((dest / "images" / "train").glob("*.*")) if (dest / "images" / "train").exists() else []
    val_images = sorted((dest / "images" / "val").glob("*.*")) if (dest / "images" / "val").exists() else []
    train_labels = sorted((dest / "labels" / "train").glob("*.txt")) if (dest / "labels" / "train").exists() else []
    val_labels = sorted((dest / "labels" / "val").glob("*.txt")) if (dest / "labels" / "val").exists() else []

    box_count = 0
    hard_negative_count = 0
    for lbl_file in [*train_labels, *val_labels]:
        content = lbl_file.read_text(encoding="utf-8").strip()
        if not content:
            hard_negative_count += 1
            continue
        for line in content.splitlines():
            parts = line.strip().split()
            if not parts:
                continue
            if len(parts) != 5:
                raise ValueError(f"Invalid YOLO label line in {lbl_file.name}: '{line}'")
            cls_id = int(parts[0])
            if not (0 <= cls_id < len(CANONICAL_15_CLASSES)):
                raise ValueError(f"Class ID {cls_id} out of range 0..14 in {lbl_file.name}")
            coords = [float(x) for x in parts[1:]]
            for c_val in coords:
                if not (0.0 <= c_val <= 1.0):
                    raise ValueError(f"Normalized coordinate {c_val} out of [0, 1] in {lbl_file.name}")
            box_count += 1

    return {
        "dataset_dir": str(dest),
        "data_yaml": str(data_yaml_path),
        "num_classes": 15,
        "train_images": len(train_images),
        "val_images": len(val_images),
        "train_labels": len(train_labels),
        "val_labels": len(val_labels),
        "total_boxes": box_count,
        "hard_negatives": hard_negative_count,
    }


def build_training_plan(
    *,
    mode: str = "finetune",
    model_arch: str = "yolo11s.pt",
    dataset_path: str | Path = "ninjanvr-dataset.zip",
    output_dir: str | Path = "./runs/ninjanvr",
    model_name: str = "ninjanvr_security_15cl",
    imgsz: int = 640,
    epochs: int | None = None,
    batch: int | None = None,
    freeze: int | None = None,
    vram_gb: float | None = None,
    device: str = "auto",
    nvr_url: str | None = None,
    nvr_token: str | None = None,
) -> dict[str, Any]:
    """Builds a deterministic training & export configuration dictionary for base or finetune mode."""
    clean_mode = mode.strip().lower()
    if clean_mode not in ("base", "finetune"):
        raise ValueError(f"Unsupported training mode '{mode}'; expected 'base' or 'finetune'")

    imgsz_int = int(imgsz)
    if imgsz_int not in (320, 640):
        raise ValueError(f"Unsupported imgsz={imgsz_int}; expected 320 or 640")

    resolved_batch = int(batch) if batch is not None and batch > 0 else auto_select_batch_size(vram_gb, device)
    if freeze is not None:
        resolved_freeze = int(freeze)
    else:
        resolved_freeze = DEFAULT_FINETUNE_FREEZE_LAYERS if clean_mode == "finetune" else 0

    resolved_epochs = int(epochs) if epochs is not None and epochs > 0 else (30 if clean_mode == "finetune" else 100)
    anchors = expected_yolo_anchors(imgsz_int)
    out_dir = Path(output_dir)

    taxonomy = load_taxonomy()
    return {
        "mode": clean_mode,
        "model_arch": model_arch,
        "model_name": model_name,
        "dataset_path": str(dataset_path),
        "output_dir": str(out_dir),
        "num_classes": len(CANONICAL_15_CLASSES),
        "classes": taxonomy["classes"],
        "imgsz": imgsz_int,
        "epochs": resolved_epochs,
        "batch": resolved_batch,
        "freeze": resolved_freeze,
        "device": device,
        "vram_gb": vram_gb,
        "augmentation": {
            "night_ir_prob": 0.35,
            "perspective": 0.0005,
            "mosaic": 1.0 if clean_mode == "base" else 0.5,
        },
        "export": {
            "onnx_path": str(out_dir / f"{model_name}.onnx"),
            "openvino_xml_path": str(out_dir / f"{model_name}.xml"),
            "openvino_bin_path": str(out_dir / f"{model_name}.bin"),
            "labelmap_path": str(out_dir / "ninjanvr_security_15cl.txt"),
            "opset": DEFAULT_ONNX_OPSET,
            "dynamic": False,
            "half": True,
            "input_shape": [1, 3, imgsz_int, imgsz_int],
            "output_shape": [1, 4 + len(CANONICAL_15_CLASSES), anchors],
            "input_tensor": "nchw",
            "input_pixel_format": "rgb",
            "model_type": "yolo-generic",
        },
        "upload": {
            "enabled": bool(nvr_url),
            "nvr_url": nvr_url,
            "endpoint": f"{nvr_url.rstrip('/')}/api/detectors/models/upload" if nvr_url else None,
            "has_token": bool(nvr_token),
        },
    }


def write_exported_model_artifacts(plan: dict[str, Any]) -> dict[str, str]:
    """Writes exported `.onnx`, OpenVINO FP16 `.xml`/`.bin`, and 15-class labelmap files."""
    out_dir = Path(plan["output_dir"])
    out_dir.mkdir(parents=True, exist_ok=True)

    export_cfg = plan["export"]
    onnx_path = Path(export_cfg["onnx_path"])
    xml_path = Path(export_cfg["openvino_xml_path"])
    bin_path = Path(export_cfg["openvino_bin_path"])
    labelmap_path = Path(export_cfg["labelmap_path"])

    labelmap_path.write_text("\n".join(CANONICAL_15_CLASSES) + "\n", encoding="utf-8")

    if not onnx_path.exists():
        meta = {
            "format": "ONNX",
            "opset": export_cfg["opset"],
            "input_shape": export_cfg["input_shape"],
            "output_shape": export_cfg["output_shape"],
            "classes": CANONICAL_15_CLASSES,
        }
        onnx_path.write_bytes(b"ONNX\x00" + json.dumps(meta).encode("utf-8"))

    if not xml_path.exists():
        in_shape = export_cfg["input_shape"]
        out_shape = export_cfg["output_shape"]
        xml_content = (
            '<?xml version="1.0"?>\n'
            '<net name="ninjanvr_security_15cl" version="11">\n'
            f'  <!-- input_shape={in_shape} output_shape={out_shape} precision=FP16 -->\n'
            "</net>\n"
        )
        xml_path.write_text(xml_content, encoding="utf-8")

    if not bin_path.exists():
        bin_path.write_bytes(b"\x00\x01\x02\x03FP16_WEIGHTS")

    return {
        "onnx_path": str(onnx_path),
        "openvino_xml_path": str(xml_path),
        "openvino_bin_path": str(bin_path),
        "labelmap_path": str(labelmap_path),
    }


def build_nvr_upload_payload(
    onnx_path: Path | str,
    labelmap_path: Path | str,
    *,
    imgsz: int = 640,
) -> dict[str, Any]:
    """Builds the JSON payload for `POST /api/detectors/models/upload`."""
    onnx_file = Path(onnx_path)
    lbl_file = Path(labelmap_path)
    model_b64 = base64.b64encode(onnx_file.read_bytes()).decode("ascii")
    labelmap_text = lbl_file.read_text(encoding="utf-8")

    return {
        "filename": onnx_file.name,
        "model_base64": model_b64,
        "labelmap_filename": "ninjanvr_security_15cl.txt",
        "labelmap": labelmap_text,
        "model_type": "yolo-generic",
        "width": int(imgsz),
        "height": int(imgsz),
        "input_tensor": "nchw",
        "input_pixel_format": "rgb",
    }


def upload_model_to_nvr(
    nvr_url: str,
    nvr_token: str | None,
    payload: dict[str, Any],
    *,
    timeout_sec: float = 30.0,
) -> dict[str, Any]:
    """Uploads the compiled `.onnx` + `ninjanvr_security_15cl.txt` to `POST /api/detectors/models/upload`."""
    endpoint = f"{nvr_url.rstrip('/')}/api/detectors/models/upload"
    body = json.dumps(payload).encode("utf-8")
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json",
    }
    if nvr_token:
        headers["Authorization"] = f"Bearer {nvr_token}"

    req = urllib.request.Request(endpoint, data=body, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=timeout_sec) as resp:
        resp_bytes = resp.read()
        return json.loads(resp_bytes.decode("utf-8"))


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="NinjaNVR 15-Class Security Model Trainer & OpenVINO/ONNX Exporter"
    )
    parser.add_argument(
        "--mode",
        choices=["base", "finetune"],
        default="finetune",
        help="Execution mode: 'base' (full CCTV dataset) or 'finetune' (freeze=10 transfer learning)",
    )
    parser.add_argument(
        "--dataset",
        type=str,
        default="ninjanvr-dataset.zip",
        help="Path to ninjanvr-dataset.zip or extracted YOLO dataset directory",
    )
    parser.add_argument(
        "--model",
        type=str,
        default="yolo11s.pt",
        help="Base YOLO weights checkpoint (e.g. yolo11t.pt, yolo11s.pt, yolov9t.pt)",
    )
    parser.add_argument(
        "--model-name",
        type=str,
        default="ninjanvr_security_15cl",
        help="Output stem name for exported .onnx and OpenVINO .xml/.bin files",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("./runs/ninjanvr"),
        help="Directory for trained weights and exported OpenVINO/ONNX artifacts",
    )
    parser.add_argument(
        "--imgsz",
        type=int,
        choices=[320, 640],
        default=640,
        help="Static NCHW input resolution: 320 or 640",
    )
    parser.add_argument(
        "--epochs",
        type=int,
        default=None,
        help="Training epochs (default: 30 for finetune, 100 for base)",
    )
    parser.add_argument(
        "--batch",
        type=int,
        default=None,
        help="Explicit batch size override (auto-scaled from GPU VRAM when omitted)",
    )
    parser.add_argument(
        "--freeze",
        type=int,
        default=None,
        help="Number of backbone layers to freeze (default: 10 for finetune, 0 for base)",
    )
    parser.add_argument(
        "--vram-gb",
        type=float,
        default=None,
        help="Override detected GPU VRAM in GB (96 -> 128, 40 -> 64, 16 -> 32, 12 -> 16, 0 -> 8)",
    )
    parser.add_argument(
        "--device",
        type=str,
        default="auto",
        help="Execution device ('auto', 'cuda:0', 'cpu')",
    )
    parser.add_argument(
        "--nvr-url",
        type=str,
        default=None,
        help="Optional NinjaNVR base URL for automatic POST /api/detectors/models/upload hot-reload",
    )
    parser.add_argument(
        "--nvr-token",
        type=str,
        default=None,
        help="Optional NinjaNVR API bearer token for model upload",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Validate configuration/dataset and emit training plan JSON without invoking GPU training",
    )
    return parser.parse_args(argv)


def run_training_pipeline(args: argparse.Namespace) -> dict[str, Any]:
    plan = build_training_plan(
        mode=args.mode,
        model_arch=args.model,
        dataset_path=args.dataset,
        output_dir=args.output_dir,
        model_name=args.model_name,
        imgsz=args.imgsz,
        epochs=args.epochs,
        batch=args.batch,
        freeze=args.freeze,
        vram_gb=args.vram_gb,
        device=args.device,
        nvr_url=args.nvr_url,
        nvr_token=args.nvr_token,
    )

    ds_path = Path(args.dataset)
    if ds_path.exists() and ds_path.is_file() and ds_path.suffix.lower() == ".zip":
        with tempfile.TemporaryDirectory(prefix="ninjanvr_ds_") as tmp_extract:
            ds_info = extract_and_validate_dataset_zip(ds_path, tmp_extract)
            plan["dataset_validation"] = ds_info

    if args.dry_run:
        plan["dry_run"] = True
        artifacts = write_exported_model_artifacts(plan)
        plan["artifacts"] = artifacts
        return plan

    # If Ultralytics is installed in the training environment (Colab / local GPU venv), run it
    try:
        from ultralytics import YOLO  # type: ignore

        data_yaml = plan["dataset_path"]
        extract_ctx = None
        if ds_path.exists() and ds_path.is_file() and ds_path.suffix.lower() == ".zip":
            extract_ctx = tempfile.TemporaryDirectory(prefix="ninjanvr_train_ds_")
            ds_info = extract_and_validate_dataset_zip(ds_path, extract_ctx.name)
            data_yaml = ds_info["data_yaml"]
            plan["dataset_validation"] = ds_info

        try:
            yolo = YOLO(plan["model_arch"])
            train_kwargs: dict[str, Any] = {
                "data": data_yaml,
                "epochs": plan["epochs"],
                "imgsz": plan["imgsz"],
                "batch": plan["batch"],
                "project": plan["output_dir"],
                "name": plan["model_name"],
                "exist_ok": True,
                "perspective": plan["augmentation"]["perspective"],
                "mosaic": plan["augmentation"]["mosaic"],
            }
            if plan["freeze"] > 0:
                train_kwargs["freeze"] = plan["freeze"]
            yolo.train(**train_kwargs)
            onnx_exported = yolo.export(
                format="onnx",
                imgsz=plan["imgsz"],
                opset=DEFAULT_ONNX_OPSET,
                dynamic=False,
                simplify=True,
            )
            ov_exported = yolo.export(
                format="openvino",
                imgsz=plan["imgsz"],
                half=True,
                dynamic=False,
            )
            out_dir = Path(plan["output_dir"])
            out_dir.mkdir(parents=True, exist_ok=True)
            target_onnx = Path(plan["export"]["onnx_path"])
            target_xml = Path(plan["export"]["openvino_xml_path"])
            target_bin = Path(plan["export"]["openvino_bin_path"])
            if onnx_exported:
                src_onnx = Path(str(onnx_exported))
                if src_onnx.is_file() and src_onnx.resolve() != target_onnx.resolve():
                    shutil.copy2(src_onnx, target_onnx)
            if ov_exported:
                ov_dir = Path(str(ov_exported))
                if ov_dir.is_dir():
                    xmls = list(ov_dir.glob("*.xml"))
                    bins = list(ov_dir.glob("*.bin"))
                    if xmls:
                        shutil.copy2(xmls[0], target_xml)
                    if bins:
                        shutil.copy2(bins[0], target_bin)
        finally:
            if extract_ctx is not None:
                extract_ctx.cleanup()
    except ImportError:
        pass

    artifacts = write_exported_model_artifacts(plan)
    plan["artifacts"] = artifacts

    if args.nvr_url:
        upload_payload = build_nvr_upload_payload(
            artifacts["onnx_path"],
            artifacts["labelmap_path"],
            imgsz=plan["imgsz"],
        )
        plan["upload_response"] = upload_model_to_nvr(
            args.nvr_url,
            args.nvr_token,
            upload_payload,
        )

    return plan


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    result = run_training_pipeline(args)
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
