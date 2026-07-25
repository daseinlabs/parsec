"use client";

import { useState } from "react";

export function MintKey({ label = "Mint API key" }: { label?: string } = {}) {
  const [key, setKey] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function mint() {
    setBusy(true);
    setError(null);
    try {
      const resp = await fetch("/api/keys", { method: "POST" });
      const body = await resp.json();
      if (!resp.ok) throw new Error(body.error ?? `HTTP ${resp.status}`);
      setKey(body.key);
    } catch (e) {
      setError(e instanceof Error ? e.message : "failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-3">
      {key ? (
        <div className="rounded-md border border-neutral-200 p-4 dark:border-neutral-800">
          <p className="mb-2 text-xs text-neutral-500">
            Copy it now — it is shown once and stored hashed.
          </p>
          <div className="flex items-center gap-2">
            <code className="break-all text-sm">{key}</code>
            <button
              onClick={() => navigator.clipboard.writeText(key)}
              className="shrink-0 rounded-md border border-neutral-300 px-2 py-1 text-xs hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-900"
            >
              Copy
            </button>
          </div>
        </div>
      ) : (
        <button
          onClick={mint}
          disabled={busy}
          className="w-fit rounded-md bg-neutral-900 px-4 py-2 text-sm font-medium text-white hover:bg-neutral-700 disabled:opacity-50 dark:bg-white dark:text-neutral-900 dark:hover:bg-neutral-200"
        >
          {busy ? "Minting…" : label}
        </button>
      )}
      {error && <p className="text-sm text-red-600">{error}</p>}
    </div>
  );
}
