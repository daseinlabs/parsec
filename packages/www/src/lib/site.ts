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
    "parsec is a Claude Code plugin that compresses agent context per turn " +
    "with a learned curator. Model traffic goes straight from your machine " +
    "to Anthropic — never through our cloud — and savings are measured " +
    "per-request, never estimated.",
  links: {
    app: "https://app.getparsec.ai",
    github: "https://github.com/daseinlabs/claude-plugins",
    daseinlabs: "https://daseinlabs.ai",
  },
  publisher: {
    name: "Dasein Labs",
    url: "https://daseinlabs.ai",
  },
} as const;

// The install pair shown in the demo terminal and CTA — keep in lockstep with
// packages/marketplace/README.md.
export const INSTALL_COMMANDS = [
  "claude plugin marketplace add https://github.com/daseinlabs/claude-plugins",
  "claude plugin install parsec@parsec-marketplace",
] as const;
