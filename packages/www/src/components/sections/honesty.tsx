// honesty — the conversion section for skeptical engineers. The counterfactual
// pipeline is docs/savings-reporting.md; the invariant is DIRECTION.md §8.4.
// The ledger-row visual is ILLUSTRATIVE and captioned as such — copy-honesty
// rule in AGENTS.md. No .glow in this section by design.

// Field names match contracts/schemas/savings-ledger.schema.json spirit; values
// are placeholders, marked illustrative in the caption below the block.
const LEDGER_ROW = [
  { k: "counterfactual_input_tokens", v: "21968", cls: "text-phosphor" },
  { k: "billed_input_tokens", v: "1204", cls: "text-phosphor" },
  { k: "cache_read_tokens", v: "9861", cls: "text-ink" },
  { k: "model", v: '"claude-sonnet-5"', cls: "text-ink" },
  { k: "request_id", v: '"req_011CQx…"', cls: "text-ink" },
  { k: "fail_open", v: "false", cls: "text-ink" },
] as const;

export function Honesty() {
  return (
    <section id="honesty" aria-labelledby="honesty-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <p className="text-xs tracking-caps text-faint uppercase">measurement honesty</p>
        <h2
          id="honesty-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          Measured, never modeled
        </h2>
        <div className="mt-10 grid gap-10 lg:grid-cols-2">
          <div className="space-y-4 text-base text-muted">
            <p>
              Before curation touches anything, the proxy calls Anthropic&apos;s
              free{" "}
              <code className="rounded-sm bg-elevated px-1 py-0.5 text-sm text-ink">
                count_tokens
              </code>{" "}
              on the original request body — the exact bytes you would have
              sent.
            </p>
            <p>
              After the response comes back, it records what you were actually
              billed. The savings number is the difference between the two,
              computed per request.
            </p>
            <p>
              Every row lands in a local ledger —{" "}
              <code className="rounded-sm bg-elevated px-1 py-0.5 text-sm text-ink">
                ~/.parsec/ledger.jsonl
              </code>{" "}
              — that you can open and read yourself.
            </p>
            <p className="text-ink">
              If the probe fails, the row records a hole — never an invented
              number.
            </p>
          </div>
          <figure>
            <div className="overflow-x-auto rounded-lg border border-line bg-surface p-6">
              <pre className="text-sm">
                <span className="text-faint">{"{"}</span>
                {"\n"}
                {LEDGER_ROW.map((row, i) => (
                  <span key={row.k}>
                    {"  "}
                    <span className="text-muted">&quot;{row.k}&quot;</span>
                    <span className="text-faint">: </span>
                    <span className={row.cls}>{row.v}</span>
                    {i < LEDGER_ROW.length - 1 && (
                      <span className="text-faint">,</span>
                    )}
                    {"\n"}
                  </span>
                ))}
                <span className="text-faint">{"}"}</span>
              </pre>
            </div>
            <figcaption className="mt-3 text-xs text-faint">
              One row of the local savings ledger — illustrative values.
            </figcaption>
          </figure>
        </div>
      </div>
    </section>
  );
}
