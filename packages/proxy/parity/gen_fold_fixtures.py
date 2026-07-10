#!/usr/bin/env python3
"""Generate fold-back parity fixtures from service/anthropic_shapes.py.

Covers: system flattening, content text view, rewrite_first_text (incl.
parallel tool_result consolidation and is_error guards), cache_control
stripping, the raw-inbound fingerprint, and apply_curation with and without
the served_folds freeze — including the frozen-replay and fingerprint-refold
paths across simulated successive calls.
"""
import json
import os
import sys

ACC = os.environ.get("ACC_ROOT") or os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "adaptive-context-clean"))
sys.path.insert(0, ACC)

from service import anthropic_shapes as S  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(__file__), "fixtures", "fold.json")

TR = {"type": "tool_result", "tool_use_id": "tu_1", "content": "long observation text " * 20}
TR2 = {"type": "tool_result", "tool_use_id": "tu_2",
       "content": [{"type": "text", "text": "second result"}, {"type": "image", "source": "s"}]}
TRE = {"type": "tool_result", "tool_use_id": "tu_3", "is_error": True, "content": "traceback..."}

SYSTEMS = [
    ("sys_str", "You are an agent. " * 10),
    ("sys_blocks", [{"type": "text", "text": "block one"},
                    {"type": "text", "text": "block twö", "cache_control": {"type": "ephemeral"}},
                    {"type": "image", "source": "x"}]),
    ("sys_none", None),
]

CONTENTS = [
    ("plain_str", "hello wörld"),
    ("text_blocks", [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]),
    ("tool_result_str", [TR]),
    ("parallel_results", [TR, TR2, TRE]),
    ("error_result", [TRE]),
    ("mixed", [{"type": "text", "text": "lead"}, {"type": "tool_use", "id": "tu_9", "name": "bash",
                "input": {"command": "ls"}}, TR2]),
    ("no_text_block", [{"type": "tool_use", "id": "tu_8", "name": "bash", "input": {}}]),
    ("non_dict_parts", [{"type": "text", "text": "x"}, "bare"]),
    ("empty_list", []),
    ("none", None),
    ("cache_ctl", [{"type": "text", "text": "t", "cache_control": {"type": "ephemeral"}},
                   {"type": "tool_result", "tool_use_id": "t", "cache_control": {"type": "ephemeral"},
                    "content": [{"type": "text", "text": "inner", "cache_control": {"type": "ephemeral"}}]}]),
    # fingerprint bytes depend on CPython number repr — pin the float families
    ("numbers", [{"type": "tool_use", "id": "tu_n", "name": "calc",
                  "input": {"a": 1e-7, "b": 0.00001, "c": 1e16, "d": -0.0, "e": 1.1534175185142759,
                            "f": 123456789, "g": [1.0, 0.5, 1e15, -2.5e-7], "h": 0.0001}}]),
]

NEW_TEXTS = ["", "trimmed", "unicode 🐍 trim", "   "]


def body(system, msgs):
    b = {"model": "claude-x", "max_tokens": 10, "messages": msgs}
    if system is not None:
        b["system"] = system
    return b


def curated(texts):
    return [{"role": "x", "content": t} for t in texts]


