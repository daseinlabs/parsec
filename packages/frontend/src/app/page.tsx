import { redirect } from "next/navigation";
import { ledgerSummary, ledgerUsage, PlatformHttpError } from "@/lib/platform";
import type { LedgerSummary, LedgerUsage } from "@/lib/platform";
import { supabaseServer } from "@/lib/supabase/server";
import { Lockup, NavLink, SignOut } from "@/components/brand";
import { Onboarding } from "@/components/onboarding";
import { ThemeToggle } from "@/components/theme-toggle";

// The app is the dashboard. Signed out, you get /login; signed in, you get the
// savings ledger. There is no separate front door — the marketing landing page
// lives outside this app, and a logged-in user has no use for a splash screen.
export const dynamic = "force-dynamic";

// Colour carries meaning here, so the glow stays rare. Phosphor's glow belongs
// to exactly one number — the win, dollars saved — and that number is the
// <CostSaved> headline, not a tile; hence no "win" tone here. Spend and volume
// are neutral facts in ink; fail-open is a degradation signal and goes warning
// once it is non-zero. Everything glowing green would read as "all of this is
// good news" — spend and fail-opens are not.
type Tone = "neutral" | "warn";

const TONE: Record<Tone, string> = {
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
// Sub-cent sums round to "$0.00", which reads as "this saved nothing" on a fresh
// account. Give small dollar amounts the precision to show they are non-zero.
const usdPrecise = (n: number) =>
  Intl.NumberFormat("en", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: n !== 0 && Math.abs(n) < 1 ? 4 : 2,
  }).format(n);

// The headline. It gets the page's one glow (globals.css: "the number that is
// the story") and the largest type — everything else on the page exists to
// explain it.
function CostSaved({ summary }: { summary: LedgerSummary }) {
  return (
    <div className="rounded-lg border border-line bg-surface p-6">
      <div className="text-xs tracking-caps text-faint uppercase">Total cost saved</div>
      <div className="mt-2 text-2xl font-bold tabular-nums text-phosphor glow">
        {usdPrecise(summary.cost_saved_usd)}
      </div>
      <div className="mt-2 text-xs text-faint">
        {fmt(summary.tokens_saved)} input tokens not sent, valued at the rate you
        actually paid · measured on {fmt(summary.measured_rows)} of{" "}
        {fmt(summary.rows_count)} requests
      </div>
    </div>
  );
}

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
              <th className="px-4 py-2 text-right font-medium">Cost saved</th>
              <th className="px-4 py-2 text-right font-medium">Tokens saved</th>
              <th className="px-4 py-2 text-right font-medium">Billed input</th>
              <th className="px-4 py-2 text-right font-medium">Cache read</th>
              <th className="px-4 py-2 text-right font-medium">Cache write</th>
              <th className="px-4 py-2 text-right font-medium">Cost</th>
            </tr>
          </thead>
          <tbody>
            {summary.by_model.map((m) => (
              <tr key={m.model ?? "unknown"} className="border-b border-line last:border-0">
                <td className="px-4 py-2 text-xs text-muted">{m.model ?? "unknown"}</td>
                <td className="px-4 py-2 text-right">{fmt(m.rows_count)}</td>
                {/* Unpriced model ⇒ no dollar figure at all, not a zero. */}
                <td
                  className={`px-4 py-2 text-right ${
                    m.cost_saved_usd ? "text-phosphor" : "text-faint"
                  }`}
                >
                  {m.cost_saved_usd === null ? "—" : usdPrecise(m.cost_saved_usd)}
                </td>
                <td
                  className={`px-4 py-2 text-right ${
                    m.tokens_saved > 0 ? "text-phosphor" : "text-faint"
                  }`}
                >
                  {fmt(m.tokens_saved)}
                </td>
                <td className="px-4 py-2 text-right">{fmt(m.billed_input_tokens)}</td>
                <td className="px-4 py-2 text-right">
                  {fmt(m.billed_cache_read_tokens)}
                </td>
                <td className="px-4 py-2 text-right">
                  {fmt(m.billed_cache_write_tokens)}
                </td>
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
            {/* Same currency as the headline — a token count here would make the
                reader convert units mid-page. */}
            <span className="w-24 shrink-0 text-right text-muted">
              {usdPrecise(d.cost_saved_usd)} saved
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

/** Name the actual fault. Every branch here is a different on-call action. */
function platformFailureMessage(e: unknown): string {
  if (!(e instanceof PlatformHttpError)) {
    // Never got an HTTP status back at all — DNS, TLS, refused connection.
    return "The platform API is unreachable — check PLATFORM_URL.";
  }
  if (e.status === 401 || e.status === 403) {
    return `The platform API rejected this session (${e.status}). It is reachable, so this is an auth fault — check that SUPABASE_JWKS_URL (or SUPABASE_JWT_SECRET) is set on the platform service.`;
  }
  return `The platform API returned ${e.status}.`;
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
  // Why not a single "unreachable" flag: a 401 and a refused connection are
  // different outages with different fixes, and collapsing them sent us hunting
  // PLATFORM_URL while the real fault was the platform's JWT config.
  let failure: string | null = null;
  try {
    [summary, usage] = await Promise.all([ledgerSummary(), ledgerUsage(30)]);
  } catch (e) {
    failure = platformFailureMessage(e);
  }

  return (
    <main className="mx-auto w-full max-w-4xl p-10">
      {/* Root page, so it carries the mark — this is the app's front page now. */}
      <div className="mb-8 flex items-center justify-between">
        <Lockup />
        <nav className="flex items-center gap-5 text-sm">
          <ThemeToggle />
          <NavLink href="/account">account</NavLink>
          <SignOut />
        </nav>
      </div>

      <h1 className="mb-4 flex items-baseline gap-2 text-lg font-bold tracking-display">
        <span className="text-phosphor glow">❯</span>
        usage &amp; savings
      </h1>

      {failure && <p className="text-sm text-error">{failure}</p>}
      {summary && summary.rows_count === 0 && <Onboarding />}
      {summary && summary.rows_count > 0 && (
        <>
          {/* Dollars saved leads; raw token totals live in the by-model table
              below rather than competing with the headline up here. */}
          <CostSaved summary={summary} />
          <div className="mt-4 grid grid-cols-2 gap-4">
            <Tile
              label="Spend"
              value={usd(summary.cost_usd)}
              hint="billed tokens at list price"
            />
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
