#!/usr/bin/env python3
"""Generate det NEEDED-LABEL parity fixtures — the /trim keep-set reference.

Provenance (the exact code the SWE-bench det arm measured):
  - dasein-trim @ 8da20f53642f135b387e5ba38c01d3aec56bc7a6
    deps/adaptive_context_optimizer/chunking.py: is_needed (with _planning_doc),
    targets_after, future_text, refetch_targets, window_rare, dup_stats,
    _covered — MATERIALIZED VERBATIM below (the vendored tree deliberately
    deleted the label machinery; do not re-import it from there).
  - adaptive-context-clean @ 58db530 labelers.py: parse_reads,
    parse_patch_hunks (not snapshotted in dasein-trim/deps; stable since).
  - dasein-trim deps/trace_graph.py build_trace_graph label path, STEPMODE
    off: needed_birth (global rare/sup) + the admission-readout loop
    (ages 0/1, alive-set rare/sup, len(alive)<6 -> None), det call-site
    constants: CHUNK_LINES=10, precise_only=True, W=0, other_min_len=8,
    continuity=2, sup_cut=0.8, refetch_mode="dead", fight_tgt=None,
    code_tokens=None (AC_DEDUP off), horizon=None (needed-EVER).
  - det keep-set = union of y >= 0.5 over admission readouts; rendered as
    "\\n".join(kept texts) (dasein-trim scripts/_det_via_established.py:37).

Chunkers/steps come from parsec_brain.vendored.chunking (already parity-pinned
to engine::chunking by the other fixtures); spans from vendored.attribution.

tests/parity_needed.rs replays each case through engine::needed::det_trim and
asserts chunk metadata, the keep indices, and the rendered body EXACTLY.

Run with the brain venv:
  packages/brain/.venv/bin/python parity/gen_needed.py [out.json]
"""
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "fixtures", "needed.json")
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "packages", "brain", "src"))
os.environ["AC_CHUNK_MODE"] = "fixed"  # the det trace path; read at import

from parsec_brain.vendored.attribution import spans  # noqa: E402
from parsec_brain.vendored.chunking import (  # noqa: E402
    accumulated_chunks,
    assistant_chunks_of,
    reasoning_chunks_of,
    steps_of,
)
from parsec_brain.vendored.labelers import parse_grep_candidate  # noqa: E402

CHUNK_LINES = 10

# --------------------------------------------------------------------------
# labelers.py @ adaptive-context-clean 58db530 — verbatim
# --------------------------------------------------------------------------
_PLUSFILE = re.compile(r"^\+\+\+ b/(.+)$", re.M)
_HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@", re.M)
_PATHTOK = re.compile(r"[\w./+-]*\.[A-Za-z]{1,5}$")


def parse_patch_hunks(patch):
    out = {}
    cur = None
    for line in patch.splitlines():
        mf = _PLUSFILE.match(line)
        if mf:
            cur = mf.group(1).strip().split("/")[-1]
            out.setdefault(cur, [])
            continue
        mh = _HUNK.match(line)
        if mh and cur:
            a = int(mh.group(1)); b = int(mh.group(2) or 1)
            out[cur].append((a, a + max(1, b) - 1))
    return {k: v for k, v in out.items() if v}


def parse_reads(actions):
    reads = {}
    for cmd in actions or []:
        toks = cmd.replace("'", " ").replace('"', " ").split()
        files = [t.split("/")[-1] for t in toks if _PATHTOK.search(t.split("/")[-1])]
        if not files:
            continue
        rng = None
        m = (re.search(r"\b(\d+),(\d+)p", cmd)
             or re.search(r"NR\s*>=\s*(\d+)\s*&&\s*NR\s*<=\s*(\d+)", cmd))
        if m:
            rng = (int(m.group(1)), int(m.group(2)))
        else:
            m = re.search(r"\bsed\s+-n\s+(\d+)p", cmd)
            if m:
                rng = (int(m.group(1)), int(m.group(1)))
            else:
                m = re.search(r"\bhead\s+(?:-n\s*)?(\d+)", cmd)
                if m:
                    rng = (1, int(m.group(1)))
        for f in files:
            reads.setdefault(f, []).append(rng)
    return reads


