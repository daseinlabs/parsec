"""Context-window chunking + the 'needed-downstream' label — Stage 1 of the context curator.

We intercept the FULL accumulated context and slice it into uniform chunks (the user's 'just chunk
it'), so reads, greps and dumps are all scored the same way. A chunk is NEEDED at step t if the
agent later (steps > t) operates on its location — reads or edits that file near those lines. If the
curator drops a needed chunk, the agent must re-fetch it (the redundant re-reading the paper pins
failures on); if it keeps the un-needed ones, it bloats the window (cost + context rot). So a good
curator KEEPS the needed and SHEDS the rest — that is the coverage/compression trade-off we measure.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field

from .labelers import parse_grep_candidate

_SEARCH = re.compile(r"\b(grep|rg|egrep|fgrep|ag|ack|find|git grep)\b")
_READ = re.compile(r"\b(cat|sed|head|tail|less|more|nl|awk|view|open)\b")
_FILEARG = re.compile(r"[\w./+-]*\.[A-Za-z]{1,5}")

# read-chunking backend: "fixed" (G=read_lines line windows, default) | "cst" (tree-sitter semantic
# atoms, Arm-2). Read once at import; the launcher sets AC_CHUNK_MODE before python starts (same
# pattern as trace_graph.CHUNK_LINES). Serving must re-chunk with the SAME mode -> persisted on ckpt.
_CHUNK_MODE = os.environ.get("AC_CHUNK_MODE", "fixed").lower()


@dataclass
class Chunk:
    text: str
    file: str | None            # basename, or None for non-file content (test output, reasoning)
    lo: int | None              # line range covered (for file chunks)
    hi: int | None
    step: int                   # which step produced it (age)
    kind: str                   # 'read' | 'grep' | 'other' | 'reasoning' | 'asst' (agent's own text)
    tokens: int = 0             # token weight; if <=0, derived from len(text)
    emb: list[float] | None = None
    evict: str = "content"      # eviction primitive: 'content' (truncate msg body) | 'provider'
    #                             ('provider' -> strip the message's reasoning/provider fields)
    cmd: str = ""               # the raw ACTION text that produced this observation (any domain)
    rc: int | None = None       # parsed exit status when the harness exposes one (unused by
    #                             features — status semantics ride in via the head embedding)
    head: str = ""              # observation head (~240 chars): where EVERY harness puts its
    #                             status line — exit codes, HTTP status, error banners, 'no results'
    struct_class: str = ""      # CST node class for code-read atoms (func|class|import|body); "" when
    #                             fixed-window chunked or non-code. Tree-sitter signal source (Arm-2).

    def __post_init__(self):
        if self.tokens <= 0:                              # reasoning chunks pass an explicit weight
            self.tokens = max(1, len(self.text) // 4)


_RC = re.compile(r"<returncode>\s*(-?\d+)\s*</returncode>|\breturncode[:=]\s*(-?\d+)", re.I)


def _obs_rc(obs: str) -> int | None:
    m = _RC.search(obs[:400])
    return int(m.group(1) or m.group(2)) if m else None


# provider/reasoning fields that get re-fed every call and are the eviction target for 'provider' chunks
_REASON_FIELDS = ("provider_specific_fields", "thinking_blocks", "reasoning_content")


def blob_tokens(m: dict) -> int:
    """Token weight of an assistant message's re-fed reasoning payload — the Gemini-3
    thought_signature / thinking blobs on tool_calls AND at the top level. This is what stripping
    removes, so it is the reasoning chunk's true cost (independent of the model: 0 if absent)."""
    import json
    tot = 0
    for tc in (m.get("tool_calls") or []):
        psf = tc.get("provider_specific_fields")
        if psf:
            tot += len(json.dumps(psf)) // 4
    for k in _REASON_FIELDS:
        v = m.get(k)
        if v:
            tot += len(v if isinstance(v, str) else json.dumps(v)) // 4
    return tot


