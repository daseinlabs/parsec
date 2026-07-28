// Structured-data builders + renderer. Server component — emits a plain
// <script type="application/ld+json"> at build time; nothing here runs in the
// browser. Every builder sets "@context" itself so callers can pass the result
// straight to <JsonLd data={…} />.
import { SITE } from "@/lib/site";

export function JsonLd({ data }: { data: Record<string, unknown> }) {
  return (
    <script
      type="application/ld+json"
      dangerouslySetInnerHTML={{ __html: JSON.stringify(data) }}
    />
  );
}

export function softwareApplicationLd(): Record<string, unknown> {
  return {
    "@context": "https://schema.org",
    "@type": "SoftwareApplication",
    name: SITE.name,
    description: SITE.description,
    url: SITE.url,
    applicationCategory: "DeveloperApplication",
    operatingSystem: "macOS, Linux, Windows",
    offers: {
      "@type": "Offer",
      price: "0",
      priceCurrency: "USD",
    },
  };
}

export function organizationLd(): Record<string, unknown> {
  return {
    "@context": "https://schema.org",
    "@type": "Organization",
    name: SITE.publisher.name,
    url: SITE.publisher.url,
  };
}

export function faqLd(
  items: { q: string; a: string }[],
): Record<string, unknown> {
  return {
    "@context": "https://schema.org",
    "@type": "FAQPage",
    mainEntity: items.map(({ q, a }) => ({
      "@type": "Question",
      name: q,
      acceptedAnswer: {
        "@type": "Answer",
        text: a,
      },
    })),
  };
}

export function blogPostingLd(post: {
  slug: string;
  title: string;
  description: string;
  date: string;
  author?: string;
  authorUrl?: string;
}): Record<string, unknown> {
  return {
    "@context": "https://schema.org",
    "@type": "BlogPosting",
    headline: post.title,
    description: post.description,
    datePublished: post.date,
    ...(post.author && {
      author: {
        "@type": "Person",
        name: post.author,
        ...(post.authorUrl && { url: post.authorUrl }),
      },
    }),
    url: `${SITE.url}/blog/${post.slug}/`,
    publisher: {
      "@type": "Organization",
      name: SITE.publisher.name,
      url: SITE.publisher.url,
    },
  };
}

export function breadcrumbLd(
  crumbs: { name: string; url: string }[],
): Record<string, unknown> {
  return {
    "@context": "https://schema.org",
    "@type": "BreadcrumbList",
    itemListElement: crumbs.map((crumb, i) => ({
      "@type": "ListItem",
      position: i + 1,
      name: crumb.name,
      item: crumb.url,
    })),
  };
}
