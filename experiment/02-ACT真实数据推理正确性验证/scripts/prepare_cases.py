#!/usr/bin/env python3
"""从固定Hugging Face dataset commit生成五组ACT正确性测试向量。

脚本只在宿主机运行：Parquet提供state和精确索引，FFmpeg从完整MP4提取RGB。
生成后的HWC RGB字节、f32状态与manifest会同时交给PyTorch、Rust、Exokernel
和seL4，四条路径不再各自解码视频。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
import subprocess
from pathlib import Path

import pyarrow.parquet as pq


MODEL_REPO = "Vel044/so101_act_bottle"
MODEL_COMMIT = "1c9b9309c387041f051e4ded0199e79f538f4984"
DATASET_REPO = "Vel044/so101_bottle"
DATASET_COMMIT = "4b5f3bc6638db278caaf6b3b1696bdb49493ad3a"
EPISODE = 0
# 五个位置覆盖episode 0的开始、前段、中段、后段和结束附近。
FRACTIONS = (0.10, 0.30, 0.50, 0.70, 0.90)
WIDTH = 640
HEIGHT = 360
IMAGE_BYTES = WIDTH * HEIGHT * 3


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def extract_rgb(ffmpeg: str, video: Path, timestamp: float, output: Path) -> None:
    subprocess.run(
        [
            ffmpeg,
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            f"{timestamp:.9f}",
            "-i",
            str(video),
            "-frames:v",
            "1",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "-y",
            str(output),
        ],
        check=True,
    )
    if output.stat().st_size != IMAGE_BYTES:
        raise RuntimeError(f"unexpected RGB size: {output}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset-dir", type=Path, required=True)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--ffmpeg", default="ffmpeg")
    args = parser.parse_args()

    data_path = args.dataset_dir / "data/chunk-000/file-000.parquet"
    episode_path = args.dataset_dir / "meta/episodes/chunk-000/file-000.parquet"
    handeye_video = (
        args.dataset_dir / "videos/observation.images.handeye/chunk-000/file-000.mp4"
    )
    fixed_video = args.dataset_dir / "videos/observation.images.fixed/chunk-000/file-000.mp4"
    required = [
        data_path,
        episode_path,
        handeye_video,
        fixed_video,
        args.model_dir / "model.safetensors",
        args.model_dir / "policy_preprocessor_step_3_normalizer_processor.safetensors",
    ]
    for path in required:
        if not path.is_file():
            raise FileNotFoundError(path)

    data = pq.read_table(data_path).to_pydict()
    episodes = pq.read_table(episode_path).to_pydict()
    episode_row = episodes["episode_index"].index(EPISODE)
    length = int(episodes["length"][episode_row])
    dataset_from = int(episodes["dataset_from_index"][episode_row])
    video_start = float(
        episodes["videos/observation.images.handeye/from_timestamp"][episode_row]
    )

    args.output.mkdir(parents=True, exist_ok=True)
    cases = []
    for case_index, fraction in enumerate(FRACTIONS):
        frame_index = round((length - 1) * fraction)
        global_index = dataset_from + frame_index
        timestamp = float(data["timestamp"][global_index])
        state = [float(value) for value in data["observation.state"][global_index]]
        case_dir = args.output / f"case-{case_index:03d}"
        case_dir.mkdir(parents=True, exist_ok=True)
        handeye = case_dir / "handeye.rgb"
        fixed = case_dir / "fixed.rgb"
        state_file = case_dir / "state.f32le"
        extract_rgb(args.ffmpeg, handeye_video, video_start + timestamp, handeye)
        extract_rgb(args.ffmpeg, fixed_video, video_start + timestamp, fixed)
        state_file.write_bytes(struct.pack("<6f", *state))
        case = {
            "case_id": case_index,
            "fraction": fraction,
            "episode_index": EPISODE,
            "frame_index": frame_index,
            "global_index": global_index,
            "episode_timestamp_seconds": timestamp,
            "video_timestamp_seconds": video_start + timestamp,
            "state": state,
            "files": {
                "handeye.rgb": sha256(handeye),
                "fixed.rgb": sha256(fixed),
                "state.f32le": sha256(state_file),
            },
        }
        (case_dir / "manifest.json").write_text(
            json.dumps(case, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        cases.append(case)

    manifest = {
        "format_version": 1,
        "model_repo": MODEL_REPO,
        "model_commit": MODEL_COMMIT,
        "dataset_repo": DATASET_REPO,
        "dataset_commit": DATASET_COMMIT,
        "image": {"width": WIDTH, "height": HEIGHT, "channels": 3, "layout": "HWC RGB u8"},
        "state": {"shape": [6], "dtype": "float32-le"},
        "action": {"shape": [100, 6], "dtype": "float32-le"},
        "model_sha256": sha256(args.model_dir / "model.safetensors"),
        "normalizer_sha256": sha256(
            args.model_dir / "policy_preprocessor_step_3_normalizer_processor.safetensors"
        ),
        "cases": cases,
    }
    (args.output / "manifest.json").write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    print(f"prepared {len(cases)} cases in {args.output}")


if __name__ == "__main__":
    main()
