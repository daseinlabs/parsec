"use client";

import { useEffect, useRef, useState } from "react";

// idle → copied (confirmed write) | failed (clipboard refused; key is selected
// instead so the user can still hit ⌘C).
type CopyState = "idle" | "copied" | "failed";

export function MintKey({ label = "Mint API key" }: { label?: string } = {}) {
  const [key, setKey] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState<CopyState>("idle");
  const codeRef = useRef<HTMLElement>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => () => {
    if (timer.current) clearTimeout(timer.current);
  }, []);

  function flash(state: CopyState) {
    setCopied(state);
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied("idle"), 2500);
  }

  // Fallback for when the clipboard write is refused: put the key in the
  // selection so ⌘C/Ctrl-C still works.
  function selectKey() {
    const node = codeRef.current;
    if (!node) return;
    const range = document.createRange();
    range.selectNodeContents(node);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
  }

  async function copy() {
    if (!key) return;
    try {
      // navigator.clipboard is undefined outside a secure context (fine on
      // localhost, absent when the dashboard is opened over plain http on a
      // LAN address), and writeText rejects if the document is not focused.
      // Both used to fail silently — the promise was never awaited.
      if (!navigator.clipboard) throw new Error("clipboard unavailable");
      await navigator.clipboard.writeText(key);
      flash("copied");
    } catch {
      selectKey();
      flash("failed");
    }
  }

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
        <div className="rounded-md border border-line-strong bg-surface p-4">
          <p className="mb-2 text-xs text-warning">
            Copy it now — it is shown once and stored hashed.
          </p>
          <div className="flex items-center gap-2">
            <code
              ref={codeRef}
              onClick={selectKey}
              className="cursor-pointer break-all text-sm text-phosphor"
            >
              {key}
            </code>
            <button
              onClick={copy}
              // The confirmation is a fill, not a colour tweak — a ghost button
              // swapping one phosphor word for another is easy to miss.
              className={`shrink-0 rounded-md border px-2 py-1 text-xs transition-colors duration-150 ease-parsec ${
                copied === "copied"
                  ? "border-phosphor bg-phosphor text-on-phosphor"
                  : copied === "failed"
                    ? "border-warning text-warning"
                    : "border-line-strong text-phosphor hover:bg-elevated"
              }`}
            >
              {copied === "copied"
                ? "✓ Copied"
                : copied === "failed"
                  ? "Press ⌘C"
                  : "Copy"}
            </button>
          </div>
          {/* Button text alone is not reliably announced when it changes. */}
          <p role="status" aria-live="polite" className="sr-only">
            {copied === "copied"
              ? "API key copied to clipboard"
              : copied === "failed"
                ? "Could not copy — the key is selected, press Command-C"
                : ""}
          </p>
        </div>
      ) : (
        <button
          onClick={mint}
          disabled={busy}
          className="w-fit rounded-md bg-phosphor px-4 py-2 text-sm font-medium text-on-phosphor transition-colors duration-150 ease-parsec hover:bg-phosphor-hover active:bg-phosphor-press disabled:opacity-50"
        >
          {busy ? "Minting…" : label}
        </button>
      )}
      {error && <p className="text-sm text-error">{error}</p>}
    </div>
  );
}
