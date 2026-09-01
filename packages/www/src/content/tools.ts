// Copy for the per-agent landing pages (/claude-code/, /codex/, /opencode/).
// These exist for ad/search message match: a visitor who clicked "parsec for
// Claude Code" lands on a page that says Claude Code in the headline and
// pins the installer to that agent. Facts come from
// packages/marketplace/README.md ("What each tool gets") — keep in lockstep.
//
// Copy honesty: the SWE-bench numbers were measured with headless Claude
// Code, so every page names Claude Code as the harness — the Codex and
// opencode pages never imply tool-specific measurements.

export type ToolSlug = "claude-code" | "codex" | "opencode";

export type ToolPage = {
  /** Display name, matching tool-icons.tsx. */
  name: string;
  /** install.sh selector — curl … | bash -s -- <installArg>. */
  installArg: "claude" | "codex" | "opencode";
  /** <title> via the root template ("%s · parsec"). */
  title: string;
  description: string;
  headline: string;
  /** One tool-specific sentence under the headline. */
  detail: string;
};

export const TOOL_PAGES: Record<ToolSlug, ToolPage> = {
  "claude-code": {
    name: "Claude Code",
    installArg: "claude",
    title: "parsec for Claude Code",
    description:
      "Compress Claude Code's context per turn with a learned curator. " +
      "54% fewer input tokens on SWE-bench Verified, measured per-request " +
      "— never estimated.",
    headline: "Double Claude Code's limit, while improving accuracy",
    detail:
      "Claude Code gets the full plugin: scout tools, the no-reread hook, " +
      "skills, and status-line savings. Uninstall anytime with " +
      "claude plugin uninstall parsec.",
  },
  codex: {
    name: "Codex CLI",
    installArg: "codex",
    title: "parsec for Codex CLI",
    description:
      "Route Codex CLI through parsec's learned context curator with your " +
      "existing ChatGPT sign-in — the token never leaves your machine. " +
      "Savings measured per-request.",
    headline: "Double Codex CLI's limit — with your ChatGPT sign-in",
    detail:
      "Every Codex session routes through parsec using your existing " +
      "ChatGPT sign-in; the token never leaves your machine (API-key mode: " +
      "--byok). Type $ and pick parsec-savings for the measured " +
      "ledger. Undo with parsec disable codex.",
  },
  opencode: {
    name: "opencode",
    installArg: "opencode",
    title: "parsec for opencode",
    description:
      "Compress opencode's context per turn with a learned curator, on " +
      "Anthropic API-key providers. Savings measured per-request — never " +
      "estimated.",
    headline: "Double opencode's limit, while improving accuracy",
    detail:
      "Works with opencode's Anthropic API-key providers; /parsec-savings " +
      "shows the measured ledger. Undo with parsec disable opencode.",
  },
};

export const TOOL_SLUGS = Object.keys(TOOL_PAGES) as ToolSlug[];
