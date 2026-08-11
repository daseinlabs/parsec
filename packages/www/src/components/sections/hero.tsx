import { Mark } from "@/components/brand";
import { SavingsStat } from "@/components/savings-stat";
import { INSTALL_COMMANDS } from "@/lib/site";

// Top of the landing page — the one place the page glows: the mark carries
// .glow-mark. Every other section stays flat. Server component; the install
// command is copyable via `select-all` (one click selects the whole line).
// The only client island is SavingsStat — the live site-wide counter on the
// right, which renders nothing until real numbers arrive.
export function Hero() {
  return (
    <section aria-label="parsec">
      <div className="mx-auto flex w-full max-w-6xl flex-col gap-12 px-6 py-24 sm:py-32 lg:flex-row lg:items-center lg:justify-between">
        <div>
          {/* text-logo, not text-phosphor: the mark keeps the bright brand
              green on light while interactive greens darken to emerald. */}
          <Mark className="h-16 w-auto text-logo glow-mark sm:h-20" />

          <h1 className="mt-10 text-2xl font-extrabold tracking-display text-ink sm:text-3xl">
            Double your Claude Code limit, while improving accuracy
          </h1>

          <p className="mt-4 max-w-2xl text-sm text-muted">
            Measured on 100 SWE-bench Verified tasks with headless Claude Code:
            Parsec cut input tokens 54% and total cost 39% while solving 62
            tasks against the no-compression baseline&apos;s 57.{" "}
            <a
              href="https://github.com/daseinlabs/code-compression-bench"
              target="_blank"
              rel="noopener noreferrer"
              className="text-info underline underline-offset-2 hover:text-phosphor"
            >
              See the benchmark
            </a>
          </p>

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

          <a
            href="https://www.antler.co/"
            target="_blank"
            rel="noopener noreferrer"
            className="group mt-12 inline-flex items-baseline gap-3"
          >
            <span className="text-xs tracking-caps text-faint uppercase">
              Backed by
            </span>
            <span className="text-sm font-bold tracking-[0.3em] text-antler transition-opacity duration-150 ease-parsec group-hover:opacity-75">
              ANTLER
            </span>
          </a>
        </div>

        <div className="lg:shrink-0">
          <SavingsStat />
        </div>
      </div>
    </section>
  );
}
