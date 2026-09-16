#!/usr/bin/env python3
"""把模型、归一化参数和五组测试向量打成单一二进制文件。

这个 bundle 只是实验资产容器，不是新的模型格式。seL4 Root Task 从
CPIO 中取出整个文件，Rust 运行时再以只读 slice 直接引用内部数据。
"""

from __future__ import annotations

import argparse
import struct
from pathlib import Path


MAGIC = b"ACTBNDL1"
CASE_FILES = (
    "handeye.rgb",
    "fixed.rgb",
    "state.f32le",
    "pytorch-action.f32le",
)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--cases-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    model = (args.model_dir / "model.safetensors").read_bytes()
    stats = (
        args.model_dir
        / "policy_preprocessor_step_3_normalizer_processor.safetensors"
    ).read_bytes()

    payloads = [model, stats]
    for case_index in range(5):
        case_dir = args.cases_dir / f"case-{case_index:03d}"
        payloads.extend((case_dir / name).read_bytes() for name in CASE_FILES)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("wb") as output:
        # Header使用小端：8字节magic、version、case_count、model/stats长度。
        output.write(MAGIC)
        output.write(struct.pack("<IIQQ", 1, 5, len(model), len(stats)))
        for payload in payloads:
            output.write(payload)

    print(f"ACT bundle: {args.output} ({args.output.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
