#!/usr/bin/env python3
"""分析PyTorch ACTPolicy真值与Rust ACT后端的3000个逐元素结果。"""

from __future__ import annotations

import argparse
import csv
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np


JOINT_NAMES = (
    "shoulder_pan",
    "shoulder_lift",
    "elbow_flex",
    "wrist_flex",
    "wrist_roll",
    "gripper",
)


def load_rows(path: Path) -> dict[str, np.ndarray]:
    with path.open(newline="", encoding="utf-8") as source:
        rows = list(csv.DictReader(source))
    if len(rows) != 3000:
        raise RuntimeError(f"expected 3000 rows, got {len(rows)}")
    return {
        "case": np.array([int(row["case_id"]) for row in rows]),
        "step": np.array([int(row["step"]) for row in rows]),
        "joint": np.array([int(row["joint"]) for row in rows]),
        "reference": np.array([float(row["pytorch_reference"]) for row in rows]),
        "actual": np.array([float(row["rust_actual"]) for row in rows]),
        "signed": np.array([float(row["signed_error"]) for row in rows]),
        "absolute": np.array([float(row["abs_error"]) for row in rows]),
        "tolerance": np.array([float(row["tolerance"]) for row in rows]),
        "passed": np.array([row["pass"] == "true" for row in rows]),
    }


def percentile(values: np.ndarray, value: float) -> float:
    return float(np.percentile(values, value))


def write_statistics(data: dict[str, np.ndarray], output: Path) -> None:
    fields = (
        "scope",
        "count",
        "mean_abs_error",
        "median_abs_error",
        "p95_abs_error",
        "p99_abs_error",
        "max_abs_error",
        "rmse",
        "max_tolerance_ratio",
        "failed",
    )

    def summarize(scope: str, mask: np.ndarray) -> dict[str, object]:
        absolute = data["absolute"][mask]
        signed = data["signed"][mask]
        ratios = absolute / data["tolerance"][mask]
        return {
            "scope": scope,
            "count": len(absolute),
            "mean_abs_error": f"{absolute.mean():.9f}",
            "median_abs_error": f"{np.median(absolute):.9f}",
            "p95_abs_error": f"{percentile(absolute, 95):.9f}",
            "p99_abs_error": f"{percentile(absolute, 99):.9f}",
            "max_abs_error": f"{absolute.max():.9f}",
            "rmse": f"{np.sqrt(np.mean(signed**2)):.9f}",
            "max_tolerance_ratio": f"{ratios.max():.9f}",
            "failed": int(np.count_nonzero(~data["passed"][mask])),
        }

    summaries = [summarize("all", np.ones(3000, dtype=bool))]
    summaries.extend(
        summarize(f"case-{case_id:03d}", data["case"] == case_id)
        for case_id in range(5)
    )
    summaries.extend(
        summarize(f"joint-{joint_id}-{name}", data["joint"] == joint_id)
        for joint_id, name in enumerate(JOINT_NAMES)
    )
    with output.open("w", newline="", encoding="utf-8") as target:
        writer = csv.DictWriter(target, fieldnames=fields)
        writer.writeheader()
        writer.writerows(summaries)


