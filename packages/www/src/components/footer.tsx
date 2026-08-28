"use client";

import Link from "next/link";
import { Mark } from "@/components/brand";
import { SITE } from "@/lib/site";
import { setPendingLeadCookie } from "@/lib/analytics";

const linkClass =
  "text-muted transition-colors duration-150 ease-parsec hover:text-phosphor";

export function Footer() {
  return (
    <footer className="border-t border-line">
      <div className="mx-auto flex w-full max-w-6xl flex-col gap-6 px-6 py-10 sm:flex-row sm:items-center sm:justify-between">
        <div className="flex items-center gap-3">
          <Mark className="h-6 w-auto text-logo" />
          <span className="text-sm text-faint">
            © {new Date().getFullYear()} {SITE.publisher.name}
          </span>
        </div>

        <nav aria-label="Footer" className="flex flex-wrap items-center gap-5 text-sm">
          <Link href="/blog/" className={linkClass}>
            Blog
          </Link>
          <Link href="/privacy/" className={linkClass}>
            Privacy
          </Link>
          <a href={SITE.links.github} rel="noopener" className={linkClass}>
            GitHub
          </a>
          <a
            href={SITE.links.app}
            onClick={setPendingLeadCookie}
            rel="noopener"
            className={linkClass}
          >
            Dashboard
          </a>
        </nav>

        <p className="text-sm text-faint">
          Built with <span aria-hidden>❤️</span>
          <span className="sr-only">love</span> by{" "}
          <a
            href={SITE.links.daseinlabs}
            rel="noopener"
            className="text-info underline underline-offset-2 transition-colors duration-150 ease-parsec hover:text-phosphor"
          >
            daseinlabs
          </a>
        </p>
      </div>
    </footer>
  );
}
