import Link from "next/link";
import { redirect } from "next/navigation";
import { supabaseServer } from "@/lib/supabase/server";
import { MintKey } from "./mint-key";

export const dynamic = "force-dynamic";

export default async function AccountPage() {
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

  // Hosted Stripe surfaces only (§7c: no billing UI of our own). The
  // checkout link MUST carry client_reference_id=<account id> — that is the
  // join the platform webhook uses to map customer -> account.
  const checkout = process.env.NEXT_PUBLIC_STRIPE_CHECKOUT_URL;
  const portal = process.env.NEXT_PUBLIC_STRIPE_PORTAL_URL;
  const checkoutHref = checkout
    ? `${checkout}?client_reference_id=${encodeURIComponent(data.user.id)}`
    : null;

  return (
    <main className="mx-auto max-w-4xl p-10">
      <div className="mb-8 flex items-center justify-between">
        <h1 className="text-xl font-semibold">Account</h1>
        <nav className="flex items-center gap-4 text-sm">
          <Link href="/dashboard" className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
            Savings
          </Link>
          <form action="/auth/signout" method="post">
            <button className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
              Sign out
            </button>
          </form>
        </nav>
      </div>

      <div className="flex flex-col gap-10">
        <section>
          <h2 className="mb-1 text-sm font-medium">Signed in as</h2>
          <p className="text-sm text-neutral-500">
            {data.user.email} <span className="text-neutral-400">· {data.user.id}</span>
          </p>
        </section>

        <section>
          <h2 className="mb-3 text-sm font-medium">Brain API key</h2>
          <MintKey />
        </section>

        <section>
          <h2 className="mb-3 text-sm font-medium">Billing</h2>
          <div className="flex gap-3">
            {checkoutHref ? (
              <a
                href={checkoutHref}
                className="rounded-md bg-neutral-900 px-4 py-2 text-sm font-medium text-white hover:bg-neutral-700 dark:bg-white dark:text-neutral-900 dark:hover:bg-neutral-200"
              >
                Upgrade to Pro
              </a>
            ) : (
              <p className="text-sm text-neutral-500">
                Set NEXT_PUBLIC_STRIPE_CHECKOUT_URL to enable upgrades.
              </p>
            )}
            {portal && (
              <a
                href={portal}
                className="rounded-md border border-neutral-300 px-4 py-2 text-sm hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-900"
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