def main():
    fx = {"system_to_text": [], "content_text": [], "rewrite_first_text": [],
          "strip_cache_control": [], "fingerprint": [], "apply_curation": []}

    for name, s in SYSTEMS:
        fx["system_to_text"].append({"name": name, "system": s, "expected": S._system_to_text(s)})
    for name, c in CONTENTS:
        fx["content_text"].append({"name": name, "content": c, "expected": S._content_text(c)})
        fx["strip_cache_control"].append({"name": name, "content": c,
                                          "expected": S._strip_cache_control(c)})
        fx["fingerprint"].append({"name": name, "content": c,
                                  "expected": S._orig_fingerprint(c)})
        for t in NEW_TEXTS:
            fx["rewrite_first_text"].append({"name": f"{name}|{t[:8]}", "content": c, "new_text": t,
                                             "expected": S._rewrite_first_text(c, t)})

    # apply_curation: stateless (no folds) single-shot cases
    msgs1 = [
        {"role": "user", "content": "task description"},
        {"role": "assistant", "content": [{"type": "text", "text": "plan"},
                                          {"type": "tool_use", "id": "tu_1", "name": "bash",
                                           "input": {"command": "cat a.py"}}]},
        {"role": "user", "content": [TR, TR2]},
    ]
    b1 = body(SYSTEMS[0][1], msgs1)
    cur_texts = ["SYS TRIMMED", "task description", "plan", "curated obs"]
    fx["apply_curation"].append({
        "name": "stateless_trim", "body": b1, "curated": curated(cur_texts), "state": None,
        "expected": S.apply_curation(b1, curated(cur_texts), None), "expected_state": None,
    })
    # curated == original -> verbatim
    same_texts = [S._system_to_text(b1["system"])] + [S._content_text(m["content"]) for m in msgs1]
    fx["apply_curation"].append({
        "name": "stateless_unchanged", "body": b1, "curated": curated(same_texts), "state": None,
        "expected": S.apply_curation(b1, curated(same_texts), None), "expected_state": None,
    })
    # short curated list (cur None fallthrough)
    fx["apply_curation"].append({
        "name": "stateless_short_curated", "body": b1, "curated": curated(cur_texts[:2]), "state": None,
        "expected": S.apply_curation(b1, curated(cur_texts[:2]), None), "expected_state": None,
    })
    # no system
    b2 = body(None, msgs1)
    cur2 = curated(["task description", "plan trimmed", "obs trimmed"])
    fx["apply_curation"].append({
        "name": "stateless_no_system", "body": b2, "curated": cur2, "state": None,
        "expected": S.apply_curation(b2, cur2, None), "expected_state": None,
    })

    # stateful: call 1 freezes, call 2 replays frozen bytes ignoring new curator opinion,
    # call 3 sees a client-changed turn (new fingerprint -> refold)
    state = {}
    r1 = S.apply_curation(b1, curated(cur_texts), state)
    fx["apply_curation"].append({
        "name": "freeze_call1", "body": b1, "curated": curated(cur_texts), "state": {},
        "expected": r1, "expected_state": state,
    })
    state2 = json.loads(json.dumps(state))
    cur_changed = curated(["SYS DIFFERENT", "task NEW", "plan NEW", "obs NEW"])
    msgs2 = msgs1 + [{"role": "assistant", "content": "next step"}]
    b3 = body(SYSTEMS[0][1], msgs2)
    r2 = S.apply_curation(b3, curated(["SYS DIFFERENT", "task NEW", "plan NEW", "obs NEW", "next step"]), state2)
    fx["apply_curation"].append({
        "name": "freeze_call2_replays", "body": b3,
        "curated": curated(["SYS DIFFERENT", "task NEW", "plan NEW", "obs NEW", "next step"]),
        "state": json.loads(json.dumps(state)), "expected": r2, "expected_state": state2,
    })
    state3 = json.loads(json.dumps(state2))
    msgs3 = [dict(msgs1[0], content="task description EDITED")] + msgs2[1:]
    b4 = body(SYSTEMS[0][1], msgs3)
    r3 = S.apply_curation(b4, cur_changed + [curated(["next step"])[0]], state3)
    fx["apply_curation"].append({
        "name": "freeze_call3_refold_changed_turn", "body": b4,
        "curated": cur_changed + [curated(["next step"])[0]],
        "state": json.loads(json.dumps(state2)), "expected": r3, "expected_state": state3,
    })

    # place_cache_breakpoint: sequential calls through one state dict.
    # Inputs carry CC-style stale cache_control (must be cleared + re-placed)
    # and a bare-string content (the promotion path). Deep-copy per call —
    # the reference mutates in place.
    import copy
    cc = {"cache_control": {"type": "ephemeral"}}
    A = {"role": "user", "content": [{"type": "text", "text": "the task", **cc}]}
    B = {"role": "assistant", "content": "plain string reply"}
    C = {"role": "user", "content": [TR, {"type": "text", "text": "obs tail", **cc}]}
    D = {"role": "assistant", "content": [{"type": "text", "text": "next"}]}
    E = {"role": "user", "content": [{"type": "text", "text": "[CONTEXT-REUSE] directive"}]}
    SYS = [{"type": "text", "text": "system prompt", **cc}]
    calls = [
        {"messages": [A, B], "directive": False},
        {"messages": [A, B, C, D], "directive": False},
        {"messages": [A, B, C, D, E], "directive": True},
        {"messages": [dict(A, content="EDITED task"), B, C, D], "directive": False},
    ]
    state: dict = {}
    bp_cases = []
    for c in calls:
        inp = {"model": "claude-x", "system": copy.deepcopy(SYS),
               "messages": copy.deepcopy(c["messages"])}
        req = copy.deepcopy(inp)
        out = S.place_cache_breakpoint(req, directive_appended=c["directive"], state=state)
        bp_cases.append({"input": inp, "directive": c["directive"],
                         "expected": json.loads(json.dumps(out)),
                         "expected_fps": list(state["last_msg_fps"])})
    fx["place_cache_breakpoint"] = bp_cases

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(fx, f, ensure_ascii=False, indent=1)
    print(f"wrote {OUT}: " + ", ".join(f"{k}={len(v)}" for k, v in fx.items()))


if __name__ == "__main__":
    main()
