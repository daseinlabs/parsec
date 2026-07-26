import { redirect } from "next/navigation";
import { supabaseServer } from "@/lib/supabase/server";
import { MintKey } from "@/components/mint-key";
import { NavLink, PageTitle, SignOut } from "@/components/brand";

export const dynamic = "force-dynamic";

export default async function AccountPage() {
  const supabase = await supabaseServer();
  if (!supabase) {
    return (
      <main className="p-10 text-sm text-muted">
        Set NEXT_PUBLIC_SUPABASE_URL / NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY to enable
        the dashboard (see .env.example).
      </main>
    );
  }
  const { data } = await supabase.auth.getUser();
  if (!data.user) redirect("/login");

  // Hosted Stripe surfaces only (§7c: no billing UI of our own). The
  // checkout link MUST carry client_reference_id=<account id> — that is the
  // join the platform webhook uses to map customer -> account.
  const checkout = process.env.NEXT_PUBLIC_STRIPE_CHECKOUT_URL;
  const portal = process.env.NEXT_PUBLIC_STRIPE_PORTAL_URL;
  const checkoutHref = checkout
    ? `${checkout}?client_reference_id=${encodeURIComponent(data.user.id)}`
    : null;

  return (
    <main className="mx-auto w-full max-w-4xl p-10">
      <div className="mb-8 flex items-center justify-between">
        <PageTitle>account</PageTitle>
        <nav className="flex items-center gap-5 text-sm">
          <NavLink href="/">savings</NavLink>
          <SignOut />
        </nav>
      </div>

      <div className="flex flex-col gap-10">
        <section>
          <h2 className="mb-1 text-xs tracking-caps text-faint uppercase">
            Signed in as
          </h2>
          <p className="text-sm text-muted">
            {data.user.email} <span className="text-faint">· {data.user.id}</span>
          </p>
        </section>

        <section>
          <h2 className="mb-3 text-xs tracking-caps text-faint uppercase">
            Brain API key
          </h2>
          <MintKey />
        </section>

        <section>
          <h2 className="mb-3 text-xs tracking-caps text-faint uppercase">Billing</h2>
          <div className="flex gap-3">
            {checkoutHref ? (
              <a
                href={checkoutHref}
                className="rounded-md bg-phosphor px-4 py-2 text-sm font-medium text-on-phosphor transition-colors duration-150 ease-parsec hover:bg-phosphor-hover active:bg-phosphor-press"
              >
                Upgrade to Pro
              </a>
            ) : (
              <p className="text-sm text-muted">
                Set NEXT_PUBLIC_STRIPE_CHECKOUT_URL to enable upgrades.
              </p>
            )}
            {portal && (
              <a
                href={portal}
                className="rounded-md border border-line-strong px-4 py-2 text-sm text-phosphor transition-colors duration-150 ease-parsec hover:bg-elevated"
              >
                Manage billing
              </a>
            )}
          </div>
        </section>
      </div>
    </main>
  );
}
