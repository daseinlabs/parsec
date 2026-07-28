"use client";

// Fake-terminal replay of the install → savings flow. Crawlability first: the
// FULL transcript is server-rendered and visible; the typing effect is pure
// progressive enhancement. `shown === null` (the SSR/no-JS/reduced-motion
// state) renders every line. Only after the IntersectionObserver fires does a
// line-count ticker hide the transcript and reveal it sequentially — so with
// JS disabled nothing is ever hidden.
//
// Role colors follow brand/parsecbrandkit/BRANDING.md §5: prompt ❯ phosphor,
// ok ✓ success, step • muted.

import { useEffect, useRef, useState } from "react";
import { INSTALL_COMMANDS } from "@/lib/site";

type Seg = {
  text: string;
  className?: string;
  // Decorative role glyph (❯ ✓ •) — hidden from the accessibility tree.
  glyph?: boolean;
};

type Line = {
  segs: Seg[];
  // Pause before this line appears, ms.
  delay: number;
};

const prompt = (cmd: string): Line => ({
  segs: [
    { text: "❯ ", className: "text-phosphor", glyph: true },
    { text: cmd, className: "text-ink" },
  ],
  delay: 650,
});

const ok = (msg: string): Line => ({
  segs: [
    { text: "✓ ", className: "text-success", glyph: true },
    { text: msg, className: "text-muted" },
  ],
  delay: 300,
});

const step = (msg: string): Line => ({
  segs: [
    { text: "• ", className: "text-muted", glyph: true },
    { text: msg, className: "text-muted" },
  ],
  delay: 350,
});

const blank = (): Line => ({ segs: [{ text: " " }], delay: 200 });

// Illustrative session — the Demo section captions it as such. The numbers
// here are demo copy, not a claim; real reports come from the per-request
// count_tokens counterfactual (docs/savings-reporting.md).
const LINES: Line[] = [
  prompt(INSTALL_COMMANDS[0]),
  ok("marketplace added: parsec-marketplace"),
  prompt(INSTALL_COMMANDS[1]),
  ok("parsec installed — prebuilt binary, no postinstall"),
  blank(),
  prompt("claude"),
  step("parsec active — local proxy running, requests go straight to Anthropic"),
  prompt("/parsec:savings"),
  blank(),
  {
    segs: [
      { text: "  " },
      // text-phosphor only — the hero owns the page's one .glow.
      { text: "12.4k", className: "font-bold text-phosphor" },
      { text: " input tokens not sent this session", className: "text-ink" },
    ],
    delay: 450,
  },
  {
    segs: [
      {
        text: "  across 31 requests — each measured via count_tokens",
        className: "text-muted",
      },
    ],
    delay: 300,
  },
];

const CURSOR_DELAY = 400;
// Content lines + the trailing cursor line.
const TOTAL = LINES.length + 1;

export function Terminal() {
  const ref = useRef<HTMLDivElement>(null);
  // null → animation has not taken over: every line is visible. This is the
  // server-rendered state, the no-JS state, and the reduced-motion state.
  const [shown, setShown] = useState<number | null>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;

    let timer = 0;
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (!entry.isIntersecting) return;
        observer.disconnect();
        setShown(0);
        let i = 0;
        const tick = () => {
          i += 1;
          setShown(i);
          if (i < TOTAL) {
            timer = window.setTimeout(
              tick,
              i < LINES.length ? LINES[i].delay : CURSOR_DELAY,
            );
          }
        };
        timer = window.setTimeout(tick, LINES[0].delay);
      },
      { threshold: 0.2 },
    );
    observer.observe(el);

    return () => {
      observer.disconnect();
      window.clearTimeout(timer);
    };
  }, []);

  const lineClass = (i: number) =>
    `whitespace-pre transition-opacity duration-300 ease-parsec ${
      shown !== null && i >= shown ? "opacity-0" : "opacity-100"
    }`;

  return (
    <div
      ref={ref}
      className="overflow-hidden rounded-lg border border-line bg-surface shadow-md"
    >
      <div className="flex items-center gap-2 border-b border-line px-4 py-2">
        <div aria-hidden className="flex gap-1.5 text-xs text-dim">
          <span>●</span>
          <span>●</span>
          <span>●</span>
        </div>
        <p className="flex-1 text-center text-xs text-faint">
          ~ — claude code
        </p>
        {/* Balances the dots so the title stays optically centered. */}
        <span aria-hidden className="w-8" />
      </div>
      <div className="overflow-x-auto p-4 text-sm">
        {LINES.map((line, i) => (
          <div key={i} className={lineClass(i)}>
            {line.segs.map((seg, j) => (
              <span
                key={j}
                className={seg.className}
                aria-hidden={seg.glyph || undefined}
              >
                {seg.text}
              </span>
            ))}
          </div>
        ))}
        <div className={lineClass(LINES.length)}>
          <span aria-hidden className="text-phosphor">
            ❯{" "}
          </span>
          <span aria-hidden className="animate-pulse text-phosphor">
            █
          </span>
        </div>
      </div>
    </div>
  );
}
