#!/usr/bin/env python3
"""Provision per-instance repo checkouts + test environments for the bench agent.

Port of adaptive-context-clean/bench/prepare_repos.py, as-is (local single-machine provisioning).

The fixed agent (headless Claude Code) edits a working copy of each SWE-bench
task's repository at its ``base_commit`` and we capture its work as
``git diff``. The runner's cwd is ``AC_REPO_ROOT/<instance_id>`` but the runner
does NOT create that checkout — this script does, reproducibly:

  * resolve each instance's ``repo`` + ``base_commit`` + ``version`` from the
    SWE-bench Verified dataset (by id),
  * clone each distinct repo ONCE into a cache,
  * add a clean ``git worktree`` at ``base_commit`` for every instance under
    ``AC_REPO_ROOT/<instance_id>``,
  * BUILD AN ISOLATED PER-INSTANCE TEST ENV at ``AC_REPO_ROOT/.venvs/<iid>``
    (exact-or-nearest Python via ``uv`` + the SWE-bench spec's install + pinned
    deps), so the agent's ``pytest``/``python`` resolve to the task's own
    toolchain instead of the host's mismatched global tools.

Why the env matters (the bug it fixes): a bare worktree gives the agent the
SOURCE but no working test env. ``pytest`` then resolves to the host's global
modern pytest, which cannot even import an old repo's ``conftest.py``
(``ImportError: cannot import name 'Testdir'``), so every test run errors out.
The agent keeps re-running the tests (with pip/PYTHONPATH/python3 variations) to
get a green signal it can never reach -> a call/cost explosion that hits EVERY
arm (curating or not). The docker grader has a correct per-instance env, so
grading still works — only the agent's LOCAL verification is broken, which is
invisible until you read the transcripts. The per-instance venv here is for that
local verification; the docker grader remains authoritative for the graded result.

Worktrees share the cache's object store (space-cheap) but each has its own
index + working tree, so parallel agents editing different instances of the
same repo never collide, and ``git -C <dir> add -A && git diff --cached`` in the
runner yields exactly that instance's patch. The venv lives OUTSIDE the worktree
(a sibling under ``.venvs/``) and build artifacts (``*.egg-info`` etc.) are added
to the cache repo's ``.git/info/exclude``, so neither pollutes the captured diff.

Usage (on the runner box):
    AC_REPO_ROOT=~/task_repos python -m dasein_bench.prepare_repos --tasks tasks_bloated100.json
    # skip env build (worktrees only):  ... --no-env
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

DEFAULT_DATASET = os.environ.get("AC_SWEBENCH_DATASET_HF", "princeton-nlp/SWE-bench_Verified")
DEFAULT_SPLIT = os.environ.get("AC_SWEBENCH_SPLIT", "test")


def _sh(args: list[str], cwd: str | None = None, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(args, cwd=cwd, check=check, capture_output=True, text=True)


def _load_ids(tasks_path: str) -> list[str]:
    data = json.loads(Path(tasks_path).read_text(encoding="utf-8"))
    if isinstance(data, dict):
        ids = data.get("instances") or data.get("instance_ids") or data.get("ids")
    else:
        ids = data
    if not ids:
        raise SystemExit(f"no instance ids found in {tasks_path}")
    return [str(x) for x in ids]


def _resolve_instances(ids: list[str], dataset: str, split: str) -> dict[str, dict]:
    """id -> {repo, base_commit, version} from the SWE-bench dataset."""
    try:
        from datasets import load_dataset  # lazy: heavy import
    except ImportError:
        raise SystemExit("pip install datasets  (needed to resolve repo/base_commit)")
    ds = load_dataset(dataset, split=split)
    want = set(ids)
    out: dict[str, dict] = {}
    for row in ds:
        iid = row.get("instance_id")
        if iid in want:
            out[iid] = {
                "repo": row["repo"],
                "base_commit": row["base_commit"],
                "version": row.get("version"),
            }
    missing = want - set(out)
    if missing:
        raise SystemExit(f"{len(missing)} ids not found in {dataset}:{split}: {sorted(missing)[:5]}...")
    return out


def _ensure_cache_clone(repo: str, cache: Path) -> Path:
    cdir = cache / repo.replace("/", "__")
    if not (cdir / ".git").exists():
        cdir.parent.mkdir(parents=True, exist_ok=True)
        print(f"  clone {repo} -> {cdir}")
        _sh(["git", "clone", "-q", f"https://github.com/{repo}.git", str(cdir)])
    return cdir


def _ensure_commit(cdir: Path, base: str) -> None:
    if _sh(["git", "cat-file", "-e", f"{base}^{{commit}}"], cwd=str(cdir), check=False).returncode != 0:
        # base commit not in the default clone (shallow/old) — fetch it explicitly.
        _sh(["git", "fetch", "-q", "origin", base], cwd=str(cdir), check=False)


def _provision_worktree(cdir: Path, dest: Path, base: str) -> None:
    if dest.exists():
        # idempotent: drop and re-add so the tree is clean at base.
        _sh(["git", "worktree", "remove", "--force", str(dest)], cwd=str(cdir), check=False)
        if dest.exists():  # not a registered worktree (stale dir) — nuke it
            _sh(["rm", "-rf", str(dest)], check=False)
    dest.parent.mkdir(parents=True, exist_ok=True)
    _sh(["git", "worktree", "add", "--force", "--detach", str(dest), base], cwd=str(cdir))
    _deleak_checkout(dest)


def _deleak_checkout(dest: Path) -> None:
    """Isolate the checkout from the shared cache history so the agent cannot git-log/show the fix
    commit (worktrees share the cache repo's refs, which include every future fix). Re-init as a
    standalone 1-commit repo at the base snapshot. Idempotent; fail-open."""
    try:
        gp = dest / ".git"
        if gp.exists():
            if gp.is_dir():
                _sh(["rm", "-rf", str(gp)], check=False)
            else:
                gp.unlink()
        _sh(["git", "init", "-q"], cwd=str(dest), check=False)
        _sh(["git", "add", "-A"], cwd=str(dest), check=False)
        _sh(["git", "-c", "user.email=x", "-c", "user.name=x", "-c", "commit.gpgsign=false",
             "commit", "-qm", "base"], cwd=str(dest), check=False)
    except Exception:
        pass


def _harden_excludes(cdir: Path) -> None:
    """Keep env-build artifacts out of the agent's captured ``git diff``.

    ``pip install -e .`` may drop ``*.egg-info`` into the worktree; ``add -A``
    would otherwise stage it. ``.git/info/exclude`` is shared by all linked
    worktrees of this cache repo, so one write covers every instance.
    """
    ex = cdir / ".git" / "info" / "exclude"
    pats = ["*.egg-info/", "*.egg-link", ".eggs/", "build/", "__pycache__/", "*.pyc", ".venvs/", ".venv/"]
    try:
        cur = ex.read_text(encoding="utf-8") if ex.exists() else ""
        add = [p for p in pats if p not in cur]
        if add:
            ex.parent.mkdir(parents=True, exist_ok=True)
            sep = "" if (not cur or cur.endswith("\n")) else "\n"
            ex.write_text(cur + sep + "\n".join(add) + "\n", encoding="utf-8")
    except Exception:
        pass


# ── per-instance test environment (so the agent's pytest runs in one shot) ────

_UV_OK_MINORS: dict[str, bool] = {}


def _find_uv(explicit: str | None) -> str | None:
    cands = [explicit] if explicit else []
    cands += [os.path.expanduser("~/.local/bin/uv"), os.path.expanduser("~/.cargo/bin/uv"), "uv"]
    for c in cands:
        if not c:
            continue
        r = _sh(["bash", "-lc", f"command -v {c} >/dev/null 2>&1 && echo OK || true"], check=False)
        if r.stdout.strip() == "OK" or (os.path.sep in c and os.path.exists(c)):
            return c
    return None


def _uv_can(uv: str, ver: str) -> bool:
    if ver not in _UV_OK_MINORS:
        probe = f"/tmp/_uvprobe_{ver.replace('.', '_')}"
        r = _sh([uv, "venv", "--python", ver, probe], check=False)
        _UV_OK_MINORS[ver] = (r.returncode == 0)
        _sh(["rm", "-rf", probe], check=False)
    return _UV_OK_MINORS[ver]


def _resolve_py(uv: str, want: str) -> str:
    """Exact Python if uv can fetch it, else climb to the nearest higher minor.

    The agent's env is for LOCAL verification, not the graded result, so a
    nearest-newer Python (e.g. 3.6 -> 3.7/3.8 for old Django, which runs there)
    is sufficient to give the agent a working test loop.
    """
    ladder = ["3.6", "3.7", "3.8", "3.9", "3.10", "3.11", "3.12"]
    start = ladder.index(want) if want in ladder else 0
    for ver in ladder[start:]:
        if _uv_can(uv, ver):
            return ver
    return want  # nothing available — let the venv step fail loudly


def _spec_for(repo: str, version: str | None) -> dict | None:
    if not version:
        return None
    try:
        from swebench.harness.constants import MAP_REPO_VERSION_TO_SPECS as M
        return M.get(repo, {}).get(version)
    except Exception:
        return None


def _pip_args(install_cmd: str) -> list[str]:
    """Spec install string ('python -m pip install -e .[test] --verbose') -> uv-pip args
    (['-e', '.[test]']). uv's editable build is more robust than the venv's pip for old
    C-extension repos (astropy/matplotlib), which fail to build an editable wheel under pip."""
    s = (install_cmd or "").strip()
    for pre in ("python -m pip install ", "python3 -m pip install ", "pip install ", "pip3 install "):
        if s.startswith(pre):
            s = s[len(pre):]
            break
    return [t for t in s.split() if t not in ("--verbose", "-v")]


def _provision_env(uv: str, venv_dir: Path, worktree: Path, spec: dict) -> tuple[bool, str]:
    if venv_dir.exists():
        _sh(["rm", "-rf", str(venv_dir)], check=False)
    venv_dir.parent.mkdir(parents=True, exist_ok=True)
    py = _resolve_py(uv, str(spec.get("python") or "3.9"))
    r = _sh([uv, "venv", "--python", py, "--seed", str(venv_dir)], check=False)
    if r.returncode != 0:
        return False, f"venv(py={py}) failed: {r.stderr.strip()[:160]}"
    pybin = str(venv_dir / "bin" / "python")
    shenv = {
        **os.environ,
        "PATH": str(venv_dir / "bin") + os.pathsep + os.environ.get("PATH", ""),
        "VIRTUAL_ENV": str(venv_dir),
    }
    shenv.pop("PYTHONHOME", None)
    shenv.pop("PYTHONPATH", None)
    # pre_install (arbitrary shell: sed edits / dep pins), then the repo install, then pinned test deps.
    for cmd in (spec.get("pre_install") or []):
        subprocess.run(cmd, shell=True, cwd=str(worktree), env=shenv, check=False,
                       capture_output=True, text=True)
    pkgs = spec.get("pip_packages") or []

    def _uvpip(extra: list[str]) -> subprocess.CompletedProcess:
        return subprocess.run([uv, "pip", "install", "--python", pybin, *extra],
                              cwd=str(worktree), env=shenv, capture_output=True, text=True)

    # Use uv for all pip ops: its editable build handles old C-extension repos
    # (astropy/matplotlib) that the venv's own pip can't build an editable wheel for.
    args = _pip_args(spec.get("install") or "python -m pip install -e .")
    ri = _uvpip(args)
    note = f"py={py}"
    if ri.returncode != 0:
        # Fallback for repos whose setup.py imports numpy/etc at build time: an isolated
        # editable build can't see them. Install the pinned deps first + the usual build
        # helpers, then retry the editable install WITHOUT build isolation.
        if pkgs:
            _uvpip(pkgs)
        _uvpip(["extension-helpers", "cython"])
        ri = _uvpip(args + ([] if "--no-build-isolation" in args else ["--no-build-isolation"]))
        if ri.returncode != 0:
            tail = (ri.stderr or ri.stdout or "").strip().replace("\n", " ")[-220:]
            return False, f"install failed (py={py}): {tail}"
        note = f"py={py} (no-build-isolation fallback)"
    if pkgs:
        _uvpip(pkgs)
    return True, note


def main() -> None:
    ap = argparse.ArgumentParser(description="Check out each task repo at base_commit + build its test env for the agent.")
    ap.add_argument("--tasks", default="tasks_bloated50.json")
    ap.add_argument("--repo-root", default=os.environ.get("AC_REPO_ROOT", ""),
                    help="where per-instance checkouts go (AC_REPO_ROOT/<iid>)")
    ap.add_argument("--cache", default=os.environ.get("REPO_CACHE", str(Path.home() / "repo_cache")),
                    help="per-repo full clones live here (shared across instances)")
    ap.add_argument("--dataset", default=DEFAULT_DATASET)
    ap.add_argument("--split", default=DEFAULT_SPLIT)
    ap.add_argument("--no-env", action="store_true",
                    help="provision worktrees only; skip the per-instance test env build")
    ap.add_argument("--uv", default=os.environ.get("UV_BIN", ""),
                    help="path to the uv binary (default: autodetect ~/.local/bin/uv, PATH)")
    ap.add_argument("--arms", default="",
                    help="comma list of arms -> a SEPARATE worktree+venv per (iid, arm) as "
                         "<root>/<iid>__<arm>, so all arms can run the SAME task in PARALLEL. "
                         "Empty = one shared <root>/<iid> tree (sequential-arm mode).")
    ap.add_argument("--jobs", type=int, default=int(os.environ.get("PREP_JOBS", "8")),
                    help="parallel env builds (the slow step)")
    a = ap.parse_args()

    repo_root = a.repo_root.strip()
    if not repo_root:
        raise SystemExit("set --repo-root or AC_REPO_ROOT (where the agent's cwd per instance lives)")
    repo_root_p = Path(repo_root).expanduser()
    cache = Path(a.cache).expanduser()
    repo_root_p.mkdir(parents=True, exist_ok=True)
    cache.mkdir(parents=True, exist_ok=True)
    venv_root = repo_root_p / ".venvs"

    uv = None if a.no_env else _find_uv(a.uv.strip() or None)
    if not a.no_env and uv is None:
        print("WARNING: uv not found -> building worktrees WITHOUT test envs "
              "(install uv: curl -LsSf https://astral.sh/uv/install.sh | sh)", file=sys.stderr)

    arms = [s.strip() for s in a.arms.split(",") if s.strip()]
    suffixes = ["__" + x for x in arms] if arms else [""]

    ids = _load_ids(a.tasks)
    print(f"resolving {len(ids)} instances from {a.dataset}:{a.split} "
          f"({len(ids)} tasks x {len(suffixes)} tree(s) = {len(ids) * len(suffixes)} units) ...")
    by_id = _resolve_instances(ids, a.dataset, a.split)

    # 1) WORKTREES — sequential (git worktree add on one cache repo must serialize on its lock).
    units: list[tuple] = []   # (dirname, repo, ver)
    for iid in ids:
        repo, base, ver = by_id[iid]["repo"], by_id[iid]["base_commit"], by_id[iid].get("version")
        try:
            cdir = _ensure_cache_clone(repo, cache)
            _ensure_commit(cdir, base)
            _harden_excludes(cdir)
            for suf in suffixes:
                dirname = iid + suf
                _provision_worktree(cdir, repo_root_p / dirname, base)
                units.append((dirname, repo, ver))
            print(f"[worktree] {iid:40s} {repo}@{base[:10]} x{len(suffixes)}")
        except subprocess.CalledProcessError as e:
            print(f"[FAIL worktree] {iid}: {e.stderr.strip()[:200]}", file=sys.stderr)
    print(f"DONE  {len(units)} worktrees under {repo_root_p}")

    if a.no_env or uv is None:
        return

    # pre-warm the uv python-version cache SEQUENTIALLY so parallel builds don't race on probe paths.
    for v in sorted({str((_spec_for(by_id[i]['repo'], by_id[i].get('version')) or {}).get('python') or '3.9') for i in ids}):
        _resolve_py(uv, v)

    # 2) ENVS — parallel (the slow step; each (iid,arm) venv is independent).
    from concurrent.futures import ThreadPoolExecutor

    def _build(unit):
        dirname, repo, ver = unit
        spec = _spec_for(repo, ver)
        if spec is None:
            return (dirname, False, f"no swebench spec for {repo}@{ver}")
        good, info = _provision_env(uv, venv_root / dirname, repo_root_p / dirname, spec)
        return (dirname, good, info)

    env_ok = 0
    env_fail: list[str] = []
    with ThreadPoolExecutor(max_workers=max(1, a.jobs)) as ex:
        for dirname, good, info in ex.map(_build, units):
            if good:
                env_ok += 1
            else:
                env_fail.append(dirname)
            print(f"  [env {'OK' if good else 'FAIL'}] {dirname:46s} {info}")
    print(f"ENV   {env_ok}/{len(units)} test envs built under {venv_root}")
    if env_fail:
        print(f"ENV FAIL ({len(env_fail)}): {env_fail}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
