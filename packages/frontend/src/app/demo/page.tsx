"use client";

// Live "cost race" demo: the same task, same model, same machine — routed
// through Claude Code with and without the dasein plugin. Left panel is plain
// Claude Code; right panel is Claude Code + dasein. Both meters fill with the
// tokens actually sent to the model each turn; the gap is the measured saving.
//
// Every number is replayed from public/demo-tape.json, which is built from the
// real on-disk dasein ledger + hook state (scripts/build-demo-tape.mjs). No
// estimates — honoring the repo's measurement-honesty invariant.

import { useEffect, useMemo, useRef, useState } from "react";

type Turn = {
  t: number;
  plainContext: number;
  pluginContext: number;
  saved: number;
  model: string | null;
  freezeCut: number;
  toolsTotal: number | null;
  toolsKept: number | null;
  toolsStubbed: number | null;
  insists: number;
  brainMs: number | null;
  failOpen: boolean;
};

type Tape = {
  prompt: string;
  conv: string;
  turns: Turn[];
  totals: { plainContext: number; pluginContext: number; saved: number; pct: number };
  hook: {
    sessionCount: number;
    reReadsBlocked: number;
    loopsBroken: number;
    tokensSaved: number | null;
    deniedSamples: { kind: string; label: string; count: number }[];
  };
  proxyLifetime: { counterfactual: number; billed: number; saved: number; pct: number } | null;
  generatedFrom: { ledger: string; sessions: string; savingsBinary: string };
};

const CADENCE_MS = 1900; // wall-clock between turns landing
const nf = new Intl.NumberFormat("en");
const fmt = (n: number) => nf.format(Math.round(n));

