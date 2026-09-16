#!/usr/bin/env python3
"""把PyTorch真值与Rust ACT后端输出展开为可直接审查的逐元素CSV。"""

from __future__ import annotations

import argparse
import csv
from pathlib import Path

import numpy as np


HEADER = (
    "case_id",
    "step",
    "joint",
    "pytorch_reference",
    "rust_actual",
    "signed_error",
    "abs_error",
    "tolerance",
    "pass",
)

# 与PyTorch torch.allclose采用相同的比较形式；这里使用其默认atol/rtol。
ABSOLUTE_TOLERANCE = 1e-8
RELATIVE_TOLERANCE = 1e-5


def rows_for_case(cases: Path, actual_root: Path, actual_name: str, case_id: int):
    directory = cases / f"case-{case_id:03d}"
    reference = np.fromfile(directory / "pytorch-action.f32le", dtype="<f4")
    actual = np.fromfile(
        actual_root / f"case-{case_id:03d}" / actual_name,
        dtype="<f4",
    )
    if reference.shape != (600,) or actual.shape != (600,):
        raise RuntimeError(f"case-{case_id:03d}: expected two 600-value outputs")
    for index, (expected, observed) in enumerate(zip(reference, actual, strict=True)):
        signed = float(observed) - float(expected)
        absolute = abs(signed)
        tolerance = ABSOLUTE_TOLERANCE + RELATIVE_TOLERANCE * abs(float(expected))
        yield (
            f"{case_id:03d}",
            index // 6,
            index % 6,
            f"{float(expected):.9f}",
            f"{float(observed):.9f}",
            f"{signed:.9f}",
            f"{absolute:.9f}",
            f"{tolerance:.9f}",
            str(absolute <= tolerance).lower(),
        )


def write_csv(path: Path, rows) -> int:
    count = 0
    with path.open("w", newline="", encoding="utf-8") as output:
        writer = csv.writer(output)
        writer.writerow(HEADER)
        for row in rows:
            writer.writerow(row)
            count += 1
    return count


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--actual-root", type=Path)
    parser.add_argument("--actual-name", default="rust-action.f32le")
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()

    actual_root = args.actual_root or args.cases
    output_dir = args.output_dir or args.cases
    output_dir.mkdir(parents=True, exist_ok=True)

    combined = []
    for case_id in range(5):
        rows = list(rows_for_case(args.cases, actual_root, args.actual_name, case_id))
        count = write_csv(output_dir / f"case-{case_id:03d}-pytorch-vs-rust.csv", rows)
        if count != 600:
            raise RuntimeError(f"case-{case_id:03d}: wrote {count} rows")
        combined.extend(rows)
    count = write_csv(output_dir / "all-cases-pytorch-vs-rust.csv", combined)
    print(f"comparison CSV: 5 x 600 = {count} rows")


if __name__ == "__main__":
    main()
