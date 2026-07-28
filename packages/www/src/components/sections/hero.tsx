import { Mark } from "@/components/brand";
import { INSTALL_COMMANDS } from "@/lib/site";

// Top of the landing page — the one place the page glows: the mark carries
// .glow-mark. Every other section stays flat. Server component: the install
// command is copyable via `select-all` (one click selects the whole line),
// no JS needed.
export function Hero() {
  return (
    <section aria-label="parsec">
      <div className="mx-auto w-full max-w-6xl px-6 py-24 sm:py-32">
        {/* text-logo, not text-phosphor: the mark keeps the bright brand
            green on light while interactive greens darken to emerald. */}
        <Mark className="h-16 w-auto text-logo glow-mark sm:h-20" />

        <h1 className="mt-10 text-2xl font-extrabold tracking-display text-ink sm:text-3xl">
          Double your Claude Code limit, while improving accuracy
        </h1>

        <div className="mt-10 flex flex-col items-start gap-5 sm:flex-row sm:items-center">
          <div className="flex max-w-full items-center gap-3 overflow-x-auto rounded-md border border-line bg-surface px-4 py-3">
            <span aria-hidden className="select-none text-phosphor">
              ❯
            </span>
            <code className="select-all text-sm whitespace-nowrap text-ink">
              {INSTALL_COMMANDS[0]}
            </code>
          </div>
          <a
            href="#how"
            className="text-sm text-muted transition-colors duration-150 ease-parsec hover:text-phosphor"
          >
            Read how it works <span aria-hidden>↓</span>
          </a>
        </div>
      </div>
    </section>
  );
}