def reasoning_text(m: dict) -> str:
    """Embed text for the THINKING object — ONE of two separate reasoning objects a step can
    carry. The other is the visible assistant ``content`` (see visible_text), kept SEPARATE on
    purpose: the two differ in curatability (thinking is out-of-context / not cut surface;
    visible text is re-fed / curatable), so the GNN must score them as distinct chunks, not a
    merged blob. This returns the thinking text only (Anthropic returns a summary of Sonnet-4's
    raw CoT), falling back to the action when the step did no thinking."""
    rc = m.get("reasoning_content")
    if isinstance(rc, str) and rc.strip():
        return rc[:2000]
    acts = [a.get("command") or a.get("query") or "" for a in m.get("extra", {}).get("actions", [])]
    t = " ; ".join(a for a in acts if a)
    return (t[:2000] or "reasoning")




def reasoning_chunk(m: dict, step: int) -> "Chunk | None":
    """One scorable chunk for an assistant step's re-fed reasoning payload. None when the step
    carries no reasoning blob — which, for a frontier reasoning model (Sonnet 4.5/4.6), means we
    FAILED TO CAPTURE it (extended thinking not requested on the call), NOT that the lever is
    absent. Capture is fixed at the harness seam (make_model enables thinking; the CC adapter
    normalizes ThinkingBlocks) so this returns a real chunk; the None-guard stays as defense."""
    tok = blob_tokens(m)
    if tok <= 0:
        return None
    return Chunk(reasoning_text(m), None, None, None, step, "reasoning", tokens=tok, evict="provider")


def steps_of(messages) -> list[tuple[str, str]]:
    """Ordered (command, observation_text) per agent step."""
    out, pending = [], None
    for m in messages:
        role = m.get("role")
        if role == "assistant":
            cmds = [a.get("command") or a.get("query") or ""
                    for a in m.get("extra", {}).get("actions", [])]
            pending = " ; ".join(cmds)
        elif role in ("user", "tool") and pending is not None:
            c = m.get("content", "")
            txt = c if isinstance(c, str) else " ".join(p.get("text", "") for p in c if isinstance(p, dict))
            out.append((pending, txt))
            pending = None
    return out


def reasoning_chunks_of(messages) -> list["Chunk"]:
    """Reasoning chunk per agent step, aligned to steps_of()'s step index (s-th assistant→obs pair).
    Empty ONLY when the trace failed to capture reasoning (thinking not requested at the call seam) —
    not a model property; a captured Sonnet trace yields one reasoning chunk per thinking step."""
    out, pending, step = [], None, 0
    for m in messages:
        role = m.get("role")
        if role == "assistant":
            pending = m
        elif role in ("user", "tool") and pending is not None:
            rc = reasoning_chunk(pending, step)
            if rc is not None:
                out.append(rc)
            pending, step = None, step + 1
    return out


def _first_file(cmd: str) -> str | None:
    for tok in cmd.replace("'", " ").replace('"', " ").split():
        m = _FILEARG.search(tok.split("/")[-1])
        if m:
            return tok.split("/")[-1]
    return None


def _read_atom_lines(cmd: str, obs: str):
    """(orig_line_no, text) pairs for a CODE-READ observation, fully-blank lines DROPPED, comments
    KEPT, each kept line carrying its TRUE original-file line number (base + raw_index, base from
    `sed -n 'a,bp'`/head, else 1). This is granularity_probe.read_observation_lines' exact rule —
    reused here so finer read atoms keep correct lo/hi (dropping blanks removes token mass but never
    shifts a kept line's coordinate, so overlap with patch hunks / sed ranges stays exact)."""
    f = _first_file(cmd)
    m = re.search(r"\b(\d+),(\d+)p", cmd) or re.search(r"\bsed\s+-n\s+(\d+)p", cmd)
    base = int(m.group(1)) if m else 1
    out = []
    for i, ln in enumerate(obs.splitlines()):
        if ln.strip() == "":           # STRIP fully-blank lines only; comment lines are kept
            continue
        out.append((base + i, ln))     # original file line number preserved
    return f, out


