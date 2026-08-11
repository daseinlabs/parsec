"use client";

import { useEffect, useState } from "react";

// Live site-wide savings counter in the hero's right column, fetched from the
// platform's public aggregate (GET /savings/public — unauthenticated,
// cross-account totals, nothing per-user). Fetched client-side because www is
// a static export with no BFF; the platform allows the getparsec.ai origins
// via CORS. NEXT_PUBLIC_PLATFORM_URL is inlined at build time — when it is
// unset (local builds) or the fetch fails or returns zero, the component
// renders nothing and the hero keeps its single column: the copy rule allows
// only real count_tokens-measured numbers, so there is no placeholder state.
const PLATFORM_URL = process.env.NEXT_PUBLIC_PLATFORM_URL;

type PublicSavings = {
  tokens_saved: number;
  cost_saved_usd: number;
  currency: string;
};

const usd = (v: number) =>
  new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: v < 100 ? 2 : 0,
  }).format(v);

const compact = (v: number) =>
  new Intl.NumberFormat("en-US", {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(v);

export function SavingsStat() {
  const [stat, setStat] = useState<PublicSavings | null>(null);

  useEffect(() => {
    if (!PLATFORM_URL) return;
    const ctrl = new AbortController();
    fetch(`${PLATFORM_URL}/savings/public`, { signal: ctrl.signal })
      .then((resp) => (resp.ok ? resp.json() : null))
      .then((data: PublicSavings | null) => {
        if (data && data.tokens_saved > 0) setStat(data);
      })
      .catch(() => {
        /* platform unreachable — the hero simply stays single-column */
      });
    return () => ctrl.abort();
  }, []);

  if (!stat) return null;

  return (
    <div className="rounded-md border border-line bg-surface px-6 py-5">
      <p className="text-xs tracking-caps text-faint uppercase">
        Saved by parsec users
      </p>
      <p className="mt-3 text-3xl font-extrabold tracking-display text-phosphor">
        {usd(stat.cost_saved_usd)}
      </p>
      <p className="mt-1 text-sm text-muted">
        <span className="text-ink">{compact(stat.tokens_saved)}</span> input
        tokens
      </p>
      <p className="mt-3 text-xs text-faint">
        Measured per-request, never estimated.
      </p>
    </div>
  );
}
