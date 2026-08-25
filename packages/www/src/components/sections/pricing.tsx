// Tiers from DIRECTION.md §3 — what each tier runs is settled. Pro's list
// price is $40/mo, offered free for now (struck through via wasPrice).

type Tier = {
  name: string;
  stack: string;
  price: string;
  // List price shown struck through ahead of `price` (e.g. "$40/mo" → Free).
  wasPrice?: string;
  points: string[];
  featured?: boolean;
  contact?: boolean;
};

const TIERS: Tier[] = [
  {
    name: "Pro",
    stack: "Plugin + local proxy",
    price: "Free",
    wasPrice: "$40/mo",
    featured: true,
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
    stack: "Deployed in your cloud",
    price: "Talk to us",
    contact: true,
    points: [
      "The whole stack — scoring API included — runs in your VPC",
      "Licensed checkpoints, containerized",
      "Fine-tuning on your traces, without them leaving your network",
    ],
  },
];

export function Pricing() {
  return (
    <section id="pricing" aria-labelledby="pricing-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <h2
          id="pricing-h"
          className="text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          Pricing
        </h2>
        <div className="mt-8 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {TIERS.map((tier) => (
            <div
              key={tier.name}
              className={`flex min-h-[28rem] flex-col rounded-lg border bg-surface p-6 ${
                tier.featured ? "border-line-strong" : "border-line"
              }`}
            >
              <div className="flex items-center justify-between gap-2">
                <h3 className="text-lg font-bold tracking-display text-ink">
                  {tier.name}
                </h3>
                {tier.featured && (
                  <span className="bg-phosphor text-on-phosphor rounded-sm px-1.5 py-0.5 text-xs font-bold">
                    Start here
                  </span>
                )}
              </div>
              <p className="mt-1 text-sm text-muted">{tier.stack}</p>
              <p className="mt-5 text-2xl font-bold tracking-display text-ink">
                {tier.wasPrice && (
                  <>
                    <span className="text-lg text-faint line-through">
                      {tier.wasPrice}
                    </span>{" "}
                  </>
                )}
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
                    href="mailto:hello@daseinlabs.ai"
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
      </div>
    </section>
  );
}