def chunk_observation(cmd: str, obs: str, step: int, win: int = 40,
                      read_lines: int | None = None) -> list[Chunk]:
    """Slice one observation into chunks with file/line metadata where possible.

    read_lines (default None -> legacy win windowing, byte-identical to the per-decision path): when
    set (the trace path's G=10), CODE-READ observations are re-split into read_lines-line atoms with
    TRUE original-file line coords (blanks dropped, comments kept; granularity_probe's rule). Only the
    READ branch is re-granularised — grep/other observations keep `win` (granularity is a read-atom
    question; the probe only re-windows code reads)."""
    if read_lines is not None and not _SEARCH.search(cmd) and _READ.search(cmd):
        f, coord_lines = _read_atom_lines(cmd, obs)
        out: list[Chunk] = []
        # (cst branch deleted in the vendored copy: AC_CHUNK_MODE pinned "fixed" — Arm-2 only)
        for k in range(0, len(coord_lines), read_lines):   # fixed read_lines-line windows (default)
            seg = coord_lines[k:k + read_lines]
            text = "\n".join(t for _, t in seg)
            out.append(Chunk(text, f, seg[0][0], seg[-1][0], step, "read"))
        if not out:                    # no surviving lines -> fall back to whole-obs read chunk
            out = [Chunk(obs, f, None, None, step, "read")]
    else:
        out = _chunk_observation(cmd, obs, step, win)
    rc = _obs_rc(obs)
    for c in out:                                  # action + obs-head ride on every chunk and are
        c.cmd, c.rc, c.head = cmd[:300], rc, obs[:240]   # EMBEDDED as model input (universal)
    return out


def _chunk_observation(cmd: str, obs: str, step: int, win: int = 40) -> list[Chunk]:
    # FULL COVERAGE INVARIANT: every character of the tool output lands in exactly one chunk, in
    # original order. The old truncation caps ([:300]/[:1500]/[:2000]) left ~17% of tool-output
    # bytes with NO chunk address — unscoreable, undecidable, and silently deleted (uncounted) on
    # partial rerenders. A 30k dump now gets its ~20 decisions, not one decision off a 1.5k peek.
    # (Feature-side truncation still happens at EMBED time — c.text[:2000] — which is fine: that
    # caps what the model reads, never what the curator can address.)
    lines = obs.splitlines()
    if _SEARCH.search(cmd):                       # grep/find: each match line is a tiny chunk
        out: list[Chunk] = []
        run: list[str] = []

        def _flush():
            for k in range(0, len(run), win):
                seg = "\n".join(run[k:k + win])
                if seg.strip():
                    out.append(Chunk(seg, None, None, None, step, "other"))
            run.clear()
        for ln in lines:
            pc = parse_grep_candidate(ln)
            if pc:
                _flush()
                f, n = pc
                out.append(Chunk(ln, f, n, n, step, "grep"))
            else:                                  # non-candidate lines (headers, context, noise)
                run.append(ln)                     # become addressable chunks too
        _flush()
        return out or [Chunk(obs, None, None, None, step, "other")]
    if _READ.search(cmd):                         # file read: window the file by `win` lines
        f = _first_file(cmd)
        # detect a starting line from `sed -n 'a,bp'`
        m = re.search(r"\b(\d+),(\d+)p", cmd) or re.search(r"\bsed\s+-n\s+(\d+)p", cmd)
        base = int(m.group(1)) if m else 1
        out = []
        for i in range(0, len(lines), win):
            seg = lines[i:i + win]
            out.append(Chunk("\n".join(seg), f, base + i, base + i + len(seg) - 1, step, "read"))
        return out or [Chunk(obs, f, None, None, step, "read")]
    # everything else (test output, python, ls, git): token-window chunks, no file
    out = []
    for i in range(0, max(1, len(lines)), win):
        seg = "\n".join(lines[i:i + win])
        if seg.strip():
            out.append(Chunk(seg, None, None, None, step, "other"))
    return out or [Chunk(obs, None, None, None, step, "other")]


