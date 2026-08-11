import { JsonLd, faqLd } from "@/components/json-ld";

// Answers are grounded in DIRECTION.md (§2 revised, §3, §6) and double as
// FAQPage JSON-LD — keep them plain text, no markup.
export const FAQ_ITEMS: { q: string; a: string }[] = [
  {
    q: "Does my code go to your servers?",
    a: "On the Pro tier, the local proxy sends chunk text and structural features to our scoring API over TLS and gets keep/cut scores back — your model requests never route through us; they leave your machine with your own credentials. On the free tier, tokens are sent to us but never stored. The one exception is the hosted Team gateway, where you explicitly choose to run the proxy in our cloud with a BYOK API key.",
  },
  {
    q: "Can I use my Claude subscription (Max plan)?",
    a: "Yes. Traffic leaves your machine with your own OAuth token, same as stock Claude Code — the rewriting happens locally. We never route subscription tokens through our cloud; the hosted gateway is BYOK API keys only.",
  },
  {
    q: "What happens when your API is down?",
    a: "The proxy fails open. Your request passes through uncompressed, so a scoring outage never blocks a session. Fail-open events are counted and visible, not silently swallowed.",
  },
  {
    q: "How are savings calculated?",
    a: "Per request, the proxy runs a count_tokens probe on the original body and compares it against what was actually billed. That measured counterfactual is the only source of savings numbers — never a modeled baseline. The ledger is kept locally, alongside your traffic.",
  },
  {
    q: "Does it train on my data?",
    a: "No by default. Telemetry is opt-in and tiered, and the default is off. You can preview exactly what would upload before opting in, and purge requests are honored.",
  },
];

export function Faq() {
  return (
    <section id="faq" aria-labelledby="faq-h" className="border-t border-line">
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <h2
          id="faq-h"
          className="text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          FAQ
        </h2>

        <div className="mt-8 max-w-3xl divide-y divide-line border-y border-line">
          {FAQ_ITEMS.map((item) => (
            <details key={item.q} className="group py-4">
              <summary className="flex cursor-pointer list-none items-baseline gap-3 text-md font-bold text-ink [&::-webkit-details-marker]:hidden">
                <span
                  aria-hidden="true"
                  className="inline-block text-phosphor transition-transform ease-parsec group-open:rotate-90"
                >
                  ❯
                </span>
                {item.q}
              </summary>
              <p className="mt-3 pl-7 text-sm text-muted">{item.a}</p>
            </details>
          ))}
        </div>

        <JsonLd data={faqLd(FAQ_ITEMS)} />
      </div>
    </section>
  );
}
