import type { Metadata } from "next";
import Link from "next/link";
import { allPosts } from "@/lib/posts";

export const metadata: Metadata = {
  title: "Blog",
  description:
    "Notes from building parsec: context compression, determinism, and " +
    "measuring savings honestly.",
  alternates: { canonical: "/blog/" },
};

// Build-time index over src/content/blog — lib/posts.ts reads the frontmatter
// once; the post pages import the full MDX separately.
export default function BlogIndexPage() {
  const posts = allPosts();

  return (
    <main className="mx-auto w-full max-w-3xl px-6 py-16">
      <h1 className="flex items-baseline gap-2 text-2xl font-extrabold tracking-display text-ink">
        <span aria-hidden className="text-phosphor">
          ❯
        </span>
        Blog
      </h1>

      {posts.length === 0 ? (
        <p className="mt-10 text-sm text-muted">No posts yet.</p>
      ) : (
        <ul className="mt-10 space-y-4">
          {posts.map((post) => (
            <li key={post.slug}>
              <article className="rounded-lg border border-line bg-surface p-5">
                <Link
                  href={`/blog/${post.slug}/`}
                  className="text-base font-bold text-ink transition-colors duration-150 ease-parsec hover:text-phosphor"
                >
                  {post.title}
                </Link>
                <time
                  dateTime={post.date}
                  className="mt-1 block text-xs text-faint"
                >
                  {post.date}
                </time>
                <p className="mt-2 text-sm text-muted">{post.description}</p>
                {post.tags.length > 0 && (
                  <ul className="mt-3 flex flex-wrap gap-2">
                    {post.tags.map((tag) => (
                      <li
                        key={tag}
                        className="rounded-sm border border-line px-2 py-0.5 text-xs text-faint"
                      >
                        {tag}
                      </li>
                    ))}
                  </ul>
                )}
              </article>
            </li>
          ))}
        </ul>
      )}
    </main>
  );
}