// Ease a displayed number toward its target so counters STREAM up like tokens
// flooding in, rather than snapping between turns.
function useStream(target: number) {
  const [val, setVal] = useState(target);
  const cur = useRef(target);
  useEffect(() => {
    let raf = 0;
    const tick = () => {
      const d = target - cur.current;
      if (Math.abs(d) < 0.5) {
        cur.current = target;
        setVal(target);
        return;
      }
      cur.current += d * 0.1;
      setVal(cur.current);
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [target]);
  return val;
}

export default function DemoPage() {
  const [tape, setTape] = useState<Tape | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [landed, setLanded] = useState(0); // turns processed so far
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState(1);
  const [flashIdx, setFlashIdx] = useState(0);
  const [promptText, setPromptText] = useState(
    "Add a --format=json flag to `dasein savings` and cover it with tests."
  );
  const [ran, setRan] = useState("");
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);

  useEffect(() => {
    fetch("/demo-tape.json")
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then(setTape)
      .catch((e) => setErr(String(e)));
    return () => timers.current.forEach(clearTimeout);
  }, []);

  // Cumulative sent-tokens after k landed turns (index 0..n).
  const { cumPlain, cumPlugin, maxCum } = useMemo(() => {
    const cp = [0];
    const cg = [0];
    if (tape) {
      for (const t of tape.turns) {
        cp.push(cp[cp.length - 1] + t.plainContext);
        cg.push(cg[cg.length - 1] + t.pluginContext);
      }
    }
    return { cumPlain: cp, cumPlugin: cg, maxCum: cp[cp.length - 1] || 1 };
  }, [tape]);

  // Cycle the lifetime DENIED sample chips while playing.
  useEffect(() => {
    if (!playing || !tape?.hook.deniedSamples.length) return;
    const id = setInterval(
      () => setFlashIdx((i) => (i + 1) % tape.hook.deniedSamples.length),
      1100 / speed
    );
    return () => clearInterval(id);
  }, [playing, tape, speed]);

  // Targets drive the streaming counters (hooks must run before any return).
  const plainNow = cumPlain[landed];
  const pluginNow = cumPlugin[landed];
  const savedTarget = Math.max(0, plainNow - pluginNow);
  const pctTarget = plainNow > 0 ? (savedTarget / plainNow) * 100 : 0;
  const savedStream = useStream(savedTarget);
  const pctStream = useStream(pctTarget);

  function reset() {
    timers.current.forEach(clearTimeout);
    timers.current = [];
    setLanded(0);
    setPlaying(false);
  }

  function play() {
    if (!tape) return;
    reset();
    setRan(promptText.trim() || "(empty prompt)");
    setPlaying(true);
    const step = CADENCE_MS / speed;
    tape.turns.forEach((_, i) => {
      timers.current.push(setTimeout(() => setLanded(i + 1), step * (i + 1)));
    });
    timers.current.push(
      setTimeout(() => setPlaying(false), step * tape.turns.length + 400)
    );
  }

  if (err)
    return (
      <main className="mx-auto max-w-2xl p-10 text-sm text-red-500">
        Couldn&apos;t load the demo tape ({err}). Build it with{" "}
        <code className="rounded bg-neutral-800 px-1">node scripts/build-demo-tape.mjs</code>.
      </main>
    );
  if (!tape) return <main className="p-10 text-sm text-neutral-500">Loading tape…</main>;

  const done = landed >= tape.turns.length;
  const cur = landed > 0 ? tape.turns[landed - 1] : null;
  const flash = tape.hook.deniedSamples[flashIdx];

  return (
    <main className="mx-auto min-h-full w-full max-w-6xl px-6 py-10">
      <header className="mb-8">
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-semibold tracking-tight">The dasein cost race</h1>
          <span className="rounded-full border border-teal-500/40 bg-teal-500/10 px-2 py-0.5 text-xs font-medium text-teal-500">
            measured · not estimated
          </span>
        </div>
        <p className="mt-2 max-w-2xl text-sm text-neutral-500">
          Same task, same model ({tape.turns[0]?.model ?? "—"}), same machine — routed through
          Claude Code with and without the dasein plugin. Each meter fills with the tokens actually
          sent to the model. The gap is what dasein saved, replayed from the real ledger.
        </p>
      </header>

      {/* Prompt bar — type a task, press Enter, it runs on both sides at once */}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          play();
        }}
        className="mb-3"
      >
        <div className="flex items-center gap-2.5 rounded-lg border border-neutral-300 bg-white px-3.5 py-3 font-mono text-sm shadow-sm focus-within:border-teal-500/70 dark:border-neutral-700 dark:bg-neutral-950">
          <span className="select-none text-teal-500">❯</span>
          <input
            value={promptText}
            onChange={(e) => setPromptText(e.target.value)}
            placeholder="Describe a task and press Enter…"
            spellCheck={false}
            autoFocus
            aria-label="Prompt for both agents"
            className="flex-1 bg-transparent text-neutral-900 outline-none placeholder:text-neutral-400 dark:text-neutral-100"
          />
          <kbd className="hidden shrink-0 rounded border border-neutral-300 px-1.5 py-0.5 text-[10px] text-neutral-500 sm:inline dark:border-neutral-700">
            ⏎ Enter
          </kbd>
        </div>
      </form>

      {/* Secondary controls */}
      <div className="mb-6 flex flex-wrap items-center gap-3 text-xs text-neutral-500">
        <span>Sent to both agents at once — plain Claude Code and Claude Code + dasein.</span>
        <div className="ml-auto flex items-center gap-1">
          speed
          {[0.5, 1, 2].map((s) => (
            <button
              key={s}
              onClick={() => setSpeed(s)}
              className={`rounded px-2 py-1 tabular-nums ${
                speed === s
                  ? "bg-neutral-200 text-neutral-900 dark:bg-neutral-700 dark:text-neutral-100"
                  : "text-neutral-500 hover:text-neutral-300"
              }`}
            >
              {s}×
            </button>
          ))}
        </div>
        {playing && !done && (
          <span className="flex items-center gap-1.5 font-medium text-teal-500">
            <span className="relative flex h-2 w-2">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-teal-400 opacity-75" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-teal-500" />
            </span>
            streaming context
          </span>
        )}
        <span className="tabular-nums">
          turn {landed} / {tape.turns.length}
        </span>
      </div>

      {/* Echo the running prompt across both panels */}
      {ran && (
        <div className="mb-3 truncate font-mono text-xs text-neutral-500">
          <span className="text-teal-500">❯</span> {ran}
        </div>
      )}

      {/* The race */}
      <div className="grid grid-cols-1 gap-4 md:grid-cols-[1fr_auto_1fr] md:items-stretch">
        <Column
          label="Plain Claude Code"
          sub="no plugin"
          tone="plain"
          now={plainNow}
          max={maxCum}
          fill={maxCum ? plainNow / maxCum : 0}
          ghostFill={0}
          footer={cur ? `turn ${landed}: sent ${fmt(cur.plainContext)} tok of context` : "ready"}
        />

        {/* Savings pillar */}
        <div className="flex flex-col items-center justify-center gap-1 px-2 py-6 text-center">
          <div className="text-xs uppercase tracking-wide text-neutral-500">saved so far</div>
          <div className="text-3xl font-bold tabular-nums text-teal-500 transition-all">
            {fmt(savedStream)}
          </div>
          <div className="text-sm font-medium tabular-nums text-teal-500/80">
            {pctStream.toFixed(1)}% less context
          </div>
          <div className="mt-1 text-[11px] text-neutral-500">tokens never sent</div>
        </div>

        <Column
          label="Claude Code + dasein"
          sub="curator + no-reread hook"
          tone="dasein"
          now={pluginNow}
          max={maxCum}
          fill={maxCum ? pluginNow / maxCum : 0}
          ghostFill={maxCum ? plainNow / maxCum : 0}
          footer={
            cur
              ? `turn ${landed}: scored ${cur.toolsTotal ?? "?"} tools → kept ${
                  cur.toolsKept ?? "?"
                }, froze ${fmt(cur.saved)} tok${cur.failOpen ? " · FAIL-OPEN (passthrough)" : ""}`
              : "ready"
          }
        />
      </div>

      {/* Hook layer — measured lifetime aggregate, distinct from the per-run race */}
      <section className="mt-8 rounded-lg border border-neutral-200 p-5 dark:border-neutral-800">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 className="text-sm font-semibold">
            No-reread hook{" "}
            <span className="font-normal text-neutral-500">— lifetime, across your machine</span>
          </h2>
          <span className="text-xs text-neutral-400">
            measured across {tape.hook.sessionCount} sessions
          </span>
        </div>
        <div className="mt-3 grid grid-cols-2 gap-4 sm:grid-cols-3">
          <Stat label="re-reads blocked" value={fmt(tape.hook.reReadsBlocked)} />
          <Stat
            label="tokens saved"
            value={tape.hook.tokensSaved ? `~${fmt(tape.hook.tokensSaved)}` : "—"}
          />
          <Stat label="command loops broken" value={fmt(tape.hook.loopsBroken)} />
        </div>
        {flash && (
          <div className="mt-4 flex items-center gap-2 font-mono text-xs">
            <span className="rounded bg-red-500/15 px-2 py-1 font-semibold text-red-500">
              {flash.kind === "loop" ? "LOOP BROKEN" : "RE-READ DENIED"}
            </span>
            <span className="truncate text-neutral-500">{flash.label}</span>
          </div>
        )}
      </section>

      {/* Final headline */}
      {done && (
        <section className="mt-6 rounded-lg border border-teal-500/30 bg-teal-500/5 p-6">
          <div className="text-sm text-neutral-500">This run, end to end</div>
          <div className="mt-1 flex flex-wrap items-baseline gap-x-6 gap-y-1">
            <span className="text-3xl font-bold tabular-nums text-teal-500">
              {tape.totals.pct}% less context
            </span>
            <span className="text-lg tabular-nums text-neutral-400">
              {fmt(tape.totals.plainContext)} → {fmt(tape.totals.pluginContext)} tokens
              <span className="text-teal-500"> ({fmt(tape.totals.saved)} saved)</span>
            </span>
          </div>
        </section>
      )}

      <footer className="mt-10 border-t border-neutral-200 pt-4 text-[11px] leading-relaxed text-neutral-400 dark:border-neutral-800">
        Every number is measured from the dasein ledger and hook state — the per-request{" "}
        <code>count_tokens</code> counterfactual vs actually-billed usage. Nothing here is modeled
        or extrapolated.
      </footer>
    </main>
  );
}

