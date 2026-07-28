// features — the six things parsec actually does. Facts from DIRECTION.md
// §1–§5; no numbers here beyond the sanctioned tagline elsewhere on the page.

const FEATURES = [
  {
    glyph: "✦", // ✦
    title: "Learned curator",
    body:
      "A GNN scores every chunk of context, every turn. The proxy applies " +
      "keep/cut locally. The model sees what matters.",
    free: false,
  },
  {
    glyph: "⌖", // ⌖
    title: "Exploration maps",
    body:
      "A parsec:explore agent maps the repo up front. The agent stops " +
      "re-discovering the same files every session.",
    free: true,
  },
  {
    glyph: "⊘", // ⊘
    title: "No-reread hook",
    body:
      "Re-reading a file already in context gets denied with a pointer to " +
      "where it lives. A loop-breaker catches the same command run on repeat.",
    free: true,
  },
  {
    glyph: "Σ", // Σ
    title: "Honest measurement",
    body:
      "Every request runs a count_tokens probe of the original body. Savings " +
      "are measured against what you were actually billed — never modeled.",
    free: false,
  },
  {
    glyph: "⌂", // ⌂
    title: "Data plane local",
    body:
      "Model traffic leaves your machine, with your credentials. " +
      "Subscription tokens never touch our cloud.",
    free: false,
  },
  {
    glyph: "⇢", // ⇢
    title: "Fail open",
    body:
      "Every layer degrades to passthrough on error. Fail-opens are counted " +
      "and visible, not hidden.",
    free: false,
  },
] as const;

export function Features() {
  return (
    <section id="features" aria-labelledby="features-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <p className="text-xs tracking-caps text-faint uppercase">what you get</p>
        <h2
          id="features-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          A learned curator, with receipts
        </h2>
        <ul className="mt-10 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {FEATURES.map((f) => (
            <li key={f.title} className="rounded-lg border border-line bg-surface p-6">
              <span aria-hidden="true" className="text-lg text-phosphor">
                {f.glyph}
              </span>
              <h3 className="mt-3 text-md font-bold text-ink">
                {f.title}
                {f.free && (
                  <span className="ml-2 rounded-sm border border-line px-1.5 py-0.5 align-middle text-xs font-normal tracking-caps text-faint uppercase">
                    free tier
                  </span>
                )}
              </h3>
              <p className="mt-2 text-sm text-muted">{f.body}</p>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}
