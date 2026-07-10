"""Locate and drive the `dasein` proxy binary as a BLACK BOX.

Dependency direction (CLAUDE.md §7): bench -> proxy -> engine. The bench never
links proxy internals — it spawns the compiled binary and speaks HTTP to it,
exactly like a user's Claude Code would. Shared by the dasein arm
(dasein_bench.arms.dasein) and the replay mode (dasein_bench.replay).

The proxy's config surface is env-only (packages/proxy/src/server.rs run()):
  DASEIN_PROXY_PORT   port to bind on 127.0.0.1
  DASEIN_UPSTREAM     upstream base URL (the run gateway / mock upstream)
  HOME                the savings ledger lands at $HOME/.dasein/ledger.jsonl
  DASEIN_BRAIN_URL + DASEIN_BRAIN_DEV_RAW=1
                      activate the brain scorer (v0 dev raw-text contract);
                      absent -> passthrough-shaped curation (wire freeze only)
  DASEIN_BRAIN_KEY / DASEIN_BRAIN_TIMEOUT_MS / DASEIN_TARGET_COV /
  DASEIN_TOOL_PRUNE / DASEIN_TOOL_CUT / DASEIN_FREEZE
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
    "DASEIN_BRAIN_KEY", "DASEIN_BRAIN_TIMEOUT_MS", "DASEIN_TARGET_COV",
    "DASEIN_TOOL_PRUNE", "DASEIN_TOOL_CUT", "DASEIN_FREEZE",
)


def repo_root() -> Path:
    """The learner repo root (this file lives at packages/bench/src/dasein_bench/)."""
    return Path(__file__).resolve().parents[4]


def resolve_proxy_bin(explicit: Optional[str] = None, *, build: bool = True) -> str:
    """Path to the `dasein` binary: $DASEIN_BIN > explicit > target/release >
    target/debug > (optionally) `cargo build --bin dasein`.

    Raises FileNotFoundError with an actionable message when nothing resolves.
    """
    cands = [os.environ.get("DASEIN_BIN"), explicit]
    root = repo_root()
    cands += [str(root / "target" / "release" / "dasein"),
              str(root / "target" / "debug" / "dasein")]
    for c in cands:
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    if build:
        proc = subprocess.run(["cargo", "build", "-q", "--bin", "dasein"],
                              cwd=str(root), capture_output=True, text=True)
        built = root / "target" / "debug" / "dasein"
        if proc.returncode == 0 and built.is_file():
            return str(built)
    raise FileNotFoundError(
        "dasein proxy binary not found. Build it (`cargo build --release --bin "
        "dasein` at the repo root) or point DASEIN_BIN at it.")


def free_port() -> int:
    """An OS-assigned free TCP port on 127.0.0.1 (bind(0), read, release).

    Small race window between release and the proxy's bind; ProxyProcess.start
    retries on a failed come-up.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class ProxyProcess:
    """One running `dasein proxy` bound to 127.0.0.1:<port>.

    home_dir isolates the savings ledger: the proxy writes to
    $HOME/.dasein/ledger.jsonl, so a per-run home gives a per-run ledger.
    brain_url (+ the implied DASEIN_BRAIN_DEV_RAW=1) turns on the real-scorer
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
        return str(Path(self.home_dir) / ".dasein" / "ledger.jsonl")

    def _env(self) -> dict:
        env = dict(os.environ)
        env["DASEIN_PROXY_PORT"] = str(self.port)
        env["DASEIN_UPSTREAM"] = self.upstream
        env["HOME"] = self.home_dir
        if self.brain_url:
            env["DASEIN_BRAIN_URL"] = self.brain_url
            # v0 dev posture: the raw-text scorer only activates with the
            # explicit opt-in flag (docs/brain-serving-v0.md) — our machines only.
            env.setdefault("DASEIN_BRAIN_DEV_RAW", "1")
        else:
            env.pop("DASEIN_BRAIN_URL", None)
            env.pop("DASEIN_BRAIN_DEV_RAW", None)
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
        raise RuntimeError(f"dasein proxy never came up on 127.0.0.1 "
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
