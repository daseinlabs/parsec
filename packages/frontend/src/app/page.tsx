import { redirect } from "next/navigation";
import { ledgerSummary, ledgerUsage } from "@/lib/platform";
import type { LedgerSummary, LedgerUsage } from "@/lib/platform";
import { supabaseServer } from "@/lib/supabase/server";
import { Lockup, NavLink, SignOut } from "@/components/brand";

// The app is the dashboard. Signed out, you get /login; signed in, you get the
// savings ledger. There is no separate front door — the marketing landing page
// lives outside this app, and a logged-in user has no use for a splash screen.
export const dynamic = "force-dynamic";

// Colour carries meaning here, so the glow stays rare. Phosphor is reserved for
// the number that IS the win (tokens saved); spend and volume are neutral facts
// in ink; fail-open is a degradation signal and goes warning once it is non-zero.
// Everything glowing green would read as "all of this is good news" — spend and
// fail-opens are not.
type Tone = "win" | "neutral" | "warn";

const TONE: Record<Tone, string> = {
  win: "text-phosphor glow",
  neutral: "text-ink",
  warn: "text-warning",
};

function Tile({
  label,
  value,
  hint,
  tone = "neutral",
}: {
  label: string;
  value: string;
  hint?: string;
  tone?: Tone;
}) {
  return (
    <div className="rounded-lg border border-line bg-surface p-5">
      <div className="text-xs tracking-caps text-faint uppercase">{label}</div>
      <div className={`mt-2 text-lg font-bold tabular-nums ${TONE[tone]}`}>
        {value}
      </div>
      {hint && <div className="mt-1 text-xs text-faint">{hint}</div>}
    </div>
  );
}

const fmt = (n: number) => Intl.NumberFormat("en", { notation: "compact" }).format(n);
const usd = (n: number) =>
  Intl.NumberFormat("en", { style: "currency", currency: "USD" }).format(n);

function ByModel({ summary }: { summary: LedgerSummary }) {
  if (summary.by_model.length === 0) return null;
  return (
    <section className="mt-10">
      <h2 className="mb-3 text-xs tracking-caps text-faint uppercase">By model</h2>
      <div className="overflow-x-auto rounded-lg border border-line bg-surface">
        <table className="w-full text-sm tabular-nums">
          <thead className="text-left text-xs text-faint">
            <tr className="border-b border-line">
              <th className="px-4 py-2 font-medium">Model</th>
              <th className="px-4 py-2 text-right font-medium">Requests</th>
              <th className="px-4 py-2 text-right font-medium">Tokens saved</th>
              <th className="px-4 py-2 text-right font-medium">Billed input</th>
              <th className="px-4 py-2 text-right font-medium">Cost</th>
            </tr>
          </thead>
          <tbody>
            {summary.by_model.map((m) => (
              <tr key={m.model ?? "unknown"} className="border-b border-line last:border-0">
                <td className="px-4 py-2 text-xs text-muted">{m.model ?? "unknown"}</td>
                <td className="px-4 py-2 text-right">{fmt(m.rows_count)}</td>
                <td
                  className={`px-4 py-2 text-right ${
                    m.tokens_saved > 0 ? "text-phosphor" : "text-faint"
                  }`}
                >
                  {fmt(m.tokens_saved)}
                </td>
                <td className="px-4 py-2 text-right">{fmt(m.billed_input_tokens)}</td>
                <td className="px-4 py-2 text-right">
                  {m.cost_usd === null ? (
                    <span className="text-faint">—</span>
                  ) : (
                    usd(m.cost_usd)
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function DailyUsage({ usage }: { usage: LedgerUsage }) {
  if (usage.days.length === 0) return null;
  const maxCost = Math.max(...usage.days.map((d) => d.cost_usd), 0.000001);
  return (
    <section className="mt-10">
      <h2 className="mb-3 text-xs tracking-caps text-faint uppercase">
        Usage — last {usage.window_days} days
      </h2>
      <ul className="space-y-1">
        {usage.days.map((d) => (
          <li key={d.date} className="flex items-center gap-3 text-xs tabular-nums">
            <span className="w-20 shrink-0 text-faint">{d.date}</span>
            {/* phosphor fill on the token-line track (BRANDING.md §5) */}
            <span className="flex-1 rounded-sm bg-line">
              <span
                className="block h-3 rounded-sm bg-phosphor"
                style={{ width: `${Math.max((d.cost_usd / maxCost) * 100, 2)}%` }}
              />
            </span>
            <span className="w-16 shrink-0 text-right">{usd(d.cost_usd)}</span>
            <span className="w-24 shrink-0 text-right text-muted">
              {fmt(d.tokens_saved)} saved
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

export default async function DashboardPage() {
  const supabase = await supabaseServer();
  if (!supabase) {
    return (
      <main className="mx-auto w-full max-w-4xl p-10">
        <Lockup />
        <p className="mt-6 text-sm text-muted">
          Set NEXT_PUBLIC_SUPABASE_URL / NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY to enable
          the dashboard (see .env.example).
        </p>
      </main>
    );
  }
  const { data } = await supabase.auth.getUser();
  if (!data.user) redirect("/login");

  let summary: LedgerSummary | null = null;
  let usage: LedgerUsage | null = null;
  let unreachable = false;
  try {
    [summary, usage] = await Promise.all([ledgerSummary(), ledgerUsage(30)]);
  } catch {
    unreachable = true;
  }

  return (
    <main className="mx-auto w-full max-w-4xl p-10">
      {/* Root page, so it carries the mark — this is the app's front page now. */}
      <div className="mb-8 flex items-center justify-between">
        <Lockup />
        <nav className="flex items-center gap-5 text-sm">
          <NavLink href="/account">account</NavLink>
          <SignOut />
        </nav>
      </div>

      <h1 className="mb-4 flex items-baseline gap-2 text-lg font-bold tracking-display">
        <span className="text-phosphor glow">❯</span>
        usage &amp; savings
      </h1>

      {unreachable && (
        <p className="text-sm text-error">
          The platform API is unreachable — check PLATFORM_URL.
        </p>
      )}
      {summary && summary.rows_count === 0 && (
        <p className="text-sm text-muted">
          No ledger rows yet. Mint an API key under Account and point your
          proxy at the platform to start reporting.
        </p>
      )}
      {summary && summary.rows_count > 0 && (
        <>
          <div className="grid grid-cols-2 gap-4 md:grid-cols-3">
            <Tile
              label="Tokens saved"
              value={fmt(summary.tokens_saved)}
              hint={`measured on ${fmt(summary.measured_rows)} of ${fmt(summary.rows_count)} requests`}
              tone="win"
            />
            <Tile
              label="Spend"
              value={usd(summary.cost_usd)}
              hint="billed tokens at list price"
            />
            <Tile label="Billed input" value={fmt(summary.billed_input_tokens)} />
            <Tile
              label="Would-have-been input"
              value={fmt(summary.counterfactual_input_tokens)}
              hint="count_tokens counterfactual"
            />
            <Tile label="Cache reads" value={fmt(summary.billed_cache_read_tokens)} />
            <Tile
              label="Fail-open requests"
              value={fmt(summary.fail_open_count)}
              hint="served passthrough after an error"
              tone={summary.fail_open_count > 0 ? "warn" : "neutral"}
            />
          </div>
          <ByModel summary={summary} />
          {usage && <DailyUsage usage={usage} />}
        </>
      )}
    </main>
  );
}
