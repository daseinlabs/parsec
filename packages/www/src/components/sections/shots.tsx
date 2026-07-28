import { BrowserMockup, DashboardMockup } from "@/components/mockup";

// Product shots: the savings ledger (dashboard) and the plugin surface
// (terminal). Both are rendered HTML/CSS, not screenshots — each frame's
// content is trivially replaceable with a real <img alt="…"> capture later
// (the frames leave their body unpadded/owned by the child for exactly that).
// The shared caption below the grid marks everything as illustrative.
export function Shots() {
  return (
    <section id="product" aria-labelledby="product-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <p className="text-xs tracking-caps text-faint uppercase">the product</p>
        <h2
          id="product-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          What it looks like
        </h2>
        <p className="mt-3 max-w-2xl text-sm text-muted">
          A ledger, not a chart deck: dollars saved per request on the
          dashboard, and the same accounting surfaced in your session.
        </p>

        <div className="mt-8 grid gap-6 lg:grid-cols-2">
          {/* (a) the dashboard, in browser chrome */}
          <figure>
            <BrowserMockup url="app.getparsec.ai">
              <DashboardMockup />
            </BrowserMockup>
            <figcaption className="mt-3 text-xs text-faint">
              The savings ledger — measured per request
            </figcaption>
          </figure>

          {/* (b) the plugin surface, in terminal chrome — same border/bg
              recipe as BrowserMockup's bar; swap the body for a real
              screenshot <img> when we have one. */}
          <figure>
            <div className="overflow-hidden rounded-lg border border-line bg-surface shadow-md">
              <div className="flex items-center gap-3 border-b border-line px-4 py-2.5">
                <span className="flex gap-1.5" aria-hidden="true">
                  <span className="h-2.5 w-2.5 rounded-full bg-dim" />
                  <span className="h-2.5 w-2.5 rounded-full bg-dim" />
                  <span className="h-2.5 w-2.5 rounded-full bg-dim" />
                </span>
                <span className="rounded-md bg-elevated px-2.5 py-0.5 text-xs text-muted">
                  claude
                </span>
              </div>
              <div className="space-y-2 bg-void p-4 text-xs">
                <p className="text-muted">
                  <span className="text-dim" aria-hidden="true">
                    ›{" "}
                  </span>
                  Read src/lib/platform.ts
                </p>
                <p className="text-muted">
                  <span className="text-warning" aria-hidden="true">
                    ⚠{" "}
                  </span>
                  re-read denied: file already in context (insist once to
                  override)
                </p>
                <p className="border-t border-line pt-2 text-muted">
                  parsec{" "}
                  <span className="text-dim" aria-hidden="true">
                    ✦{" "}
                  </span>
                  <span className="tabular-nums text-ink">12.4k saved</span>
                </p>
              </div>
            </div>
            <figcaption className="mt-3 text-xs text-faint">
              The plugin surface — guardrails and savings in-session
            </figcaption>
          </figure>
        </div>

        <p className="mt-6 text-xs text-faint">
          Illustrative — rendered UI, not screenshots; numbers shown are
          examples.
        </p>
      </div>
    </section>
  );
}
