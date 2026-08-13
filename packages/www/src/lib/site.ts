// Single source for site-wide constants. Every absolute URL on the site —
// canonical tags, sitemap, JSON-LD, OG — derives from SITE.url so a domain
// change is a one-line edit.
export const SITE = {
  url: "https://getparsec.ai",
  name: "parsec",
  // The one sanctioned stat line (brand/parsecbrandkit/BRANDING.md §4).
  tagline: "2× the context. ½ the cost.",
  // "Straight to Anthropic, never through our cloud" — not "never leaves your
  // machine": requests do leave (to Anthropic), and paid-tier scoring sends
  // chunk text to our API. The invariant worth claiming is the routing one
  // (DIRECTION.md §2, revised 2026-07-20).
  description:
    "parsec compresses agent context per turn with a learned curator — in " +
    "Claude Code, Codex, and opencode. Model traffic goes straight from " +
    "your machine to your model provider — never through our cloud — and " +
    "savings are measured per-request, never estimated.",
  links: {
    app: "https://app.getparsec.ai",
    github: "https://github.com/daseinlabs/plugins",
    daseinlabs: "https://daseinlabs.ai",
  },
  publisher: {
    name: "Dasein Labs",
    url: "https://daseinlabs.ai",
  },
} as const;

// The install pair shown in the CTA — keep in lockstep with
// packages/marketplace/README.md.
export const INSTALL_COMMANDS = [
  "claude plugin marketplace add https://github.com/daseinlabs/plugins",
  "claude plugin install parsec@parsec-marketplace",
] as const;

// The universal installer (scripts/install.sh / install.ps1, published by
// release.yml): detects Claude Code, Codex, and opencode on the machine and
// activates each. Keyed by OS for the toggle in InstallCommand — keep both
// lines in lockstep with packages/marketplace/README.md.
export const INSTALL_ONE_LINERS = {
  unix: "curl -fsSL https://raw.githubusercontent.com/daseinlabs/plugins/main/install.sh | bash",
  windows:
    'powershell -c "irm https://raw.githubusercontent.com/daseinlabs/plugins/main/install.ps1 | iex"',
} as const;
