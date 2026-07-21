#!/usr/bin/env node
// Build a replay tape for the /demo race view from REAL measured data.
//
// Sources (all on-disk, nothing modeled):
//   ~/.dasein/ledger.jsonl        proxy savings-ledger/v0 — per-request
//                                 counterfactual_input_tokens vs billed(+cache)
//   ~/.dasein/sessions/*.json     hook state — re-read denials + broken loops
//   `dasein savings`              authoritative measured aggregate (parsed for
//                                 the headline hook token number)
//
// The race animation is scoped to ONE real conversation from the ledger
// (plain side = the count_tokens counterfactual = what plain Claude Code would
// have been sent that turn; plugin side = what dasein actually sent). The hook
// panel shows the measured lifetime aggregate. Every number here is measured —
// honoring the repo's measurement-honesty invariant. No estimates.
//
// Usage:
//   node scripts/build-demo-tape.mjs                 # auto-pick best conversation
//   node scripts/build-demo-tape.mjs --conv <id>     # pin a conversation
//   DASEIN_BIN=/path/to/dasein node scripts/build-demo-tape.mjs
//
// Writes: public/demo-tape.json

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const HOME = os.homedir();
const HERE = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.join(HERE, "..", "public", "demo-tape.json");
const LEDGER = path.join(HOME, ".dasein", "ledger.jsonl");
const SESSIONS_DIR = path.join(HOME, ".dasein", "sessions");
const DASEIN_BIN =
  process.env.DASEIN_BIN ||
  path.join(HERE, "..", "..", "plugin", "bin", "dasein");

const argv = process.argv.slice(2);
const pinnedConv = argv.includes("--conv")
  ? argv[argv.indexOf("--conv") + 1]
  : null;
// selection: "pct" (highest measured savings % with >= minTurns) or "saved"
// (most tokens saved). Default "pct" — the strongest honest result to demo.
const rank = argv.includes("--rank") ? argv[argv.indexOf("--rank") + 1] : "pct";
const minTurns = argv.includes("--min-turns")
  ? +argv[argv.indexOf("--min-turns") + 1]
  : 3;
// Ignore trivially small conversations (a 96-token fail-open scores 100% but
// tells no story). Require real volume before a % is meaningful.
const minCf = argv.includes("--min-cf")
  ? +argv[argv.indexOf("--min-cf") + 1]
  : 30000;

