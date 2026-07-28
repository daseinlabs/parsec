import { TESTIMONIALS } from "@/content/testimonials";

// Quotes come from src/content/testimonials.ts — currently placeholder copy
// (see the banner there). This component is copy-agnostic: it renders the
// array as-is. No Review/Rating structured data on purpose.
export function Testimonials() {
  return (
    <section
      id="testimonials"
      aria-labelledby="testimonials-h"
      className="border-t border-line"
    >
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        {/* Heading stays a neutral section label while the quotes are the
            TODO placeholders from content/testimonials.ts — "notes from
            production" would assert a provenance the data doesn't have yet. */}
        <p className="text-xs tracking-caps text-faint uppercase">
          testimonials
        </p>
        <h2
          id="testimonials-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          What engineers say
        </h2>

        <div className="mt-10 grid gap-4 md:grid-cols-3">
          {TESTIMONIALS.map((t) => (
            <figure
              key={t.name}
              className="flex h-full flex-col justify-between rounded-lg border border-line bg-surface p-6"
            >
              <blockquote className="text-base text-ink">
                <p>&ldquo;{t.quote}&rdquo;</p>
              </blockquote>
              <figcaption className="mt-6 text-xs text-muted">
                {t.name}
                <span aria-hidden="true" className="text-dim">
                  {" · "}
                </span>
                {t.role}
                <span aria-hidden="true" className="text-dim">
                  {" · "}
                </span>
                {t.company}
              </figcaption>
            </figure>
          ))}
        </div>
      </div>
    </section>
  );
}
