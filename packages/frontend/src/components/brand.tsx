import Link from "next/link";

// The parsec mark: two sightlines converging on a star — the parallax angle.
// Vector master lives at brand/parseclogo/svg/parsec-mark-flat.svg (also copied
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
      <Mark className="h-9 w-auto text-phosphor glow-mark" />
      <span className="text-xl font-extrabold tracking-display text-ink">
        parsec
      </span>
    </div>
  );
}

export function NavLink({
  href,
  children,
}: {
  href: string;
  children: React.ReactNode;
}) {
  return (
    <Link
      href={href}
      className="text-muted transition-colors duration-150 ease-parsec hover:text-phosphor"
    >
      {children}
    </Link>
  );
}

export function SignOut() {
  return (
    <form action="/auth/signout" method="post">
      <button className="text-muted transition-colors duration-150 ease-parsec hover:text-phosphor">
        sign out
      </button>
    </form>
  );
}

// Page title in the terminal voice: a prompt caret, then the thing.
export function PageTitle({ children }: { children: React.ReactNode }) {
  return (
    <h1 className="flex items-baseline gap-2 text-lg font-bold tracking-display">
      <span className="text-phosphor glow">❯</span>
      {children}
    </h1>
  );
}
