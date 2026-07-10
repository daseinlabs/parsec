#!/usr/bin/env python3
"""Freeze parity fixtures: drive the REAL curator.curate() decision/render
machinery over growing conversations, with the GNN scorer replaced by the
deterministic stub mirrored in Rust (freeze::stub_score_q / StubScorer).

torch / pyg_model / torch_curator are stubbed at module level — the patched
path never touches them. The curator instance is built via __new__ with
exactly the state curate() reads. _degrade is patched to RAISE so a silent
fail-open can never masquerade as parity.

Suites: two fixed-tau files (tau15/tau85) plus tauq (fixed_taus empty ->
per-pool qhat derived from the masked chunk texts, exercising the multi-owner
tau path). Each file carries multiple conversations, including one that opens
with an assistant message (step-0 births) and a grep line with an empty
basename ("src/:5:...", Python-falsy file).
"""
import hashlib
import json
import os
import sys
import types

ACC = os.environ.get("ACC_ROOT") or os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "adaptive-context-clean"))
sys.path.insert(0, ACC)


def _stub_module(name, attrs=()):
    mod = types.ModuleType(name)
    for a in attrs:
        setattr(mod, a, None)
    sys.modules[name] = mod
    return mod


class _AnyAttr(types.ModuleType):
    def __getattr__(self, k):
        return None


sys.modules["torch"] = _AnyAttr("torch")
_stub_module("adaptive_context.optimizer.pyg_model",
             ["PyGCurator", "attach_blocks", "attach_task", "edges"])
_stub_module("adaptive_context.optimizer.torch_curator",
             ["struct_features", "node_struct_with_type", "decided_extra_feats"])

from adaptive_context.optimizer.curator import ContextCurator  # noqa: E402

SCALE = 1_000_000


def stub_score_q(c) -> int:
    h = hashlib.sha256(f"{c.step}:{c.kind}:{c.text}".encode()).digest()
    return int.from_bytes(h[:8], "big") % SCALE


def stub_qhat_q(lc, mask_js) -> int:
    joined = "\x00".join(lc[j].text for j in mask_js)
    h = hashlib.sha256(f"qhat:{joined}".encode()).digest()
    return int.from_bytes(h[:8], "big") % SCALE


def make_curator(tau_q):
    """tau_q: int -> fixed_taus=[tau]; None -> empty fixed_taus (qhat path)."""
    cur = object.__new__(ContextCurator)
    cur.hetgraph = False
    cur.trace_level = True
    cur.trace_chunk_lines = 10
    cur.fixed_taus = [tau_q / SCALE] if tau_q is not None else []
    cur.dropped = set()
    cur.decided_msgs = set()
    cur.frozen = {}
    cur.dropped_ranges = {}
    cur.dropped_fp = set()
    cur.served_ranges = {}
    cur.served_fp = set()
    cur.insists = 0
    cur.decision_log, cur.evict_log, cur.doom_log = [], [], []
    cur.saved_tokens, cur.last_doom, cur._cum_in_tok, cur.runaway_factor = 0, 0.0, 0, 0.0
    cur._scores = lambda lc, task_text, cur_step, is_admission, recent="", mask_js=None: (
        [stub_score_q(c) / SCALE for c in lc],
        (stub_qhat_q(lc, mask_js or []) / SCALE))
    cur._explore_reset = lambda out, messages: out
    cur._neighbor_cost_median = lambda task_text: 0.0

    def _no_degrade(msg):
        raise RuntimeError(f"curator degraded during fixture gen: {msg}")
    cur._degrade = _no_degrade
    return cur


PY_FILE = "\n".join(
    [f"import mod{i}" for i in range(3)]
    + [""]
    + sum([[f"def handler_{k}(x):", f"    # handles case {k}", f"    y = x * {k} + 1",
            f"    return y - {k}", ""] for k in range(12)], [])
    + ["class App:", "    def run(self):", "        return handler_3(7)"]
)
GREP_OUT = "\n".join(
    [f"src/app.py:{5 * k + 4}:    y = x * {k} + 1" for k in range(12)]
    + ["src/util.py:20:def helper():",
       "src/:5:directory-prefix match with empty basename",   # Python-falsy file ""
       "binary file matches (x.bin)"]
)
TEST_OUT = "\n".join(
    ["<returncode>1</returncode>", "=== test session starts ==="]
    + [f"test_app.py::test_handler_{k} {'PASSED' if k % 3 else 'FAILED'}" for k in range(24)]
    + ["FAILED (failures=8)"]
)
SED_RANGE = "\n".join(PY_FILE.splitlines()[9:30])
ANALYSIS = "\n".join(f"Observation {i}: handler_{i % 12} looks {'fine' if i % 2 else 'wrong 🐍'}."
                     for i in range(50))