# --------------------------------------------------------------------------
# dasein-trim deps/adaptive_context_optimizer/chunking.py — verbatim
# --------------------------------------------------------------------------
def targets_after(steps, t, patch, horizon=None, precise_only=False):
    end = len(steps) if horizon is None else min(len(steps), t + 1 + horizon)
    later_cmds = [steps[s][0] for s in range(t + 1, end)]
    reads = parse_reads(later_cmds)
    tgt = {}
    for f, ranges in reads.items():
        for r in ranges:
            if r:
                tgt.setdefault(f, []).append(r)
            elif not precise_only:
                tgt.setdefault(f, []).append((-10**9, 10**9))
    for f, hunks in parse_patch_hunks(patch).items():
        tgt.setdefault(f, []).extend(hunks)
    for cmd in later_cmds:                       # dead in the reference; kept verbatim
        for ln in cmd.split():
            pc = parse_grep_candidate(ln)        # noqa: F841
    return tgt


def future_text(steps, t, patch, horizon=None):
    end = len(steps) if horizon is None else min(len(steps), t + 1 + horizon)
    return " ".join(steps[s][0] for s in range(t + 1, end))


def window_rare(chunks, max_df_frac=0.05, min_len=8):
    df = {}
    for c in chunks:
        for x in {x for x in spans(c.text) if len(x) >= min_len}:
            df[x] = df.get(x, 0) + 1
    cut = max(2.0, max_df_frac * len(chunks))
    return {x for x, n in df.items() if n <= cut}


def dup_stats(chunks, rows=None):
    sets = [{x for x in spans(c.text) if len(x) >= 5} for c in chunks]
    sup = [0.0] * len(chunks)
    dup_earlier = [0] * len(chunks)
    targets = range(len(chunks)) if rows is None else rows
    for i in targets:
        c = chunks[i]
        for j, d in enumerate(chunks):
            if i == j:
                continue
            ov = 0.0
            if c.file and d.file == c.file and c.lo is not None and d.lo is not None:
                inter = max(0, min(c.hi, d.hi) - max(c.lo, d.lo) + 1)
                ov = inter / max(1, c.hi - c.lo + 1)
            if ov < 1.0 and sets[i] and sets[j]:
                u = len(sets[i] | sets[j])
                if u:
                    ov = max(ov, len(sets[i] & sets[j]) / u)
            if d.step > c.step:
                sup[i] = max(sup[i], ov)
            elif d.step < c.step and ov >= 0.6:
                dup_earlier[i] += 1
    return sup, dup_earlier


def refetch_targets(steps, t):
    later = [steps[s][0] for s in range(t + 1, len(steps))]
    out = {}
    for f, ranges in parse_reads(later).items():
        for r in ranges:
            if r:
                out.setdefault(f, []).append(r)
    return out


_PLAN_TOKENS = {"plan", "plans", "planning"}


def _planning_doc(path) -> bool:
    b = os.path.basename(str(path)).lower()
    if "handoff" in b:
        return True
    return any(t in _PLAN_TOKENS for t in re.split(r"[^a-z]+", b))


def _covered(lo, hi, ranges, frac=0.8):
    if lo is None or hi is None or hi < lo:
        return False
    span = hi - lo + 1
    cov = 0
    for (a, b) in sorted(ranges):
        cov += max(0, min(hi, b) - max(lo, a) + 1)
    return cov >= frac * span


def is_needed(c, tgt, future="", W=15, distinct_cut=3.0, other_min_len=6,
              cur_step=None, continuity=2, rare=None, sup=0.0, sup_cut=0.8,
              refetch=None, patch_tgt=None, code_tokens=None,
              refetch_mode="dead", fight_tgt=None):
    if cur_step is not None and c.step >= cur_step:
        return True
    if c.kind == "reasoning":
        return cur_step is None or (cur_step - c.step) < continuity
    if c.file is not None and _planning_doc(c.file):
        return True
    if c.file is not None and fight_tgt:
        fr = fight_tgt.get(str(c.file).lower())
        if fr:
            if c.lo is None or any(not (c.hi < a or c.lo > b) for a, b in fr):
                return True
    if refetch_mode == "fight" and c.file is not None and refetch is not None and c.lo is not None:
        rr = refetch.get(c.file)
        if rr and _covered(c.lo, c.hi, rr):
            return True
    if sup >= sup_cut:
        return False
    if c.file is not None:
        if refetch_mode == "dead" and refetch is not None and c.lo is not None:
            rr = refetch.get(c.file)
            if rr and _covered(c.lo, c.hi, rr):
                pr = (patch_tgt or {}).get(c.file)
                if not (pr and any(not (c.hi < a - W or c.lo > b + W) for a, b in pr)):
                    return False
        rngs = tgt.get(c.file)
        if not rngs:
            return False
        return True if c.lo is None else any(not (c.hi < a - W or c.lo > b + W) for a, b in rngs)
    fut = spans(future)
    cs = spans(c.text)
    distinctive = {s for s in cs if len(s) >= other_min_len}
    if rare is not None:
        distinctive &= rare
    trig = distinctive & fut
    if not trig:
        return False
    if code_tokens is not None and trig <= code_tokens:
        return False
    return True


