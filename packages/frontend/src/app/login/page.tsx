"use client";

import { useState } from "react";
import { supabaseBrowser } from "@/lib/supabase/browser";
import { Lockup } from "@/components/brand";

export default function LoginPage() {
  const [error, setError] = useState<string | null>(null);

  async function signIn() {
    const supabase = supabaseBrowser();
    // ?next= is read at click time rather than via useSearchParams so this page
    // needs no Suspense boundary. The callback re-validates it.
    const next = new URLSearchParams(window.location.search).get("next");
    const callback = new URL("/auth/callback", window.location.origin);
    if (next) callback.searchParams.set("next", next);
    const { error } = await supabase.auth.signInWithOAuth({
      provider: "github",
      options: { redirectTo: callback.toString() },
    });
    if (error) setError(error.message);
  }

  return (
    <main className="flex min-h-screen items-center justify-center p-6">
      <div className="flex w-80 flex-col gap-4 rounded-lg border border-line bg-surface p-8 shadow-md">
        <Lockup />
        <p className="text-sm text-muted">
          Sign in to see your savings ledger and manage API keys.
        </p>
        <button
          onClick={signIn}
          className="rounded-md bg-phosphor px-4 py-2 text-sm font-medium text-on-phosphor transition-colors duration-150 ease-parsec hover:bg-phosphor-hover active:bg-phosphor-press"
        >
          Sign in with GitHub
        </button>
        {error && <p className="text-sm text-error">{error}</p>}
      </div>
    </main>
  );
}
