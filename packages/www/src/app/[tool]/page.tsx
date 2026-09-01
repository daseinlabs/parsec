import type { Metadata } from "next";
import { ToolHero } from "@/components/sections/tool-hero";
import { Testimonials } from "@/components/sections/testimonials";
import { Pricing } from "@/components/sections/pricing";
import { Faq } from "@/components/sections/faq";
import { TOOL_PAGES, TOOL_SLUGS, type ToolSlug } from "@/content/tools";

// Per-agent landing pages, one static export per slug in content/tools.ts.
// Everything below the hero is the homepage's own sections — the pages
// differ only where message match demands it.
export const dynamicParams = false;

export function generateStaticParams() {
  return TOOL_SLUGS.map((tool) => ({ tool }));
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ tool: ToolSlug }>;
}): Promise<Metadata> {
  const { tool } = await params;
  const page = TOOL_PAGES[tool];
  return {
    title: page.title,
    description: page.description,
    alternates: { canonical: `/${tool}/` },
    openGraph: {
      title: page.title,
      description: page.description,
      // The benchmark stat card (public/benchmark-card.png) — link previews
      // for these pages show the numbers, not just the logo lockup.
      images: [
        {
          url: "/benchmark-card.png",
          width: 1200,
          height: 630,
          alt:
            "parsec benchmark: 54% fewer input tokens, 39% lower cost, " +
            "62 vs 57 SWE-bench Verified tasks solved — measured per-request.",
        },
      ],
    },
  };
}

export default async function ToolLandingPage({
  params,
}: {
  params: Promise<{ tool: ToolSlug }>;
}) {
  const { tool } = await params;
  return (
    <main>
      <ToolHero tool={TOOL_PAGES[tool]} />
      <Testimonials />
      <Pricing />
      <Faq />
    </main>
  );
}