# --------------------------------------------------------------------------
# dasein-trim deps/trace_graph.py label path (STEPMODE off), label-relevant
# parts only — needed_birth + the admission readout union.
# --------------------------------------------------------------------------
def det_keep(messages, patch):
    steps = steps_of(messages)
    T = len(steps)
    if T < 6:
        return {"skip": "steps", "n": T}
    chunks = sorted(accumulated_chunks(steps, T - 1, read_lines=CHUNK_LINES)
                    + [c for c in assistant_chunks_of(messages) if c.step <= T - 1]
                    + [c for c in reasoning_chunks_of(messages) if c.step <= T - 1],
                    key=lambda c: c.step)
    n = len(chunks)
    if n < 6:
        return {"skip": "chunks", "n": n}
    o_rare = window_rare(chunks)
    o_sup, _ = dup_stats(chunks)
    o_ptgt = parse_patch_hunks(patch)
    _ever = {}

    def ctx(t):
        if t not in _ever:
            _ever[t] = (targets_after(steps, t, patch, None, precise_only=True),
                        future_text(steps, t, patch, None),
                        refetch_targets(steps, t))
        return _ever[t]

    def needed_birth(i):
        c = chunks[i]
        tgt, fut, rft = ctx(c.step)
        return is_needed(c, tgt, fut, W=0, other_min_len=8, cur_step=None, continuity=2,
                         rare=o_rare, sup=o_sup[i], refetch=rft, patch_tgt=o_ptgt,
                         code_tokens=None, refetch_mode="dead", fight_tgt=None)

    admitted = [c.kind == "reasoning" or needed_birth(i) for i, c in enumerate(chunks)]
    keep = [False] * n
    for age in (0, 1):
        for t in range(1 + age, T - 1):
            decided = [i for i, c in enumerate(chunks)
                       if c.step == t - age and c.kind != "reasoning"]
            if not decided:
                continue
            alive = [i for i, c in enumerate(chunks)
                     if c.step <= t and (c.step == t or (age == 1 and c.step == t - 1)
                                         or admitted[i])]
            if len(alive) < 6:
                continue
            al = [chunks[i] for i in alive]
            rare = window_rare(al)
            sup, _ = dup_stats(al, rows=[alive.index(i) for i in decided])
            supm = {alive[k]: sup[k] for k in range(len(alive))}
            tgt, fut, rft = ctx(t)
            for i in decided:
                if keep[i]:
                    continue
                if is_needed(chunks[i], tgt, fut, W=0, other_min_len=8, cur_step=None,
                             continuity=2, rare=rare, sup=supm.get(i, 0.0), refetch=rft,
                             patch_tgt=o_ptgt, code_tokens=None, refetch_mode="dead",
                             fight_tgt=None):
                    keep[i] = True
    kept_idx = [i for i in range(n) if keep[i]]
    return {
        "chunks": [{"text": c.text, "file": c.file, "lo": c.lo, "hi": c.hi,
                    "step": c.step, "kind": c.kind, "tokens": c.tokens} for c in chunks],
        "keep_idx": kept_idx,
        "rendered": "\n".join(chunks[i].text for i in kept_idx),
    }


# --------------------------------------------------------------------------
# cases
# --------------------------------------------------------------------------
def asst(cmd, content="", reasoning=None):
    m = {"role": "assistant", "content": content,
         "extra": {"actions": [{"command": cmd, "query": ""}]}}
    if reasoning is not None:
        m["reasoning_content"] = reasoning
    return m


def tool(text):
    return {"role": "tool", "content": text, "extra": {}}


def lines(prefix, lo, hi):
    return "\n".join(f"{prefix}_{i}" for i in range(lo, hi + 1))


def pad(msgs, k, tag):
    for i in range(k):
        msgs += [asst(f"echo {tag}_{i}"), tool("ok")]
    return msgs


# Step 0: a 60-line planning doc -> 6 always-admitted chunks. This keeps the
# alive pool >= 6 so admission readouts actually fire (the reference's
# len(alive) < 6 -> None guard) in these small synthetic sessions. Step-0
# chunks are never decided (t starts at 1+age), so these never enter the
# keep-set themselves.
PLAN = [asst("cat plan.md"), tool(lines("plan_item_number", 1, 60))]

