import Link from "next/link";
import { redirect } from "next/navigation";
import { ledgerSummary } from "@/lib/platform";
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

  let summary = null;
  let unreachable = false;
  try {
    summary = await ledgerSummary();
  } catch {
    unreachable = true;
  }

  return (
    <main className="mx-auto max-w-4xl p-10">
      <div className="mb-8 flex items-center justify-between">
        <h1 className="text-xl font-semibold">Savings</h1>
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
        <div className="grid grid-cols-2 gap-4 md:grid-cols-3">
          <Tile
            label="Tokens saved"
            value={fmt(summary.tokens_saved)}
            hint={`measured on ${fmt(summary.measured_rows)} of ${fmt(summary.rows_count)} requests`}
          />
          <Tile label="Billed input" value={fmt(summary.billed_input_tokens)} />
          <Tile
            label="Would-have-been input"
            value={fmt(summary.counterfactual_input_tokens)}
            hint="count_tokens counterfactual"
          />
          <Tile label="Cache reads" value={fmt(summary.billed_cache_read_tokens)} />
          <Tile label="Cache writes" value={fmt(summary.billed_cache_write_tokens)} />
          <Tile
            label="Fail-open requests"
            value={fmt(summary.fail_open_count)}
            hint="served passthrough after an error"
          />
        </div>
      )}
    </main>
  );
}
