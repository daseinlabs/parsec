"""Locate and drive the `parsec` proxy binary as a BLACK BOX.

Dependency direction (CLAUDE.md §7): bench -> proxy -> engine. The bench never
links proxy internals — it spawns the compiled binary and speaks HTTP to it,
exactly like a user's Claude Code would. Shared by the parsec arm
(parsec_bench.arms.parsec) and the replay mode (parsec_bench.replay).

The proxy's config surface is env-only (packages/proxy/src/server.rs run()):
  PARSEC_PROXY_PORT   port to bind on 127.0.0.1
  PARSEC_UPSTREAM     upstream base URL (the run gateway / mock upstream)
  HOME                the savings ledger lands at $HOME/.parsec/ledger.jsonl
  PARSEC_BRAIN_URL + PARSEC_BRAIN_DEV_RAW=1
                      activate the brain scorer (v0 dev raw-text contract);
                      absent -> passthrough-shaped curation (wire freeze only)
  PARSEC_BRAIN_KEY / PARSEC_BRAIN_TIMEOUT_MS / PARSEC_TARGET_COV /
  PARSEC_TOOL_PRUNE / PARSEC_TOOL_CUT / PARSEC_FREEZE
                      passed through verbatim when set (docs/brain-serving-v0.md)
"""

from __future__ import annotations

import os
import socket
import subprocess
import time
from pathlib import Path
from typing import Optional

# Brain-related env the proxy reads; forwarded verbatim into the spawned
# process when set in the operator's environment.
PROXY_PASSTHROUGH_ENV = (
    "PARSEC_BRAIN_KEY", "PARSEC_BRAIN_TIMEOUT_MS", "PARSEC_TARGET_COV",
    "PARSEC_TOOL_PRUNE", "PARSEC_TOOL_CUT", "PARSEC_FREEZE",
)


def repo_root() -> Path:
    """The learner repo root (this file lives at packages/bench/src/parsec_bench/)."""
    return Path(__file__).resolve().parents[4]


def resolve_proxy_bin(explicit: Optional[str] = None, *, build: bool = True) -> str:
    """Path to the `parsec` binary: $PARSEC_BIN > explicit > target/release >
    target/debug > (optionally) `cargo build --bin parsec`.

    Raises FileNotFoundError with an actionable message when nothing resolves.
    """
    cands = [os.environ.get("PARSEC_BIN"), explicit]
    root = repo_root()
    cands += [str(root / "target" / "release" / "parsec"),
              str(root / "target" / "debug" / "parsec")]
    for c in cands:
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    if build:
        proc = subprocess.run(["cargo", "build", "-q", "--bin", "parsec"],
                              cwd=str(root), capture_output=True, text=True)
        built = root / "target" / "debug" / "parsec"
        if proc.returncode == 0 and built.is_file():
            return str(built)
    raise FileNotFoundError(
        "parsec proxy binary not found. Build it (`cargo build --release --bin "
        "parsec` at the repo root) or point PARSEC_BIN at it.")


def free_port() -> int:
    """An OS-assigned free TCP port on 127.0.0.1 (bind(0), read, release).

    Small race window between release and the proxy's bind; ProxyProcess.start
    retries on a failed come-up.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class ProxyProcess:
    """One running `parsec proxy` bound to 127.0.0.1:<port>.

    home_dir isolates the savings ledger: the proxy writes to
    $HOME/.parsec/ledger.jsonl, so a per-run home gives a per-run ledger.
    brain_url (+ the implied PARSEC_BRAIN_DEV_RAW=1) turns on the real-scorer
    path; None runs the passthrough-shaped machinery (wire freeze + ledger,
    no cuts).
    """

    def __init__(self, upstream: str, home_dir: str, *,
                 brain_url: Optional[str] = None,
                 bin_path: Optional[str] = None,
                 env_extra: Optional[dict] = None) -> None:
        self.upstream = upstream
        self.home_dir = str(home_dir)
        self.brain_url = brain_url
        self.bin_path = resolve_proxy_bin(bin_path)
        self.env_extra = dict(env_extra or {})
        self.port: int = 0
        self._proc: Optional[subprocess.Popen] = None

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    @property
    def ledger_path(self) -> str:
        return str(Path(self.home_dir) / ".parsec" / "ledger.jsonl")

    def _env(self) -> dict:
        env = dict(os.environ)
        env["PARSEC_PROXY_PORT"] = str(self.port)
        env["PARSEC_UPSTREAM"] = self.upstream
        env["HOME"] = self.home_dir
        if self.brain_url:
            env["PARSEC_BRAIN_URL"] = self.brain_url
            # v0 dev posture: the raw-text scorer only activates with the
            # explicit opt-in flag (docs/brain-serving-v0.md) — our machines only.
            env.setdefault("PARSEC_BRAIN_DEV_RAW", "1")
        else:
            env.pop("PARSEC_BRAIN_URL", None)
            env.pop("PARSEC_BRAIN_DEV_RAW", None)
        for k in PROXY_PASSTHROUGH_ENV:
            v = os.environ.get(k)
            if v is not None:
                env[k] = v
        env.update(self.env_extra)
        return env

    def start(self, timeout_s: float = 15.0, attempts: int = 3) -> "ProxyProcess":
        """Spawn the binary and wait until the port accepts connections."""
        Path(self.home_dir).mkdir(parents=True, exist_ok=True)
        last = ""
        for _ in range(attempts):
            self.port = free_port()
            self._proc = subprocess.Popen(
                [self.bin_path, "proxy"], env=self._env(),
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            deadline = time.time() + timeout_s
            while time.time() < deadline:
                if self._proc.poll() is not None:
                    break  # died (e.g. port raced) — retry on a new port
                try:
                    with socket.create_connection(("127.0.0.1", self.port), timeout=0.25):
                        return self
                except OSError:
                    time.sleep(0.05)
            last = self._drain_stderr()
            self.stop()
        raise RuntimeError(f"parsec proxy never came up on 127.0.0.1 "
                           f"(bin={self.bin_path}): {last[:300]}")

    def _drain_stderr(self) -> str:
        if self._proc is None or self._proc.stderr is None:
            return ""
        try:
            return self._proc.stderr.read().decode("utf-8", "replace")
        except Exception:
            return ""

    def stop(self) -> None:
        """Terminate the proxy. Never raises."""
        proc, self._proc = self._proc, None
        if proc is None:
            return
        try:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        except Exception:
            pass

    def __enter__(self) -> "ProxyProcess":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()
