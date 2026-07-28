// Deliberate copy of packages/frontend/src/components/brand.tsx (no JS
// workspace root to share through — see www/README.md). Change them together.

// The parsec mark: two sightlines converging on a star — the parallax angle.
// Vector master lives at brand/parsecbrandkit/logo/svg/parsec-mark-flat.svg (also copied
// to public/ for non-React consumers). Inlined here so it inherits currentColor
// and stays symmetric about the horizontal axis at any size.
//
// The viewBox crops the master's 260×160 frame to the mark's own bounds. The
// master's built-in whitespace is clear space, and at UI sizes it eats so much
// of the box that the 3.4 strokes land sub-pixel and the mark renders faint.
// Geometry is untouched — this is a crop, not a scale or stretch; layout gap
// supplies the clear space instead.
export function Mark({ className = "h-8 w-auto" }: { className?: string }) {
  return (
    <svg
      viewBox="36 32 192 96"
      fill="none"
      className={className}
      role="img"
      aria-label="parsec"
    >
      <g
        stroke="currentColor"
        strokeWidth="3.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <line x1="44" y1="40" x2="194" y2="80" />
        <line x1="44" y1="120" x2="194" y2="80" />
      </g>
      <path
        d="M194 54 L203.4 70.6 L220 80 L203.4 89.4 L194 106 L184.6 89.4 L168 80 L184.6 70.6 Z"
        fill="currentColor"
      />
    </svg>
  );
}

// Mark + wordmark. The wordmark is never baked into the mark itself.
export function Lockup({ className = "" }: { className?: string }) {
  return (
    <div className={`flex items-center gap-3 ${className}`}>
      {/* text-logo (www-only token): #4AF626 on dark, #2FA317 on light —
          BRANDING.md §2 "logo green". The frontend copy of this file is
          dark-only, where text-phosphor is the same color. */}
      <Mark className="h-9 w-auto text-logo glow-mark" />
      <span className="text-xl font-extrabold tracking-display text-ink">
        parsec
      </span>
    </div>
  );
}