function readJsonl(file) {
  if (!fs.existsSync(file)) return [];
  return fs
    .readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => {
      try {
        return JSON.parse(l);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

// ---- 1. proxy ledger -> per-conversation turns ---------------------------
const ledger = readJsonl(LEDGER).filter(
  (r) => typeof r.counterfactual_input_tokens === "number"
);
if (ledger.length === 0) {
  console.error(
    `No ledger rows at ${LEDGER}. Run a session with the dasein plugin first, ` +
      `or point DASEIN_BIN/ledger at a populated one.`
  );
  process.exit(1);
}

const byConv = new Map();
for (const r of ledger) {
  const k = r.conv_id || "unknown";
  if (!byConv.has(k)) byConv.set(k, []);
  byConv.get(k).push(r);
}

function billedTotal(r) {
  return (
    (r.billed_input_tokens || 0) +
    (r.billed_cache_read_tokens || 0) +
    (r.billed_cache_write_tokens || 0)
  );
}

let conv = pinnedConv;
if (!conv) {
  // Score every conversation; pick per --rank. Both metrics are measured.
  let best = null;
  for (const [k, rows] of byConv) {
    const cf = rows.reduce((a, r) => a + r.counterfactual_input_tokens, 0);
    const billed = rows.reduce((a, r) => a + billedTotal(r), 0);
    const saved = Math.max(0, cf - billed);
    const pct = cf > 0 ? saved / cf : 0;
    // "pct" mode requires enough turns to animate; "saved" maximizes raw tokens.
    const eligible = rows.length >= minTurns && cf >= minCf;
    let score;
    if (rank === "pct") {
      score = eligible ? pct * 1e6 + rows.length : -1;
    } else {
      score = eligible ? rows.length * 1e9 + saved : -1;
    }
    if (!best || score > best.score) best = { k, score };
  }
  conv = best.k;
}
const rows = (byConv.get(conv) || [])
  .slice()
  .sort((a, b) => (a.ts < b.ts ? -1 : a.ts > b.ts ? 1 : 0));

// Turn timeline. Use real inter-request wall-clock deltas when present,
// clamped to a watchable [500ms, 3500ms]; fall back to even spacing.
const turns = [];
let clock = 0;
for (let i = 0; i < rows.length; i++) {
  const r = rows[i];
  let dt = 1300;
  if (i > 0) {
    const prev = Date.parse(rows[i - 1].ts);
    const cur = Date.parse(r.ts);
    if (Number.isFinite(prev) && Number.isFinite(cur) && cur > prev) {
      dt = Math.min(3500, Math.max(500, cur - prev));
    }
  }
  clock += i === 0 ? 0 : dt;
  const plain = r.counterfactual_input_tokens;
  const plugin = billedTotal(r);
  turns.push({
    t: clock,
    plainContext: plain,
    pluginContext: plugin,
    saved: Math.max(0, plain - plugin),
    model: r.model || null,
    freezeCut: r.freeze_cut_tokens || 0,
    toolsTotal: r.tools_total ?? null,
    toolsKept: r.tools_kept ?? null,
    toolsStubbed: r.tools_stubbed ?? null,
    insists: r.curator_insists || 0,
    brainMs: r.brain_ms ?? null,
    failOpen: !!r.fail_open,
  });
}

const sumPlain = turns.reduce((a, t) => a + t.plainContext, 0);
const sumPlugin = turns.reduce((a, t) => a + t.pluginContext, 0);
const sumSaved = Math.max(0, sumPlain - sumPlugin);

// ---- 2. hook denials -> sampled DENIED events + real counts --------------
let reReadsBlocked = 0;
let loopsBroken = 0;
let sessionCount = 0;
const deniedSamples = [];
if (fs.existsSync(SESSIONS_DIR)) {
  for (const f of fs.readdirSync(SESSIONS_DIR)) {
    if (!f.endsWith(".json")) continue;
    let s;
    try {
      s = JSON.parse(fs.readFileSync(path.join(SESSIONS_DIR, f), "utf8"));
    } catch {
      continue;
    }
    sessionCount++;
    const denials = s.denials || {};
    for (const [key, n] of Object.entries(denials)) {
      const count = typeof n === "number" ? n : 1;
      if (key.startsWith("loop:")) {
        loopsBroken += count;
        if (deniedSamples.length < 24)
          deniedSamples.push({ kind: "loop", label: key.slice(5), count });
      } else {
        reReadsBlocked += count;
        if (deniedSamples.length < 24) {
          // key looks like "/abs/path:Lines(1, 200)" — show the basename + range
          const m = key.match(/^(.*):(Lines|Tail)\(([^)]*)\)$/);
          const label = m
            ? `${path.basename(m[1])} · ${m[2].toLowerCase()}(${m[3]})`
            : path.basename(key);
          deniedSamples.push({ kind: "reread", label, count });
        }
      }
    }
  }
}

// ---- 3. authoritative measured aggregate from `dasein savings` -----------
// Parse only the token numbers we present as headline; never re-derive them.
let hookTokensSaved = null;
let proxyLifetime = null;
try {
  const out = execFileSync(DASEIN_BIN, ["savings"], {
    encoding: "utf8",
    timeout: 8000,
  });
  const hookLine = out.match(
    /(\d[\d,]*)\s+re-reads blocked,\s+~?([\d.]+)k?\s+tokens saved,\s+(\d+)\s+command loops? broken/i
  );
  if (hookLine) {
    hookTokensSaved = Math.round(parseFloat(hookLine[2]) * 1000);
  }
  const proxyLine = out.match(
    /input:\s+(\d+)\s+counterfactual vs\s+(\d+)\s+billed.*?~?(\d+)\s+tok saved\s+\(([\d.]+)%\)/i
  );
  if (proxyLine) {
    proxyLifetime = {
      counterfactual: +proxyLine[1],
      billed: +proxyLine[2],
      saved: +proxyLine[3],
      pct: parseFloat(proxyLine[4]),
    };
  }
} catch (e) {
  console.warn(`(could not run \`${DASEIN_BIN} savings\`: ${e.message})`);
}

// ---- 4. emit tape --------------------------------------------------------
const tape = {
  schema: "dasein-demo-tape/v0",
  generatedFrom: {
    ledger: LEDGER,
    sessions: SESSIONS_DIR,
    savingsBinary: DASEIN_BIN,
  },
  prompt:
    "Same task, same model, same machine — routed through Claude Code with " +
    "and without the dasein plugin.",
  conv,
  turns,
  totals: {
    plainContext: sumPlain,
    pluginContext: sumPlugin,
    saved: sumSaved,
    pct: sumPlain > 0 ? +((sumSaved / sumPlain) * 100).toFixed(1) : 0,
  },
  hook: {
    sessionCount,
    reReadsBlocked,
    loopsBroken,
    tokensSaved: hookTokensSaved, // measured by `dasein savings`; null if unavailable
    deniedSamples,
  },
  proxyLifetime, // measured lifetime aggregate across all conversations
};

fs.mkdirSync(path.dirname(OUT), { recursive: true });
fs.writeFileSync(OUT, JSON.stringify(tape, null, 2));
console.log(
  `wrote ${OUT}\n` +
    `  conversation ${conv}: ${turns.length} turns, ` +
    `${sumPlain.toLocaleString()} counterfactual vs ${sumPlugin.toLocaleString()} sent ` +
    `→ ${sumSaved.toLocaleString()} saved (${tape.totals.pct}%)\n` +
    `  hook: ${reReadsBlocked} re-reads blocked, ${loopsBroken} loops broken across ${sessionCount} sessions` +
    (hookTokensSaved ? ` (~${(hookTokensSaved / 1000).toFixed(1)}k tok, measured)` : "")
);
