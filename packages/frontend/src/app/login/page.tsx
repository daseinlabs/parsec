"use client";

import { useState } from "react";
import { supabaseBrowser } from "@/lib/supabase/browser";

export default function LoginPage() {
  const [error, setError] = useState<string | null>(null);

  async function signIn() {
    const supabase = supabaseBrowser();
    const { error } = await supabase.auth.signInWithOAuth({
      provider: "github",
      options: { redirectTo: `${window.location.origin}/auth/callback` },
    });
    if (error) setError(error.message);
  }

  return (
    <main className="flex min-h-screen items-center justify-center">
      <div className="flex w-80 flex-col gap-4 rounded-lg border border-neutral-200 p-8 dark:border-neutral-800">
        <h1 className="text-xl font-semibold">Dasein</h1>
        <p className="text-sm text-neutral-500">
          Sign in to see your savings ledger and manage API keys.
        </p>
        <button
          onClick={signIn}
          className="rounded-md bg-neutral-900 px-4 py-2 text-sm font-medium text-white hover:bg-neutral-700 dark:bg-white dark:text-neutral-900 dark:hover:bg-neutral-200"
        >
          Sign in with GitHub
        </button>
        {error && <p className="text-sm text-red-600">{error}</p>}
      </div>
    </main>
  );
}
