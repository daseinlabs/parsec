import { InstallCommand } from "@/components/install-command";
import {
  ClaudeIcon,
  OpenAiIcon,
  OpencodeIcon,
} from "@/components/tool-icons";
import type { ToolPage } from "@/content/tools";

// Hero for the per-agent landing pages. Same order as the homepage hero —
// install command first, identity, then wording — but no Mark and no glow:
// the homepage owns the page-glow, and here the identity slot belongs to the
// agent the visitor searched for. Server component; InstallCommand is the
// only client island.
const ICONS = {
  claude: { Icon: ClaudeIcon, color: "text-brand-claude" },
  codex: { Icon: OpenAiIcon, color: "text-brand-openai" },
  opencode: { Icon: OpencodeIcon, color: "text-brand-opencode" },
} as const;

export function ToolHero({ tool }: { tool: ToolPage }) {
  const { Icon, color } = ICONS[tool.installArg];
  return (
    <section aria-label={tool.title}>
      <div className="mx-auto w-full max-w-6xl px-6 pt-12 pb-24 sm:pt-16 sm:pb-32">
        <p className="flex items-center gap-2 text-xs tracking-caps text-faint uppercase">
          <Icon className={`h-4 w-4 shrink-0 ${color}`} />
          parsec for {tool.name}
        </p>

        <div className="mt-6">
          <InstallCommand tool={tool.installArg} />
        </div>

        <h1 className="mt-10 text-2xl font-extrabold tracking-display text-ink sm:text-3xl">
          {tool.headline}
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

        <p className="mt-4 max-w-2xl text-sm text-muted">{tool.detail}</p>
      </div>
    </section>
  );
}
