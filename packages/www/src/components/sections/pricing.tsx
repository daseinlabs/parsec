// Tiers from DIRECTION.md §3 — what each tier runs is settled; prices are not.

type Tier = {
  name: string;
  stack: string;
  price: string;
  points: string[];
  featured?: boolean;
  contact?: boolean;
};

// TODO(pricing): dollar figures are an open question (DIRECTION.md §10) — do not invent them here.
const TIERS: Tier[] = [
  {
    name: "Free",
    stack: "The plugin",
    price: "Free",
    featured: true,
    points: [
      "Exploration maps",
      "No-reread hook",
      "Savings status line",
      "Runs on your plan or key — no account needed",
    ],
  },
  {
    name: "Pro",
    stack: "Plugin + local proxy",
    price: "Coming soon",
    contact: true,
    points: [
      "The full learned curator via the hosted scoring API",
      "Tool-schema prune",
      "Governor",
    ],
  },
  {
    name: "Team",
    stack: "Hosted BYOK gateway in our cloud",
    price: "Coming soon",
    contact: true,
    points: [
      "API keys only — never subscription tokens",
      "Zero local install",
      "Centralized reporting",
    ],
  },
  {
    name: "Enterprise",
    stack: "Self-host",
    price: "Talk to us",
    contact: true,
    points: [
      "Licensed checkpoints",
      "Containerized stack",
      "On-prem fine-tuning on your traces",
    ],
  },
];

export function Pricing() {
  return (
    <section id="pricing" aria-labelledby="pricing-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <p className="text-xs tracking-caps text-faint uppercase">tiers</p>
        <h2
          id="pricing-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          Start free. The learned curator is the upgrade.
        </h2>
        <p className="mt-3 max-w-2xl text-sm text-muted">
          The free plugin is a real product, not a demo. Paid tiers add the
          brain — the part that stays server-side.
        </p>

        <div className="mt-8 grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {TIERS.map((tier) => (
            <div
              key={tier.name}
              className={`flex flex-col rounded-lg border bg-surface p-5 ${
                tier.featured ? "border-line-strong" : "border-line"
              }`}
            >
              <div className="flex items-center justify-between gap-2">
                <h3 className="text-md font-bold tracking-display text-ink">
                  {tier.name}
                </h3>
                {tier.featured && (
                  <span className="bg-phosphor text-on-phosphor rounded-sm px-1.5 py-0.5 text-xs font-bold">
                    Start here
                  </span>
                )}
              </div>
              <p className="mt-1 text-sm text-muted">{tier.stack}</p>
              <p className="mt-4 text-lg font-bold tracking-display text-ink">
                {tier.price}
              </p>
              <ul className="mt-4 space-y-2 text-sm text-muted">
                {tier.points.map((point) => (
                  <li key={point} className="flex gap-2">
                    <span aria-hidden="true" className="text-faint">
                      ·
                    </span>
                    <span>{point}</span>
                  </li>
                ))}
              </ul>
              {tier.contact && (
                <div className="mt-auto pt-6">
                  <a
                    href="mailto:hello@dasein.rocks"
                    rel="noopener"
                    className="block rounded-md border border-line px-3 py-2 text-center text-sm font-bold text-ink transition-colors ease-parsec hover:border-line-strong hover:text-phosphor"
                  >
                    Contact
                  </a>
                </div>
              )}
            </div>
          ))}
        </div>

        <p className="mt-6 text-xs text-faint">
          On subscription plans there is no per-token bill — the win is more
          Claude Code inside the same rate limits.
        </p>
      </div>
    </section>
  );
}
