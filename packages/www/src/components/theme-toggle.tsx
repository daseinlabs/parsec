"use client";

// Light/dark switch. The current theme lives in ONE place — the data-theme
// attribute on <html>, stamped before first paint by the inline script in
// layout.tsx — so this component deliberately keeps no React state: the
// server-rendered HTML is identical for both themes (both icons present,
// CSS shows one), which means no hydration mismatch and no icon flash.
export function ThemeToggle() {
  return (
    <button
      type="button"
      aria-label="Toggle color theme"
      title="Toggle color theme"
      className="rounded-md border border-line px-2 py-1 text-muted transition-colors duration-150 ease-parsec hover:border-line-strong hover:text-phosphor"
      onClick={() => {
        const root = document.documentElement;
        const next = root.dataset.theme === "light" ? "dark" : "light";
        root.dataset.theme = next;
        try {
          localStorage.setItem("parsec-theme", next);
        } catch {
          /* private mode etc. — the toggle still works for this page */
        }
      }}
    >
      {/* ☾ shown in light mode (click → dark), ☀ in dark (click → light). */}
      <span aria-hidden className="hidden light:inline">
        ☾
      </span>
      <span aria-hidden className="hidden dark:inline">
        ☀
      </span>
    </button>
  );
}
