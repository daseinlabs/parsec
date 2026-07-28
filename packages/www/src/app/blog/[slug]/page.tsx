import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { allPosts, postBySlug } from "@/lib/posts";
import { JsonLd, blogPostingLd, breadcrumbLd } from "@/components/json-ld";
import { SITE } from "@/lib/site";

// Static export: every post is prerendered from the content directory, and
// anything outside that list 404s at build time (dynamicParams = false).
export function generateStaticParams() {
  return allPosts().map((p) => ({ slug: p.slug }));
}

export const dynamicParams = false;

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug: string }>;
}): Promise<Metadata> {
  const { slug } = await params;
  const post = postBySlug(slug);
  if (!post) return {};
  return {
    title: post.title,
    description: post.description,
    alternates: { canonical: `/blog/${slug}/` },
    openGraph: {
      type: "article",
      url: `/blog/${slug}/`,
      title: post.title,
      description: post.description,
      publishedTime: post.date,
    },
  };
}

export default async function BlogPostPage({
  params,
}: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await params;
  // Frontmatter via lib/posts.ts (same YAML the MDX exports — one source, two
  // readers; see next.config.ts). The MDX component carries the body AND the
  // document's own h1 — no duplicate title rendered here.
  const meta = postBySlug(slug);
  if (!meta) notFound();
  const { default: Post } = await import(`@/content/blog/${slug}.mdx`);

  return (
    <main className="mx-auto w-full max-w-3xl px-6 py-16">
      <JsonLd data={blogPostingLd(meta)} />
      <JsonLd
        data={breadcrumbLd([
          { name: "Home", url: `${SITE.url}/` },
          { name: "Blog", url: `${SITE.url}/blog/` },
          { name: meta.title, url: `${SITE.url}/blog/${slug}/` },
        ])}
      />
      <article>
        <header className="mb-2 flex flex-wrap items-baseline gap-x-4 gap-y-2">
          <time dateTime={meta.date} className="text-xs text-faint">
            {meta.date}
          </time>
          {meta.tags.length > 0 && (
            <ul className="flex flex-wrap gap-2">
              {meta.tags.map((tag) => (
                <li
                  key={tag}
                  className="rounded-sm border border-line px-2 py-0.5 text-xs text-faint"
                >
                  {tag}
                </li>
              ))}
            </ul>
          )}
        </header>
        <Post />
        <footer className="mt-12 border-t border-line pt-6">
          <Link
            href="/blog/"
            className="text-sm text-info underline underline-offset-2 transition-colors duration-150 ease-parsec hover:text-phosphor"
          >
            ← All posts
          </Link>
        </footer>
      </article>
    </main>
  );
}
