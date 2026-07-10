"""Official SWE-bench grading (clean-room).

Port of adaptive-context-clean/bench/grader.py, as-is (the grader seam).

The ONLY trustworthy signal for "did the agent fix the bug" is the project's own
hidden test suite, run in the canonical per-instance Docker image and graded by
exit code — never an LLM judge, never eval() of model output. This module shells
out to the official harness:

    python -m swebench.harness.run_evaluation -d <dataset> -s <split> -i <iid>
        -p <preds.json> --run_id <id> --report_dir <dir> ...

and parses the per-instance ``report.json`` it writes. We read
``tests_status.FAIL_TO_PASS`` / ``PASS_TO_PASS`` and the harness's own
``resolved`` flag. A task is a success iff the harness resolves it (every
FAIL_TO_PASS now passes AND every PASS_TO_PASS still passes); ``ftp`` is the
fraction of FAIL_TO_PASS tests that pass (partial-credit signal).

The first grade for an instance builds its image (multi-GB, minutes); the
``instance`` cache level reuses it on later grades. A pathological/hung suite is
killed and graded as an (unresolved) failure rather than blocking the batch.

Thin wrapper over the official SWE-bench evaluation harness — no third-party
or proprietary code is imported here.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


# Eval set. Defaults to SWE-bench Verified (the bloated-50 is a Verified subset);
# override via env for a different dataset/split without touching code.
DEFAULT_DATASET = os.environ.get("BENCH_SWEBENCH_DATASET", "SWE-bench/SWE-bench_Verified")
DEFAULT_SPLIT = os.environ.get("BENCH_SWEBENCH_SPLIT", "test")


@dataclass
class GradeResult:
    """Outcome of one graded patch.

    success            : harness resolved the instance (the headline pass/fail)
    ftp                : FAIL_TO_PASS pass fraction in [0,1] (partial credit)
    n_fail_to_pass     : total FAIL_TO_PASS tests for the instance
    n_fail_to_pass_passed
    n_pass_to_pass     : total PASS_TO_PASS (regression) tests
    n_pass_to_pass_passed
    wall_s             : grading wall-clock seconds
    error              : non-empty iff grading itself failed (no usable report)
    """

    success: bool
    ftp: float
    n_fail_to_pass: int = 0
    n_fail_to_pass_passed: int = 0
    n_pass_to_pass: int = 0
    n_pass_to_pass_passed: int = 0
    wall_s: float = 0.0
    error: str = ""


class SWEBenchGrader:
    """Thin driver over the official ``swebench.harness.run_evaluation`` CLI."""

    def __init__(
        self,
        dataset: str = DEFAULT_DATASET,
        split: str = DEFAULT_SPLIT,
        run_root: Optional[str] = None,
        timeout_s: int = 1800,
        cache_level: str = "instance",
    ):
        self.dataset = dataset
        self.split = split
        self.timeout_s = timeout_s
        self.cache_level = cache_level
        self.run_root = Path(run_root or (Path.home() / "cc_bench_runs"))
        self.run_root.mkdir(parents=True, exist_ok=True)

    def grade(self, instance_id: str, patch: str) -> GradeResult:
        """Grade one ``model_patch`` for ``instance_id`` and return the parsed result."""
        t0 = time.time()
        run_id = f"ccb_{uuid.uuid4().hex[:8]}"
        model = "code-compression-bench"
        with tempfile.TemporaryDirectory(prefix="ccb_swe_") as tmp:
            preds = Path(tmp) / "preds.json"
            preds.write_text(
                json.dumps([{
                    "instance_id": instance_id,
                    "model_name_or_path": model,
                    "model_patch": patch or "",
                }]),
                encoding="utf-8",
            )
            report_dir = self.run_root / run_id
            report_dir.mkdir(parents=True, exist_ok=True)
            cmd = [
                sys.executable, "-m", "swebench.harness.run_evaluation",
                "-d", self.dataset,
                "-s", self.split,
                "-i", instance_id,
                "-p", str(preds),
                "--run_id", run_id,
                "--max_workers", "1",
                "--cache_level", self.cache_level,
                "--timeout", str(self.timeout_s),
                "--report_dir", str(report_dir),
            ]
            try:
                proc = subprocess.run(
                    cmd, cwd=str(report_dir),
                    capture_output=True, text=True,
                    timeout=self.timeout_s + 600,
                )
            except subprocess.TimeoutExpired:
                # don't let one hung test suite stall the batch: kill its
                # container(s) and grade a (unresolved) failure.
                self._kill_run_containers(run_id)
                return GradeResult(
                    False, 0.0,
                    wall_s=time.time() - t0,
                    error=f"grade timed out (> {self.timeout_s + 600}s)",
                )
            return self._parse(report_dir, run_id, instance_id, t0, proc)

    @staticmethod
    def _kill_run_containers(run_id: str) -> None:
        """Kill eval containers left running for this grade (named with the run_id)."""
        try:
            ids = subprocess.run(
                ["docker", "ps", "-q", "--filter", f"name={run_id}"],
                capture_output=True, text=True, timeout=30,
            ).stdout.split()
            if ids:
                subprocess.run(["docker", "kill", *ids], capture_output=True, timeout=60)
        except Exception:
            pass

    def _parse(self, report_dir: Path, run_id: str, iid: str, t0: float, proc) -> GradeResult:
        # the per-instance report lands under a harness-versioned subtree; search
        # for the instance's own report first, then any report.json as a fallback.
        candidates = (
            list(report_dir.rglob(f"{iid}/report.json"))
            + list(report_dir.rglob("report.json"))
        )
        inst = None
        for c in candidates:
            try:
                data = json.loads(c.read_text(encoding="utf-8"))
                inst = data.get(iid, data)
                if isinstance(inst, dict) and ("tests_status" in inst or "resolved" in inst):
                    break
            except Exception:
                continue
        if not isinstance(inst, dict):
            return GradeResult(
                False, 0.0,
                wall_s=time.time() - t0,
                error=f"no report; stderr={(proc.stderr or '')[-500:]}",
            )

        ts = inst.get("tests_status", {}) or {}
        f2p = ts.get("FAIL_TO_PASS", {}) or {}
        p2p = ts.get("PASS_TO_PASS", {}) or {}
        f2p_pass = len(f2p.get("success", []))
        f2p_total = f2p_pass + len(f2p.get("failure", []))
        p2p_pass = len(p2p.get("success", []))
        p2p_total = p2p_pass + len(p2p.get("failure", []))
        resolved = bool(inst.get(
            "resolved",
            f2p_total > 0 and f2p_pass == f2p_total and p2p_pass == p2p_total,
        ))
        ftp = (f2p_pass / f2p_total) if f2p_total else 0.0
        return GradeResult(
            success=resolved,
            ftp=ftp,
            n_fail_to_pass=f2p_total,
            n_fail_to_pass_passed=f2p_pass,
            n_pass_to_pass=p2p_total,
            n_pass_to_pass_passed=p2p_pass,
            wall_s=time.time() - t0,
        )
