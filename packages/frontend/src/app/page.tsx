import Link from "next/link";
import { supabaseServer } from "@/lib/supabase/server";
import { MintKey } from "@/components/mint-key";

// Front door. daseinlabs.github.io / dasein-frontend still own the marketing
// landing page — this is the app's own entry point, and the one thing it has
// to make one click away is getting a brain API key.
export const dynamic = "force-dynamic";

export default async function Home() {
  const supabase = await supabaseServer();
  const user = supabase ? (await supabase.auth.getUser()).data.user : null;

  return (
    <main className="mx-auto flex w-full max-w-4xl flex-1 flex-col justify-center p-10">
      <h1 className="text-3xl font-semibold tracking-tight">Dasein</h1>
      <p className="mt-3 max-w-lg text-sm text-neutral-500">
        Learned context compression for coding agents. Model traffic stays on
        your machine with your own credentials; the brain API only scores what
        to keep.
      </p>

      <div className="mt-8">
        {!supabase ? (
          <p className="text-sm text-neutral-500">
            Set NEXT_PUBLIC_SUPABASE_URL / NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY to
            enable key minting (see .env.example).
          </p>
        ) : user ? (
          <>
            <h2 className="mb-3 text-sm font-medium">Brain API key</h2>
            <MintKey label="Get API key" />
          </>
        ) : (
          <Link
            href="/login?next=/"
            className="inline-block rounded-md bg-neutral-900 px-5 py-2.5 text-sm font-medium text-white hover:bg-neutral-700 dark:bg-white dark:text-neutral-900 dark:hover:bg-neutral-200"
          >
            Get API key
          </Link>
        )}
      </div>

      {user && (
        <nav className="mt-10 flex items-center gap-4 text-sm">
          <Link
            href="/dashboard"
            className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white"
          >
            Usage &amp; savings
          </Link>
          <Link
            href="/account"
            className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white"
          >
            Account
          </Link>
          <form action="/auth/signout" method="post">
            <button className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
              Sign out
            </button>
          </form>
        </nav>
      )}
    </main>
  );
}
