import Link from "next/link";
import { redirect } from "next/navigation";
import { ledgerSummary, ledgerUsage } from "@/lib/platform";
import type { LedgerSummary, LedgerUsage } from "@/lib/platform";
import { supabaseServer } from "@/lib/supabase/server";

export const dynamic = "force-dynamic";

function Tile({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="rounded-lg border border-neutral-200 p-5 dark:border-neutral-800">
      <div className="text-sm text-neutral-500">{label}</div>
      <div className="mt-1 text-2xl font-semibold tabular-nums">{value}</div>
      {hint && <div className="mt-1 text-xs text-neutral-400">{hint}</div>}
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
      <h2 className="mb-3 text-sm font-medium text-neutral-500">By model</h2>
      <div className="overflow-x-auto rounded-lg border border-neutral-200 dark:border-neutral-800">
        <table className="w-full text-sm tabular-nums">
          <thead className="text-left text-xs text-neutral-500">
            <tr className="border-b border-neutral-200 dark:border-neutral-800">
              <th className="px-4 py-2 font-medium">Model</th>
              <th className="px-4 py-2 text-right font-medium">Requests</th>
              <th className="px-4 py-2 text-right font-medium">Tokens saved</th>
              <th className="px-4 py-2 text-right font-medium">Billed input</th>
              <th className="px-4 py-2 text-right font-medium">Cost</th>
            </tr>
          </thead>
          <tbody>
            {summary.by_model.map((m) => (
              <tr
                key={m.model ?? "unknown"}
                className="border-b border-neutral-100 last:border-0 dark:border-neutral-900"
              >
                <td className="px-4 py-2 font-mono text-xs">{m.model ?? "unknown"}</td>
                <td className="px-4 py-2 text-right">{fmt(m.rows_count)}</td>
                <td className="px-4 py-2 text-right">{fmt(m.tokens_saved)}</td>
                <td className="px-4 py-2 text-right">{fmt(m.billed_input_tokens)}</td>
                <td className="px-4 py-2 text-right">
                  {m.cost_usd === null ? (
                    <span className="text-neutral-400">—</span>
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
      <h2 className="mb-3 text-sm font-medium text-neutral-500">
        Usage — last {usage.window_days} days
      </h2>
      <ul className="space-y-1">
        {usage.days.map((d) => (
          <li key={d.date} className="flex items-center gap-3 text-xs tabular-nums">
            <span className="w-20 shrink-0 text-neutral-500">{d.date}</span>
            <span className="flex-1">
              <span
                className="block h-3 rounded-sm bg-neutral-300 dark:bg-neutral-700"
                style={{ width: `${Math.max((d.cost_usd / maxCost) * 100, 2)}%` }}
              />
            </span>
            <span className="w-16 shrink-0 text-right">{usd(d.cost_usd)}</span>
            <span className="w-24 shrink-0 text-right text-neutral-400">
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
      <main className="p-10 text-sm text-neutral-500">
        Set NEXT_PUBLIC_SUPABASE_URL / NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY to enable
        the dashboard (see .env.example).
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
    <main className="mx-auto max-w-4xl p-10">
      <div className="mb-8 flex items-center justify-between">
        <h1 className="text-xl font-semibold">Usage &amp; savings</h1>
        <nav className="flex items-center gap-4 text-sm">
          <Link href="/account" className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
            Account
          </Link>
          <form action="/auth/signout" method="post">
            <button className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
              Sign out
            </button>
          </form>
        </nav>
      </div>

      {unreachable && (
        <p className="text-sm text-neutral-500">
          The platform API is unreachable — check PLATFORM_URL.
        </p>
      )}
      {summary && summary.rows_count === 0 && (
        <p className="text-sm text-neutral-500">
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
            />
          </div>
          <ByModel summary={summary} />
          {usage && <DailyUsage usage={usage} />}
        </>
      )}
    </main>
  );
}