function Column({
  label,
  sub,
  tone,
  now,
  max,
  fill,
  ghostFill,
  footer,
}: {
  label: string;
  sub: string;
  tone: "plain" | "dasein";
  now: number;
  max: number;
  fill: number;
  ghostFill: number;
  footer: string;
}) {
  const isDasein = tone === "dasein";
  const shown = useStream(now);
  return (
    <div className="rounded-lg border border-neutral-200 p-5 dark:border-neutral-800">
      <div className="flex items-baseline justify-between">
        <div>
          <div className="text-sm font-semibold">{label}</div>
          <div className="text-xs text-neutral-500">{sub}</div>
        </div>
        <div className="text-right">
          <div
            className={`text-2xl font-bold tabular-nums transition-all ${
              isDasein ? "text-teal-500" : "text-neutral-400"
            }`}
          >
            {fmt(shown)}
          </div>
          <div className="text-[10px] uppercase tracking-wide text-neutral-500">tokens sent</div>
        </div>
      </div>

      {/* Meter */}
      <div className="relative mt-4 h-56 w-full overflow-hidden rounded-md bg-neutral-100 dark:bg-neutral-900">
        {/* ghost: where dasein WOULD be without compression (= plain's height) */}
        {isDasein && ghostFill > 0 && (
          <div
            className="absolute inset-x-0 bottom-0 border-t border-dashed border-teal-500/50 bg-[repeating-linear-gradient(45deg,transparent,transparent_6px,rgba(20,184,166,0.06)_6px,rgba(20,184,166,0.06)_12px)] transition-all duration-[1400ms] ease-out"
            style={{ height: `${Math.min(100, ghostFill * 100)}%` }}
          >
            <span className="absolute right-1 top-1 text-[9px] font-medium text-teal-500/70">
              would-be
            </span>
          </div>
        )}
        {/* actual fill */}
        <div
          className={`absolute inset-x-0 bottom-0 transition-all duration-[1400ms] ease-out ${
            isDasein ? "bg-teal-500/80" : "bg-neutral-400/80 dark:bg-neutral-500/70"
          }`}
          style={{ height: `${Math.min(100, fill * 100)}%` }}
        />
        <div className="pointer-events-none absolute inset-0 flex items-start justify-end p-1">
          <span className="text-[9px] tabular-nums text-neutral-400">{fmt(max)} max</span>
        </div>
      </div>

      <div className="mt-3 min-h-[2.5rem] font-mono text-[11px] leading-snug text-neutral-500">
        {footer}
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-xl font-semibold tabular-nums">{value}</div>
      <div className="text-xs text-neutral-500">{label}</div>
    </div>
  );
}
