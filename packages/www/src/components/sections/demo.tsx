import { Terminal } from "@/components/terminal";

// See it run: the install pair, a session, and the savings report — replayed
// in a fake terminal. The transcript is illustrative and captioned as such;
// the only sanctioned real-number claims live elsewhere (SITE.tagline).
export function Demo() {
  return (
    <section id="demo" aria-labelledby="demo-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <p className="text-xs tracking-caps text-faint uppercase">demo</p>
        <h2
          id="demo-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          Installation
        </h2>
        <div className="mt-8 max-w-3xl">
          <Terminal />
          <p className="mt-3 text-xs text-faint">Illustrative session.</p>
        </div>
      </div>
    </section>
  );
}
