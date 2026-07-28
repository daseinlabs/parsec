import type { Metadata } from "next";

export const metadata: Metadata = {
  title: "Privacy",
  description:
    "An engineering description of what parsec sends where: model traffic " +
    "never routes through our cloud, paid-tier scoring sends chunk text to " +
    "our API, and telemetry is opt-in and off by default.",
  alternates: { canonical: "/privacy/" },
};

// Decorative prompt caret — phosphor, hidden from assistive tech.
function Caret() {
  return (
    <span aria-hidden="true" className="mr-2 text-phosphor">
      ❯
    </span>
  );
}

function H2({ id, children }: { id: string; children: React.ReactNode }) {
  return (
    <h2
      id={`${id}-h`}
      className="mt-12 text-lg font-bold tracking-display text-ink"
    >
      <Caret />
      {children}
    </h2>
  );
}

export default function PrivacyPage() {
  return (
    <main className="mx-auto w-full max-w-3xl px-6 py-16">
      <h1 className="text-2xl font-bold tracking-display text-ink">
        <Caret />
        Privacy
      </h1>
      <p className="mt-2 text-xs text-faint">Last updated: 2026-07-27</p>
      <p className="mt-4 text-sm text-muted">
        This page describes what the software actually does with your data, in
        plain language. No legal boilerplate, no claims the architecture
        doesn&apos;t support — when the engineering changes, this page changes
        with it.
      </p>

      <section id="short-version" aria-labelledby="short-version-h">
        <H2 id="short-version">The short version</H2>
        <ul className="mt-4 list-disc space-y-3 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">
              Model traffic never routes through our cloud.
            </strong>{" "}
            Requests to Anthropic leave from your machine, with your own
            credentials.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Your credentials stay on your machine.
            </strong>{" "}
            We never see, store, or relay your Anthropic API key or
            subscription token.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              The free tier sends us nothing.
            </strong>{" "}
            No account, no calls to our servers.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Paid-tier scoring sends chunk text to us.
            </strong>{" "}
            The local proxy sends chunk text plus structural features to our
            scoring API and gets scores back. Details below.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Telemetry is opt-in and off by default.
            </strong>{" "}
            The product is fully functional with it off.
          </li>
        </ul>
      </section>

      <section id="what-runs-where" aria-labelledby="what-runs-where-h">
        <H2 id="what-runs-where">What runs where</H2>
        <p className="mt-4 text-base text-muted">
          <strong className="font-semibold text-ink">
            Data plane — your machine.
          </strong>{" "}
          The proxy runs locally. Every model request goes straight from your
          machine to Anthropic, authenticated with your own key or
          subscription. Subscription OAuth tokens are never routed through our
          cloud, on any tier.
        </p>
        <p className="mt-3 text-base text-muted">
          <strong className="font-semibold text-ink">
            Control plane — our cloud.
          </strong>{" "}
          Accounts, entitlements, the savings ledger, and the scoring API. The
          learned model that does the scoring lives only here; its weights
          never leave our servers, and your model traffic never enters them.
        </p>
      </section>

      <section id="paid-wire" aria-labelledby="paid-wire-h">
        <H2 id="paid-wire">What crosses the wire on the paid tier</H2>
        <p className="mt-4 text-base text-muted">
          Each turn, the proxy chunks your context locally and sends the chunk
          text, plus structural features it computed, to our scoring API. The
          API embeds the text server-side, scores each chunk, and returns
          scores. The keep/cut decision is applied on your machine — the
          scoring API is never told which chunks were dropped.
        </p>
        <p className="mt-3 text-base text-muted">
          To be explicit:{" "}
          <strong className="font-semibold text-ink">
            chunk text does leave your machine on the paid tier.
          </strong>{" "}
          An earlier design sent only embedding vectors; as of 2026-07-20 the
          embedding step runs server-side, so text crosses the wire. We say
          that plainly rather than keep a claim the architecture no longer
          supports.
        </p>
      </section>

      <section id="ledger" aria-labelledby="ledger-h">
        <H2 id="ledger">The savings ledger</H2>
        <p className="mt-4 text-base text-muted">
          Every request writes one row: token counts (billed input, output,
          cache reads and writes, and the measured counterfactual), the model
          id, and opaque conversation and session ids.{" "}
          <strong className="font-semibold text-ink">
            No message text, ever
          </strong>{" "}
          — the row schema has no field that could carry it.
        </p>
        <p className="mt-3 text-base text-muted">
          Rows are written locally first, to{" "}
          <code className="text-ink">~/.parsec/ledger.jsonl</code> — that file
          is what the savings report and status line read. Rows ship to your
          account only after you set an API key; clearing the key stops
          shipping.
        </p>
      </section>

      <section id="telemetry" aria-labelledby="telemetry-h">
        <H2 id="telemetry">Training telemetry (opt-in)</H2>
        <p className="mt-4 text-base text-muted">
          The curator improves on traces users choose to share. Sharing is
          tiered, granular, and off by default:
        </p>
        <ul className="mt-4 list-disc space-y-3 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">Tier 0 — off.</strong>{" "}
            The default. The product is fully functional here; consent by
            degradation is not consent.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Tier 1 — metrics only.
            </strong>{" "}
            Tokens saved, cut percentage, cache ratios, outcome signal, harness
            version, latency. No content, no file paths, no prompts.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Tier 2 — featurized traces.
            </strong>{" "}
            Chunk embeddings plus trace structure, tool names, and token
            counts. No source code, no prompts — and, honestly: a vector is a
            mitigation, not anonymity. Embedding inversion exists.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Tier 3 — full traces.
            </strong>{" "}
            Design partners only, governed by contract.
          </li>
        </ul>
        <p className="mt-4 text-base text-muted">How consent works:</p>
        <ul className="mt-3 list-disc space-y-3 pl-5 text-base text-muted marker:text-faint">
          <li>
            We ask after your first savings report — never at install.
          </li>
          <li>
            A preview command dumps the exact bytes that would upload, locally,
            before anything ships. Before featurization, a local scrub masks
            secrets and hashes file paths.
          </li>
          <li>
            A persistent status-line indicator shows whenever sharing is on;
            turning it off stops uploads instantly.
          </li>
          <li>
            A tier or schema change re-prompts. Consent never silently
            expands.
          </li>
          <li>
            Purge deletes your data from the corpus and excludes it from all
            future training runs. Stated plainly: erasing its influence from
            an already-trained checkpoint requires a retrain.
          </li>
        </ul>
      </section>

      <section id="accounts" aria-labelledby="accounts-h">
        <H2 id="accounts">Accounts &amp; payments</H2>
        <p className="mt-4 text-base text-muted">
          We build on managed services rather than holding more than we must.
          The subprocessors:
        </p>
        <ul className="mt-3 list-disc space-y-3 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">Supabase</strong> —
            Authentication and the database: accounts, entitlements, ledger
            rows, and the telemetry consent registry.
          </li>
          <li>
            <strong className="font-semibold text-ink">Stripe</strong> —
            Billing. Card details go to Stripe and never touch our servers.
          </li>
          <li>
            <strong className="font-semibold text-ink">Google Cloud</strong> —
            Hosting for the control plane.
          </li>
        </ul>
      </section>

      <section id="contact" aria-labelledby="contact-h">
        <H2 id="contact">Contact</H2>
        <p className="mt-4 text-base text-muted">
          Questions about any of this:{" "}
          <a
            href="mailto:hello@dasein.rocks"
            className="text-info underline underline-offset-2 hover:text-phosphor"
          >
            hello@dasein.rocks
          </a>
          .
        </p>
      </section>

      <hr className="mt-14 border-line" aria-hidden="true" />
      <p className="mt-4 text-xs text-faint">
        This page is an engineering description of data flows, maintained with
        the code that implements them.
      </p>
    </main>
  );
}
