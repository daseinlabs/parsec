import { INSTALL_COMMANDS, SITE } from "@/lib/site";

// Closes the page: the install pair, the no-account note, and two exits
// (dashboard, source). Commands are copyable via `select-all` — one click
// selects the whole line, no JS shipped.
export function Cta() {
  return (
    <section
      id="get-started"
      aria-labelledby="get-started-h"
      className="border-t border-line"
    >
      <div className="mx-auto w-full max-w-6xl px-6 py-20">
        <p className="text-xs tracking-caps text-faint uppercase">
          get started
        </p>
        <h2
          id="get-started-h"
          className="mt-2 text-xl font-bold tracking-display text-ink sm:text-2xl"
        >
          Two commands from here
        </h2>

        <div className="mt-8 flex flex-col gap-2 overflow-x-auto rounded-md border border-line bg-surface p-4">
          {INSTALL_COMMANDS.map((command) => (
            <p key={command} className="whitespace-nowrap text-sm">
              <span aria-hidden className="select-none text-phosphor">
                ❯{" "}
              </span>
              <code className="select-all text-ink">{command}</code>
            </p>
          ))}
        </div>

        <div className="mt-8 flex flex-wrap items-center gap-6">
          <a
            href={SITE.links.app}
            rel="noopener"
            className="rounded-md border border-line-strong px-5 py-2.5 text-sm font-bold text-phosphor transition-colors duration-150 ease-parsec hover:bg-phosphor hover:text-on-phosphor"
          >
            Open the dashboard
          </a>
          <a
            href={SITE.links.github}
            rel="noopener"
            className="text-sm text-info underline underline-offset-2 transition-colors duration-150 ease-parsec hover:text-phosphor"
          >
            Source on GitHub
          </a>
        </div>
      </div>
    </section>
  );
}
