import type { Metadata } from "next";
import { Hero } from "@/components/sections/hero";
import { Testimonials } from "@/components/sections/testimonials";
import { Pricing } from "@/components/sections/pricing";
import { Faq } from "@/components/sections/faq";
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
// who says so → what it costs → questions.
export default function HomePage() {
  return (
    <main>
      <JsonLd data={softwareApplicationLd()} />
      <JsonLd data={organizationLd()} />
      <Hero />
      <Testimonials />
      <Pricing />
      <Faq />
    </main>
  );
}