# Positive control: ctrl.py read precisely at some step, lines 3-4 precisely
# re-read near the end (overlap target, only 2/10 lines -> not refetch-
# covered). Its first copy must ALWAYS be kept — proves readouts fired, so a
# case's negative expectation is meaningful.
CTRL_READ = [asst("sed -n '1,10p' ctrl.py"), tool(lines("ctrl_source_line", 1, 10))]
CTRL_TOUCH = [asst("sed -n '3,4p' ctrl.py"), tool("ctrl_source_line_3\nctrl_source_line_4")]


def case_refetch_kill():
    # q.py:1-10 read at step 1, then the SAME range precisely re-read later
    # (>=80% covered) with no patch -> both copies dead (the second has no
    # later use). The ctrl.py control chunk is kept.
    msgs = PLAN + [asst("sed -n '1,10p' q.py"), tool(lines("alpha_content_line", 1, 10))]
    msgs += CTRL_READ
    msgs = pad(msgs, 3, "fillone")
    msgs += [asst("sed -n '1,10p' q.py"), tool(lines("alpha_content_line", 1, 10))]
    msgs += CTRL_TOUCH
    msgs = pad(msgs, 2, "filltwo")
    return {"name": "refetch_kill", "messages": msgs, "patch": ""}


def case_patch_exemption():
    # Same shape, but a patch hunk overlaps the range -> the fix location
    # stays (both q.py copies gain a target via the patch union).
    c = case_refetch_kill()
    return {"name": "patch_exemption", "messages": c["messages"],
            "patch": "--- a/q.py\n+++ b/q.py\n@@ -4,3 +4,4 @@\n"}


def case_rare_token_trigger():
    # A test dump's rare token reappears in a later grep -> the dump is
    # needed; the pure-noise dump chunk stays dead.
    msgs = PLAN + [asst("python -m pytest -x"),
                   tool("FAILED tests/test_q.py\nTypeError: zorblefrazzle_mode unsupported\n"
                        + lines("noise_output_row", 1, 8))]
    msgs = pad(msgs, 3, "fillone")
    msgs += [asst("grep -rn zorblefrazzle_mode src/"), tool("src/q.py:7: zorblefrazzle_mode = 1")]
    msgs = pad(msgs, 2, "filltwo")
    return {"name": "rare_token_trigger", "messages": msgs, "patch": ""}


def case_sup_at_admission():
    # UNION SEMANTICS, documented: the age-0 readout runs when the chunk is
    # newborn, so no supersessor can be alive yet — sup never overrides an
    # age-0 keep. w.py:1-10 is re-read (1-9, 0.9-covered) later AND patch-
    # exempted from refetch-kill AND targeted (5p) -> KEPT despite the newer
    # 90%-overlapping copy. Supersession still shapes the label globally via
    # needed_birth/admission (the alive pool), which the unit tests pin.
    msgs = PLAN + [asst("sed -n '1,10p' w.py"), tool(lines("wcontent_line", 1, 10))]
    msgs = pad(msgs, 3, "fillone")
    msgs += [asst("sed -n '1,9p' w.py"), tool(lines("wcontent_line", 1, 9))]
    msgs += [asst("sed -n 5p w.py"), tool("wcontent_line_5")]
    msgs = pad(msgs, 2, "filltwo")
    return {"name": "sup_at_admission", "messages": msgs,
            "patch": "--- a/w.py\n+++ b/w.py\n@@ -2,3 +2,3 @@\n"}


def case_planning_doc():
    # A planning doc read at step >= 1 is decided and ALWAYS kept (no later
    # use needed); planet.py (token non-match) with no later use is dead.
    msgs = PLAN + [asst("cat sprint-plans.md"), tool(lines("sprint_goal_entry", 1, 10))]
    msgs += [asst("cat planet.py"), tool(lines("planet_body_row", 1, 10))]
    msgs = pad(msgs, 6, "fill")
    return {"name": "planning_doc", "messages": msgs, "patch": ""}


def case_whole_file_reread_ignored():
    # r.py read precisely at step 1; later only a WHOLE-FILE `cat r.py` —
    # precise_only drops it from targets, so the chunk is dead (and the
    # whole-file re-read is not a refetch either). ctrl.py control is kept.
    msgs = PLAN + [asst("sed -n '1,10p' r.py"), tool(lines("rcontent_line", 1, 10))]
    msgs += CTRL_READ
    msgs = pad(msgs, 3, "fillone")
    msgs += [asst("cat r.py"), tool(lines("rcontent_line", 1, 10))]
    msgs += CTRL_TOUCH
    msgs = pad(msgs, 2, "filltwo")
    return {"name": "whole_file_reread_ignored", "messages": msgs, "patch": ""}


