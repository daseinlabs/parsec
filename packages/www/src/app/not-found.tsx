import type { Metadata } from "next";
import Link from "next/link";

export const metadata: Metadata = {
  title: "404",
};

export default function NotFound() {
  return (
    <main className="mx-auto w-full max-w-6xl px-6 py-24">
      <h1 className="text-xl font-bold tracking-display text-ink sm:text-2xl">
        <span aria-hidden="true" className="text-error">
          ✗
        </span>{" "}
        404 — route not found
      </h1>
      <p className="mt-4 text-muted">
        This path doesn&apos;t resolve to anything. Nothing here to compress.
      </p>
      <Link
        href="/"
        className="mt-8 inline-block rounded-md border border-line-strong px-4 py-2 text-sm font-bold text-phosphor transition-colors ease-parsec hover:bg-phosphor hover:text-on-phosphor"
      >
        cd ~<span className="sr-only"> — back to the homepage</span>
      </Link>
    </main>
  );
}
