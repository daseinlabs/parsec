import type { Metadata } from "next";
import { Hero } from "@/components/sections/hero";
import { Honesty } from "@/components/sections/honesty";
import { Testimonials } from "@/components/sections/testimonials";
import { Pricing } from "@/components/sections/pricing";
import { Faq } from "@/components/sections/faq";
import { Cta } from "@/components/sections/cta";
import {
  JsonLd,
  organizationLd,
  softwareApplicationLd,
} from "@/components/json-ld";

export const metadata: Metadata = {
  alternates: { canonical: "/" },
};

// The landing page is pure composition — every section owns its copy and
// markup under components/sections/. Order tells the story: what it is →
// who says so → why the numbers are real → what it costs → questions →
// install.
export default function HomePage() {
  return (
    <main>
      <JsonLd data={softwareApplicationLd()} />
      <JsonLd data={organizationLd()} />
      <Hero />
      <Testimonials />
      <Honesty />
      <Pricing />
      <Faq />
      <Cta />
    </main>
  );
}