def asst(text, cmd=None, query=None, reasoning=None):
    m = {"role": "assistant", "content": text}
    act = {}
    if cmd:
        act["command"] = cmd
    if query:
        act["query"] = query
    m["extra"] = {"actions": [act] if act else []}
    if reasoning:
        m["reasoning_content"] = reasoning
    return m


MAIN_MESSAGES = [
    {"role": "system", "content": "You are a coding agent."},
    {"role": "user", "content": "Fix the failing handler tests in src/app.py."},
    asst("Let me read the app first.", cmd="cat src/app.py",
         reasoning="I should start from the file the tests point at."),
    {"role": "user", "content": PY_FILE},
    asst("Now find every handler call site.", cmd="grep -rn handler src/"),
    {"role": "user", "content": GREP_OUT},
    asst("Run the tests to see the failures.", cmd="python -m pytest -x"),
    {"role": "tool", "content": TEST_OUT},
    asst("I need lines 10-30 again.", cmd="sed -n '10,30p' src/app.py"),
    {"role": "user", "content": SED_RANGE},
    asst("Let me re-read the whole file once more.", cmd="cat src/app.py"),
    {"role": "user", "content": PY_FILE},
    asst(ANALYSIS, cmd="ls -la"),
    {"role": "user", "content": "total 8\n-rw-r--r-- 1 u u 812 app.py"},
]

# Step-0 births: assistant prose precedes the first user/tool message.
LEAD_ASST_MESSAGES = [
    asst("\n".join(f"Upfront planning thought {i} about the handler bug." for i in range(45)),
         cmd="cat src/app.py", reasoning="planning before the task arrives"),
    {"role": "user", "content": "Fix the failing handler tests in src/app.py."},
    asst("Reading the file.", cmd="cat src/app.py"),
    {"role": "user", "content": PY_FILE},
]

CONVERSATIONS = [
    {"name": "main", "messages": MAIN_MESSAGES, "prefixes": [4, 6, 8, 10, 12, 14]},
    {"name": "lead_asst", "messages": LEAD_ASST_MESSAGES, "prefixes": [2, 4]},
]

TAUS = [("tau15", 150_000), ("tau85", 850_000), ("tauq", None)]


def registries(cur) -> dict:
    # keys sorted for determinism; range LISTS keep commit order (stricter:
    # Rust must reproduce the exact append sequence, not just the set)
    ranges = lambda d: {k: list(v) for k, v in sorted(d.items())}  # noqa: E731
    return {
        "dropped": sorted(cur.dropped),
        "dropped_fp": sorted(cur.dropped_fp),
        "served_fp": sorted(cur.served_fp),
        "dropped_ranges": ranges(cur.dropped_ranges),
        "served_ranges": ranges(cur.served_ranges),
        "insists": cur.insists,
    }


def main():
    outdir = os.path.join(os.path.dirname(__file__), "fixtures")
    os.makedirs(outdir, exist_ok=True)
    for label, tau_q in TAUS:
        convs = []
        stats = []
        for conv in CONVERSATIONS:
            cur = make_curator(tau_q)
            calls = []
            for k in conv["prefixes"]:
                out = cur.curate(list(conv["messages"][:k]))
                calls.append({"prefix_len": k, "expected": json.loads(json.dumps(out))})
            convs.append({"name": conv["name"], "messages": conv["messages"],
                          "calls": calls, "registries": registries(cur)})
            digests = sum(1 for c in calls for m in c["expected"]
                          if isinstance(m.get("content"), str) and "lines (~" in m["content"])
            partials = sum(1 for c in calls for m in c["expected"]
                           if isinstance(m.get("content"), str)
                           and "[... ~" in m["content"] and "lines (~" not in m["content"])
            stats.append(f"{conv['name']}: dropped={len(cur.dropped)} insists={cur.insists} "
                         f"digests={digests} partials={partials}")
        fx = {"config": {"tau_q": tau_q, "read_lines": 10}, "conversations": convs}
        path = os.path.join(outdir, f"freeze_{label}.json")
        with open(path, "w") as f:
            json.dump(fx, f, ensure_ascii=False, indent=1)
        print(f"wrote {path}: " + " | ".join(stats))


if __name__ == "__main__":
    main()
