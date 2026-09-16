#!/usr/bin/env python3
"""使用LeRobot官方ACTPolicy为冻结输入生成完整PyTorch参考输出。"""

from __future__ import annotations

import argparse
import hashlib
import inspect
import json
import sys
from pathlib import Path

import numpy as np
import torch

# 强制使用工作区中的LeRobot源码，避免conda环境里的其他版本偷偷成为参考。
WORKSPACE_ROOT = Path(__file__).resolve().parents[4]
LEROBOT_SRC = (WORKSPACE_ROOT / "lerobot" / "src").resolve()
sys.path.insert(0, str(LEROBOT_SRC))

from lerobot.configs.policies import PreTrainedConfig
from lerobot.policies.act.modeling_act import ACTPolicy
from lerobot.policies.factory import make_pre_post_processors


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--cases", type=Path, required=True)
    args = parser.parse_args()

    torch.manual_seed(0)
    torch.set_num_threads(1)
    policy_source = Path(inspect.getfile(ACTPolicy)).resolve()
    if not policy_source.is_relative_to(LEROBOT_SRC):
        raise RuntimeError(f"ACTPolicy did not come from local lerobot/src: {policy_source}")
    print(f"PyTorch reference source: {policy_source}")
    config = PreTrainedConfig.from_pretrained(args.model_dir)
    config.device = "cpu"
    config.use_amp = False
    # 直接执行lerobot/src中的ACTPolicy，它是本实验的数值真值实现。
    policy = ACTPolicy.from_pretrained(args.model_dir, config=config, strict=True)
    policy.eval()
    preprocessor, postprocessor = make_pre_post_processors(
        policy_cfg=config,
        pretrained_path=str(args.model_dir),
        preprocessor_overrides={"device_processor": {"device": "cpu"}},
        postprocessor_overrides={"device_processor": {"device": "cpu"}},
    )

    manifest = json.loads((args.cases / "manifest.json").read_text(encoding="utf-8"))
    for case in manifest["cases"]:
        case_dir = args.cases / f"case-{case['case_id']:03d}"
        handeye = np.fromfile(case_dir / "handeye.rgb", dtype=np.uint8).reshape(360, 640, 3)
        fixed = np.fromfile(case_dir / "fixed.rgb", dtype=np.uint8).reshape(360, 640, 3)
        state = np.fromfile(case_dir / "state.f32le", dtype="<f4")
        observation = {
            "observation.images.handeye": torch.from_numpy(handeye.copy())
            .to(torch.float32)
            .div_(255.0)
            .permute(2, 0, 1)
            .contiguous(),
            "observation.images.fixed": torch.from_numpy(fixed.copy())
            .to(torch.float32)
            .div_(255.0)
            .permute(2, 0, 1)
            .contiguous(),
            "observation.state": torch.from_numpy(state.copy()),
        }
        with torch.inference_mode():
            batch = preprocessor(observation)
            actions = policy.predict_action_chunk(batch)[0]
            # 后处理器接受任意前导维度，最后一维保持action_dim=6。
            actions = postprocessor(actions).to(torch.float32).cpu().contiguous()
        if tuple(actions.shape) != (100, 6) or not torch.isfinite(actions).all():
            raise RuntimeError(f"invalid output for {case_dir.name}: {tuple(actions.shape)}")
        values = actions.numpy().astype("<f4", copy=False)
        (case_dir / "pytorch-action.f32le").write_bytes(values.tobytes())
        summary = {
            "shape": [100, 6],
            "dtype": "float32-le",
            "sha256": hashlib.sha256(values.tobytes()).hexdigest(),
            "min": values.min(axis=0).tolist(),
            "max": values.max(axis=0).tolist(),
            "mean": values.mean(axis=0).tolist(),
        }
        (case_dir / "pytorch-action.json").write_text(
            json.dumps(summary, indent=2) + "\n", encoding="utf-8"
        )
        print(case_dir.name, "action[0]=", values[0].tolist())


if __name__ == "__main__":
    main()
