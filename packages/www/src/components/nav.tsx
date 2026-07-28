import Link from "next/link";
import { Lockup } from "@/components/brand";
import { ThemeToggle } from "@/components/theme-toggle";
import { SITE } from "@/lib/site";

function NavLink({
  href,
  children,
  external = false,
}: {
  href: string;
  children: React.ReactNode;
  external?: boolean;
}) {
  const className =
    "text-muted transition-colors duration-150 ease-parsec hover:text-phosphor";
  if (external) {
    return (
      <a href={href} rel="noopener" className={className}>
        {children}
      </a>
    );
  }
  return (
    <Link href={href} className={className}>
      {children}
    </Link>
  );
}

export function Nav() {
  return (
    <header className="sticky top-0 z-40 border-b border-line bg-void/90 backdrop-blur">
      <div className="mx-auto flex w-full max-w-6xl items-center justify-between px-6 py-4">
        <Link href="/" aria-label="parsec home">
          <Lockup />
        </Link>
        <nav
          aria-label="Main"
          className="flex items-center gap-5 text-sm"
        >
          {/* Anchors resolve on / ; from other routes they land on the
              homepage section thanks to the /# prefix. */}
          <span className="hidden items-center gap-5 sm:flex">
            <NavLink href="/#features">Features</NavLink>
            <NavLink href="/#pricing">Pricing</NavLink>
            <NavLink href="/blog/">Blog</NavLink>
          </span>
          <NavLink href={SITE.links.github} external>
            GitHub
          </NavLink>
          <a
            href={SITE.links.app}
            rel="noopener"
            className="rounded-md border border-line-strong px-3 py-1.5 text-phosphor transition-colors duration-150 ease-parsec hover:bg-phosphor hover:text-on-phosphor"
          >
            Sign in
          </a>
          <ThemeToggle />
        </nav>
      </div>
    </header>
  );
}
