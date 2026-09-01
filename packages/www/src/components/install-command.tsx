"use client";

import { useRef, useState, useSyncExternalStore } from "react";
import { INSTALL_ONE_LINERS } from "@/lib/site";
import { trackInstallCopy } from "@/lib/analytics";

// The universal one-liner with a macOS/Linux ⇄ Windows toggle. Client island
// because the default tab follows the visitor's OS: the static export always
// renders the macOS/Linux variant, and Windows machines flip over right after
// hydration — no user-agent sniffing at build time. A click is a sticky
// override on top of the detected default.
type Os = keyof typeof INSTALL_ONE_LINERS;

const TABS: { os: Os; label: string }[] = [
  { os: "unix", label: "macOS / Linux" },
  { os: "windows", label: "Windows" },
];

const noopSubscribe = () => () => {};
const detectOs = (): Os =>
  /windows/i.test(navigator.userAgent) ? "windows" : "unix";

// `tool` pins the installer to one agent (install.sh's `-s -- <tool>` arg) on
// the per-agent landing pages. Unix only: install.ps1 takes no selector, so
// the Windows line stays the auto-detecting one-liner.
export function InstallCommand({
  tool,
}: {
  tool?: "claude" | "codex" | "opencode";
}) {
  const detected = useSyncExternalStore(
    noopSubscribe,
    detectOs,
    (): Os => "unix",
  );
  const [override, setOs] = useState<Os | null>(null);
  const os = override ?? detected;
  const command =
    tool && os === "unix"
      ? `${INSTALL_ONE_LINERS.unix} -s -- ${tool}`
      : INSTALL_ONE_LINERS[os];

  const [copied, setCopied] = useState(false);
  const copiedTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(command);
    } catch {
      return; // clipboard unavailable (permissions, http) — select-all still works
    }
    trackInstallCopy({ os, method: "button", tool });
    setCopied(true);
    clearTimeout(copiedTimer.current);
    copiedTimer.current = setTimeout(() => setCopied(false), 2000);
  };

  return (
    <div className="w-fit max-w-full">
      <div className="flex gap-4">
        {TABS.map((tab) => (
          <button
            key={tab.os}
            type="button"
            aria-pressed={os === tab.os}
            onClick={() => setOs(tab.os)}
            className={`text-xs tracking-caps uppercase transition-colors duration-150 ease-parsec ${
              os === tab.os
                ? "text-phosphor"
                : "text-faint hover:text-phosphor"
            }`}
          >
            {tab.label}
          </button>
        ))}
      </div>
      <div className="mt-2 flex max-w-full items-center gap-3 rounded-md border border-line bg-surface px-4 py-3">
        <div className="flex min-w-0 flex-1 items-center gap-3 overflow-x-auto">
          <span aria-hidden className="select-none text-phosphor">
            {os === "windows" ? ">" : "❯"}
          </span>
          <code
            className="select-all text-sm whitespace-nowrap text-ink"
            // Fires on a manual select-and-copy (⌘C); the copy button uses
            // clipboard.writeText, which does not dispatch a copy event, so
            // the two paths never double-count.
            onCopy={() => trackInstallCopy({ os, method: "keyboard", tool })}
          >
            {command}
          </code>
        </div>
        <button
          type="button"
          onClick={copy}
          aria-label={copied ? "Copied" : "Copy install command to clipboard"}
          className={`shrink-0 transition-colors duration-150 ease-parsec ${
            copied ? "text-success" : "text-faint hover:text-phosphor"
          }`}
        >
          {copied ? (
            <svg
              aria-hidden
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="h-4 w-4"
            >
              <path d="M20 6 9 17l-5-5" />
            </svg>
          ) : (
            <svg
              aria-hidden
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="h-4 w-4"
            >
              <rect x="8" y="2" width="8" height="4" rx="1" />
              <path d="M8 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2h-2" />
            </svg>
          )}
        </button>
      </div>
    </div>
  );
}
