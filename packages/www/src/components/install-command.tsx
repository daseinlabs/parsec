"use client";

import { useState, useSyncExternalStore } from "react";
import { INSTALL_ONE_LINERS } from "@/lib/site";

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

export function InstallCommand() {
  const detected = useSyncExternalStore(
    noopSubscribe,
    detectOs,
    (): Os => "unix",
  );
  const [override, setOs] = useState<Os | null>(null);
  const os = override ?? detected;

  return (
    <div className="max-w-full">
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
      <div className="mt-2 flex max-w-full items-center gap-3 overflow-x-auto rounded-md border border-line bg-surface px-4 py-3">
        <span aria-hidden className="select-none text-phosphor">
          {os === "windows" ? ">" : "❯"}
        </span>
        <code className="select-all text-sm whitespace-nowrap text-ink">
          {INSTALL_ONE_LINERS[os]}
        </code>
      </div>
    </div>
  );
}
