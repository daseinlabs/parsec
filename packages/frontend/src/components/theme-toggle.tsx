"use client";

// Dark ⇄ light ("Paper") switch, per brand/parsecbrandkit/tokens/theme-toggle.js:
// an explicit data-theme on <html> wins; with none set, the OS preference
// applies via the prefers-color-scheme block in globals.css. The choice is
// persisted to localStorage and re-applied pre-paint by the inline script in
// layout.tsx.
//
// System is the default: toggling *back* to the mode the OS already prefers
// clears the override entirely, so the page resumes tracking the OS instead
// of pinning a value that happens to match it today.
//
// The button renders both icons; globals.css shows the one for the mode it
// switches *to* (sun while dark, moon while light), so this component renders
// identically on server and client and nothing mismatches on hydration.

function systemTheme() {
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

function currentTheme() {
  const explicit = document.documentElement.getAttribute("data-theme");
  if (explicit === "dark" || explicit === "light") return explicit;
  return systemTheme();
}

function toggleTheme() {
  const next = currentTheme() === "dark" ? "light" : "dark";
  try {
    if (next === systemTheme()) {
      document.documentElement.removeAttribute("data-theme");
      localStorage.removeItem("theme");
    } else {
      document.documentElement.setAttribute("data-theme", next);
      localStorage.setItem("theme", next);
    }
  } catch {
    // Private mode / blocked storage — the attribute still themes this page.
    document.documentElement.setAttribute("data-theme", next);
  }
}

export function ThemeToggle() {
  return (
    <button
      onClick={toggleTheme}
      aria-label="Toggle theme"
      className="flex items-center text-muted transition-colors duration-150 ease-parsec hover:text-phosphor"
    >
      {/* sun — visible in dark mode: "switch to light" */}
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        className="theme-when-dark h-4 w-4"
        aria-hidden="true"
      >
        <circle cx="12" cy="12" r="4" />
        <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41" />
      </svg>
      {/* moon — visible in light mode: "switch to dark" */}
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
        className="theme-when-light h-4 w-4"
        aria-hidden="true"
      >
        <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
      </svg>
    </button>
  );
}
