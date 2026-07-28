import type { ReactNode } from "react";

// Browser-chrome frame for product shots. The body area is deliberately
// unpadded: children own their own padding, so the whole child can later be
// swapped for a real screenshot <img> that sits edge-to-edge in the frame.
export function BrowserMockup({
  children,
  url,
}: {
  children: ReactNode;
  url: string;
}) {
  return (
    <div className="overflow-hidden rounded-lg border border-line bg-surface shadow-md">
      <div className="flex items-center gap-3 border-b border-line px-4 py-2.5">
        <span className="flex gap-1.5" aria-hidden="true">
          <span className="h-2.5 w-2.5 rounded-full bg-dim" />
          <span className="h-2.5 w-2.5 rounded-full bg-dim" />
          <span className="h-2.5 w-2.5 rounded-full bg-dim" />
        </span>
        <span className="rounded-md bg-elevated px-2.5 py-0.5 text-xs text-muted">
          {url}
        </span>
      </div>
      {children}
    </div>
  );
}

// Compact HTML/CSS stand-in for a dashboard screenshot — mirrors the real app
// (packages/frontend/src/app/page.tsx): the CostSaved headline card, the
// Spend / Fail-open tiles, the by-model table. Replace this whole component
// with a real <img alt="…"> once we capture one; nothing else references it.
//
// All figures are ILLUSTRATIVE (a plausible single-dev week) — the section
// that renders this must caption it as such. They stay internally consistent:
// 5.12 + 2.29 = 7.41 saved; 486k + 913k ≈ 1.4M tokens; 118 + 284 = 402 rows.
// The dollar value is the section's one phosphor accent — no .glow here; the
// hero owns the page's glow.
export function DashboardMockup() {
  return (
    <div className="space-y-3 bg-void p-4">
      <p className="text-xs text-muted">
        <span className="text-dim" aria-hidden="true">
          ❯{" "}
        </span>
        usage &amp; savings
      </p>

      {/* headline card */}
      <div className="rounded-md border border-line bg-surface p-3">
        <div className="text-xs tracking-caps text-faint uppercase">
          Total cost saved
        </div>
        <div className="mt-1 text-lg font-bold tabular-nums text-phosphor">
          $7.41
        </div>
        <div className="mt-1 text-xs text-faint">
          1.4M input tokens not sent · measured on 391 of 402 requests
        </div>
      </div>

      {/* spend / fail-open tiles */}
      <div className="grid grid-cols-2 gap-3">
        <div className="rounded-md border border-line bg-surface p-3">
          <div className="text-xs tracking-caps text-faint uppercase">Spend</div>
          <div className="mt-1 text-sm font-bold tabular-nums text-ink">
            $18.63
          </div>
          <div className="mt-1 text-xs text-faint">billed at list price</div>
        </div>
        <div className="rounded-md border border-line bg-surface p-3">
          <div className="text-xs tracking-caps text-faint uppercase">
            Fail-open
          </div>
          <div className="mt-1 text-sm font-bold tabular-nums text-ink">0</div>
          <div className="mt-1 text-xs text-faint">served passthrough</div>
        </div>
      </div>

      {/* by-model table */}
      <div>
        <p className="mb-1.5 text-xs tracking-caps text-faint uppercase">
          By model
        </p>
        <div className="overflow-x-auto rounded-md border border-line bg-surface">
          <table className="w-full text-xs tabular-nums">
            <thead className="text-left text-faint">
              <tr className="border-b border-line">
                <th className="px-3 py-1.5 font-medium">Model</th>
                <th className="px-3 py-1.5 text-right font-medium">Requests</th>
                <th className="px-3 py-1.5 text-right font-medium">
                  Tokens saved
                </th>
                <th className="px-3 py-1.5 text-right font-medium">
                  Cost saved
                </th>
              </tr>
            </thead>
            <tbody className="text-muted">
              <tr className="border-b border-line">
                <td className="px-3 py-1.5">claude-opus-5</td>
                <td className="px-3 py-1.5 text-right">118</td>
                <td className="px-3 py-1.5 text-right">486k</td>
                <td className="px-3 py-1.5 text-right">$5.12</td>
              </tr>
              <tr>
                <td className="px-3 py-1.5">claude-sonnet-5</td>
                <td className="px-3 py-1.5 text-right">284</td>
                <td className="px-3 py-1.5 text-right">913k</td>
                <td className="px-3 py-1.5 text-right">$2.29</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