def case_grep_line_targeting():
    # A single-line grep chunk that IS later precisely read is refetch-KILLED
    # by that very read (span 1 -> any overlap covers >= 0.8) — the label's
    # "the re-fetch supersedes the pointer" semantics — UNLESS the line
    # overlaps a patch hunk. g.py:14 has the patch exemption -> kept;
    # h.py:99 never touched -> dead.
    msgs = PLAN + [asst("grep -rn needle_token src/"),
                   tool("src/g.py:14: needle_token here\nsrc/h.py:99: needle_token there")]
    msgs = pad(msgs, 3, "fillone")
    msgs += [asst("sed -n '10,20p' g.py"), tool(lines("gcontent_line", 10, 20))]
    msgs = pad(msgs, 2, "filltwo")
    return {"name": "grep_line_targeting", "messages": msgs,
            "patch": "--- a/g.py\n+++ b/g.py\n@@ -14,2 +14,2 @@\n"}


def case_asst_and_reasoning():
    # An assistant message whose rare identifier recurs in a later command ->
    # its asst chunk is kept; the same step's reasoning chunk is admitted to
    # the alive pool but NEVER enters the keep-set.
    msgs = PLAN[:]
    # NB: the reasoning text must NOT repeat the identifier — a third in-window
    # occurrence would push its document frequency over the rarity cutoff.
    msgs += [asst("grep -n quuxplex_handler src/",
                  content="The bug is in quuxplex_handler; check its callers next.",
                  reasoning="considering the dispatch paths before editing"),
             tool("src/m.py:3: def quuxplex_handler():")]
    msgs = pad(msgs, 3, "fillone")
    msgs += [asst("sed -n '1,5p' m.py && echo quuxplex_handler"), tool(lines("mcontent_line", 1, 5))]
    msgs = pad(msgs, 2, "filltwo")
    return {"name": "asst_and_reasoning", "messages": msgs, "patch": ""}


def case_too_short():
    msgs = []
    for i in range(5):
        msgs += [asst(f"cat f{i}.py"), tool(lines("short_body_line", 1, 3))]
    return {"name": "too_short", "messages": msgs, "patch": ""}


def case_mixed_session():
    # A denser 16-step session composing every rule.
    msgs = PLAN[:]
    msgs += [asst("grep -rn frobnicate_impl src/"),
             tool("src/core.py:41: def frobnicate_impl(x):\nsrc/util.py:9: from core import frobnicate_impl")]
    msgs += [asst("sed -n '35,60p' core.py"), tool(lines("core_source_line", 35, 60))]
    msgs += [asst("python -m pytest tests/ -x"),
             tool("<returncode>1</returncode>\n" + lines("pytest_noise_row", 1, 12)
                  + "\nAssertionError: gribblewock_state mismatch")]
    msgs += [asst("grep -rn gribblewock_state src/ tests/"),
             tool("tests/test_core.py:77: assert gribblewock_state == 3")]
    msgs += [asst("sed -n '70,85p' test_core.py"), tool(lines("testcore_src_line", 70, 85))]
    msgs += [asst("sed -n '35,44p' core.py"), tool(lines("core_source_line", 35, 44))]
    msgs = pad(msgs, 4, "tailfill")
    msgs += [asst("sed -n '40,42p' core.py"), tool(lines("core_source_line", 40, 42))]
    msgs = pad(msgs, 3, "postfill")
    patch = "--- a/core.py\n+++ b/core.py\n@@ -41,2 +41,3 @@\n"
    return {"name": "mixed_session", "messages": msgs, "patch": patch}


CASES = [
    case_refetch_kill(),
    case_patch_exemption(),
    case_rare_token_trigger(),
    case_sup_at_admission(),
    case_planning_doc(),
    case_whole_file_reread_ignored(),
    case_grep_line_targeting(),
    case_asst_and_reasoning(),
    case_too_short(),
    case_mixed_session(),
]


def main():
    out = []
    for case in CASES:
        exp = det_keep(case["messages"], case["patch"])
        row = {"name": case["name"], "messages": case["messages"],
               "patch": case["patch"], "expected": exp}
        out.append(row)
        if "skip" in exp:
            print(f"{case['name']}: skip={exp['skip']} n={exp['n']}")
        else:
            print(f"{case['name']}: {len(exp['keep_idx'])}/{len(exp['chunks'])} kept")
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(out, f, indent=1)
    print("wrote", OUT)


if __name__ == "__main__":
    main()