def chunk_assistant(txt: str, step: int, win: int = 40) -> list[Chunk]:
    """The agent's OWN message text (plans, analysis — NOT tool calls, NOT reasoning payloads),
    windowed with the same full-coverage invariant. Same birth-time admission economics as tool
    outputs: it is new bytes on the call after emission. kind='asst' is deliberately NOT in the
    struct one-hots — all-zero kind flags + zero action embedding is its unique signature."""
    lines = txt.splitlines()
    out = []
    for i in range(0, max(1, len(lines)), win):
        seg = "\n".join(lines[i:i + win])
        if seg.strip():
            out.append(Chunk(seg, None, None, None, step, "asst"))
    return out


def assistant_chunks_of(messages: list[dict]) -> list[Chunk]:
    """Per-step assistant text chunks, aligned to steps_of()'s pairing (s-th assistant→obs pair).
    Skips messages whose content isn't plain text (renderer rewrites content as a string)."""
    out, pending, step = [], None, 0
    for m in messages:
        role = m.get("role")
        if role == "assistant":
            pending = m
        elif role in ("user", "tool") and pending is not None:
            c = pending.get("content")
            ok = isinstance(c, str) or (isinstance(c, list) and all(
                isinstance(p, dict) and p.get("type", "text") == "text" for p in c))
            if ok:
                t = c if isinstance(c, str) else " ".join(p.get("text", "") for p in c)
                if t.strip():
                    out.extend(chunk_assistant(t, step))
            pending, step = None, step + 1
    return out


def accumulated_chunks(steps: list[tuple[str, str]], upto: int,
                       read_lines: int | None = None) -> list[Chunk]:
    """All chunks the agent has SEEN through step `upto` (the window to curate).
    read_lines (default None) threads the read-atom granularity to chunk_observation: None keeps the
    legacy win=40 windowing (per-decision/w326 path byte-identical); the trace path passes G (=10)."""
    out = []
    for s in range(min(upto + 1, len(steps))):     # clamp: step-0 tool-spec has 0 steps -> no obs
        out.extend(chunk_observation(steps[s][0], steps[s][1], s, read_lines=read_lines))
    return out










def dup_feats_fast(chunks: list["Chunk"], rows: list[int] | None = None) -> tuple[list[float], list[int]]:
    """O(n) HASH version of dup_stats for the SERVING FEATURE (struct_features runs every curate; the
    fuzzy all-pairs span-Jaccard profiled at ~160s of a 137s serve run). A duplicate is the SAME content
    -> hash it. Two signals: (1) EXACT content dup via hash bucket (newer identical copy supersedes ->
    ov=1.0; earlier identical -> dup_earlier++); (2) SAME-FILE line-range overlap (newer read covering
    this span supersedes it). No cross-file fuzzy Jaccard. dup_stats stays the thorough offline LABEL."""
    from collections import defaultdict
    n = len(chunks)
    sup = [0.0] * n
    dup_earlier = [0] * n
    by_hash: dict = defaultdict(list)
    by_file: dict = defaultdict(list)
    for i, c in enumerate(chunks):
        by_hash[hash(c.text or "")].append(i)
        if c.file and c.lo is not None:
            by_file[c.file].append(i)
    targets = range(n) if rows is None else rows
    for i in targets:
        c = chunks[i]
        for j in by_hash[hash(c.text or "")]:             # exact content duplicate (verify == per collision)
            if j == i or chunks[j].text != c.text:
                continue
            if chunks[j].step > c.step:
                sup[i] = 1.0
            elif chunks[j].step < c.step:
                dup_earlier[i] += 1
        if c.file and c.lo is not None:                   # same-file partial re-read (line-range overlap)
            for j in by_file[c.file]:
                if j == i:
                    continue
                d = chunks[j]
                if d.lo is None:
                    continue
                ov = max(0, min(c.hi, d.hi) - max(c.lo, d.lo) + 1) / max(1, c.hi - c.lo + 1)
                if d.step > c.step:
                    sup[i] = max(sup[i], ov)
                elif d.step < c.step and ov >= 0.6:
                    dup_earlier[i] += 1
    return sup, dup_earlier






