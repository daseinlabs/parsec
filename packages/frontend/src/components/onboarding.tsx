import { MintKey } from "@/components/mint-key";

// First-run empty state. An account with zero ledger rows is almost always an
// account that has never connected the plugin, so instead of a bare "no data"
// line the dashboard walks the whole path to a first measured request. The
// steps mirror the canonical flow in docs/plugin-user-messaging.md and the
// /parsec:key skill — if those change, this copy must change with them.

function Command({ children }: { children: string }) {
  return (
    <pre className="overflow-x-auto rounded-md bg-elevated px-3 py-2 text-xs text-ink">
      <code>{children}</code>
    </pre>
  );
}

function Step({
  n,
  title,
  children,
}: {
  n: number;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <li className="flex gap-4">
      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-line-strong text-xs tabular-nums text-phosphor">
        {n}
      </span>
      <div className="flex min-w-0 flex-1 flex-col gap-2">
        <h3 className="text-sm font-medium text-ink">{title}</h3>
        {children}
      </div>
    </li>
  );
}

export function Onboarding() {
  return (
    <div className="rounded-lg border border-line bg-surface p-6">
      <p className="mb-6 text-sm text-muted">
        Nothing measured yet — this account has not reported a request. Three
        steps from here to your first savings row:
      </p>

      <ol className="flex flex-col gap-6">
        <Step n={1} title="Install the plugin">
          <Command>
            {"claude plugin marketplace add daseinlabs/claude-plugins\nclaude plugin install parsec@parsec-marketplace"}
          </Command>
        </Step>

        <Step n={2} title="Restart Claude Code">
          <p className="text-xs text-muted">
            Setup runs automatically on the first session: it points Claude
            Code at a local parsec proxy on your machine. Routing is read at
            session launch, so open sessions need a restart. Your model
            traffic never touches our cloud.
          </p>
        </Step>

        <Step n={3} title="Generate an API key and connect it">
          <p className="text-xs text-muted">
            Shown once, stored hashed. Paste it into any Claude Code session
            with <code className="text-phosphor">/parsec:key</code> — savings
            report here from the next request, no restart needed.
          </p>
          <MintKey label="Generate API key" />
        </Step>
      </ol>
    </div>
  );
}