def plot_overview(data: dict[str, np.ndarray], output: Path, system_label: str) -> None:
    figure, axes = plt.subplots(2, 2, figsize=(14, 10), constrained_layout=True)

    # 散点图直接展示全3000个真值与被测值是否落在y=x上。
    axis = axes[0, 0]
    axis.scatter(data["reference"], data["actual"], s=7, alpha=0.35, color="#1565c0")
    lower = min(data["reference"].min(), data["actual"].min())
    upper = max(data["reference"].max(), data["actual"].max())
    axis.plot([lower, upper], [lower, upper], color="#d32f2f", linewidth=1.5, label="y = x")
    axis.set_title("PyTorch reference vs Rust output (3000 values)")
    axis.set_xlabel("PyTorch ACTPolicy")
    axis.set_ylabel(system_label)
    axis.legend()
    axis.grid(alpha=0.2)

    # 对数直方图避免小误差全部挤在线性坐标原点。
    axis = axes[0, 1]
    positive = data["absolute"][data["absolute"] > 0]
    bins = np.logspace(np.log10(positive.min()), np.log10(positive.max()), 35)
    axis.hist(positive, bins=bins, color="#00897b", alpha=0.85)
    axis.set_xscale("log")
    axis.set_title("Absolute-error distribution")
    axis.set_xlabel("absolute error (log scale)")
    axis.set_ylabel("value count")
    axis.grid(axis="y", alpha=0.2)

    axis = axes[1, 0]
    cases = np.arange(5)
    means = np.array([data["absolute"][data["case"] == case].mean() for case in cases])
    p99 = np.array([percentile(data["absolute"][data["case"] == case], 99) for case in cases])
    maxima = np.array([data["absolute"][data["case"] == case].max() for case in cases])
    width = 0.25
    axis.bar(cases - width, means, width, label="mean", color="#64b5f6")
    axis.bar(cases, p99, width, label="p99", color="#ffb74d")
    axis.bar(cases + width, maxima, width, label="max", color="#e57373")
    axis.set_title("Error by real-data case")
    axis.set_xlabel("case")
    axis.set_ylabel("absolute error")
    axis.set_xticks(cases, [f"{case:03d}" for case in cases])
    axis.legend()
    axis.grid(axis="y", alpha=0.2)

    axis = axes[1, 1]
    ratios = data["absolute"] / data["tolerance"] * 100.0
    joint_ratios = [ratios[data["joint"] == joint] for joint in range(6)]
    axis.boxplot(joint_ratios, tick_labels=JOINT_NAMES, showfliers=False)
    axis.set_title("Tolerance utilization by joint")
    axis.set_ylabel("abs_error / tolerance (%)")
    axis.tick_params(axis="x", rotation=22)
    axis.grid(axis="y", alpha=0.2)

    figure.suptitle(f"ACT correctness: LeRobot PyTorch vs {system_label}", fontsize=16)
    figure.savefig(output, dpi=180)
    plt.close(figure)


def plot_heatmaps(data: dict[str, np.ndarray], output: Path, system_label: str) -> None:
    figure, axes = plt.subplots(5, 1, figsize=(14, 13), constrained_layout=True)
    maximum = data["absolute"].max()
    image = None
    for case_id, axis in enumerate(axes):
        mask = data["case"] == case_id
        matrix = np.zeros((100, 6), dtype=np.float64)
        matrix[data["step"][mask], data["joint"][mask]] = data["absolute"][mask]
        image = axis.imshow(
            matrix.T,
            aspect="auto",
            interpolation="nearest",
            cmap="magma",
            vmin=0,
            vmax=maximum,
        )
        axis.set_title(f"case-{case_id:03d}", loc="left")
        axis.set_ylabel("joint")
        axis.set_yticks(range(6), JOINT_NAMES)
    axes[-1].set_xlabel("action step (0..99)")
    figure.colorbar(image, ax=axes, label="absolute error", shrink=0.8)
    figure.suptitle(
        f"{system_label}: absolute error at every action step and joint",
        fontsize=16,
    )
    figure.savefig(output, dpi=180)
    plt.close(figure)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--csv", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--system-label", default="Rust act-runtime")
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)

    data = load_rows(args.csv)
    write_statistics(data, args.output_dir / "statistics.csv")
    plot_overview(data, args.output_dir / "error-overview.png", args.system_label)
    plot_heatmaps(data, args.output_dir / "error-heatmaps.png", args.system_label)
    print(
        "analysis complete:",
        f"max_abs_error={data['absolute'].max():.9f}",
        f"max_tolerance_usage={(data['absolute'] / data['tolerance']).max() * 100:.3f}%",
        f"failed={np.count_nonzero(~data['passed'])}",
    )


if __name__ == "__main__":
    main()
