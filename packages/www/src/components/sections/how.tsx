// Three steps, facts sourced from DIRECTION.md §4/§5: plugin as the
// integration surface, data plane local / scores from the hosted brain,
// per-request count_tokens measurement. No capabilities invented here.
const STEPS = [
  {
    n: "01",
    title: "Install the plugin",
    body:
      "One command in Claude Code — no base-URL surgery, no wire config. " +
      "Agents, hooks, and the savings status line arrive as plugin surfaces.",
  },
  {
    n: "02",
    title: "A local proxy curates each turn",
    body:
      "Chunking, freezing, and splicing happen on your machine. The hosted " +
      "brain returns keep/cut scores; the proxy applies the cut, and your " +
      "request rides your own credentials to Anthropic.",
  },
  {
    n: "03",
    title: "Savings are measured, not modeled",
    body:
      "Every request runs a free count_tokens probe of the original body " +
      "against what was actually billed. No modeled baseline, no " +
      "extrapolation — if the ledger is empty, the report says so.",
  },
] as const;

export function How() {
  return (
    <section id="how" aria-labelledby="how-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <p className="text-xs tracking-caps text-faint uppercase">
          how it works
        </p>
        <h2
          id="how-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          Local proxy, hosted brain
        </h2>

        <ol className="mt-10 grid grid-cols-1 gap-4 md:grid-cols-3">
          {STEPS.map((step) => (
            <li
              key={step.n}
              className="rounded-lg border border-line bg-surface p-6"
            >
              {/* The <ol> carries the numbering semantics; the printed
                  number is presentation. */}
              <p aria-hidden className="text-sm font-bold text-phosphor">
                {step.n}
              </p>
              <h3 className="mt-3 text-md font-bold tracking-display text-ink">
                {step.title}
              </h3>
              <p className="mt-2 text-sm text-muted">{step.body}</p>
            </li>
          ))}
        </ol>
      </div>
    </section>
  );
}
