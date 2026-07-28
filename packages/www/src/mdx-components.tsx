import type { MDXComponents } from "mdx/types";
import Link from "next/link";

// Required by @next/mdx with the App Router — MDX will not render without it.
// Hand-styled instead of @tailwindcss/typography: `prose` carries its own
// palette that would fight the flipping-token theme, and the brand wants mono
// headings and phosphor rules anyway. Every color is a token utility, so both
// themes come for free.

const components: MDXComponents = {
  h1: ({ children }) => (
    <h1 className="mt-2 mb-6 text-xl font-extrabold tracking-display text-ink sm:text-2xl">
      {children}
    </h1>
  ),
  h2: ({ children }) => (
    <h2 className="mt-10 mb-4 flex items-baseline gap-2 text-lg font-bold tracking-display text-ink">
      <span aria-hidden className="text-phosphor">
        ❯
      </span>
      {children}
    </h2>
  ),
  h3: ({ children }) => (
    <h3 className="mt-8 mb-3 text-md font-bold tracking-display text-ink">
      {children}
    </h3>
  ),
  p: ({ children }) => <p className="mb-4 text-base text-muted">{children}</p>,
  a: ({ href = "", children }) => {
    const external = /^https?:\/\//.test(href);
    const className =
      "text-info underline underline-offset-2 transition-colors duration-150 ease-parsec hover:text-phosphor";
    return external ? (
      <a href={href} rel="noopener" className={className}>
        {children}
      </a>
    ) : (
      <Link href={href} className={className}>
        {children}
      </Link>
    );
  },
  ul: ({ children }) => (
    <ul className="mb-4 list-disc space-y-1 pl-5 text-base text-muted marker:text-phosphor">
      {children}
    </ul>
  ),
  ol: ({ children }) => (
    <ol className="mb-4 list-decimal space-y-1 pl-5 text-base text-muted marker:text-phosphor">
      {children}
    </ol>
  ),
  li: ({ children }) => <li className="pl-1">{children}</li>,
  strong: ({ children }) => (
    <strong className="font-bold text-ink">{children}</strong>
  ),
  em: ({ children }) => <em className="italic">{children}</em>,
  blockquote: ({ children }) => (
    <blockquote className="mb-4 border-l-2 border-line-strong pl-4 text-muted italic">
      {children}
    </blockquote>
  ),
  hr: () => <hr className="my-8 border-line" />,
  // Fenced blocks arrive pre-highlighted by rehype-pretty-code (dual-theme CSS
  // vars; see globals.css). This wrapper only supplies the frame.
  pre: ({ children, ...props }) => (
    <pre
      {...props}
      className="mb-4 overflow-x-auto rounded-lg border border-line bg-surface p-4 text-sm"
    >
      {children}
    </pre>
  ),
  // Inline code only — rehype-pretty-code marks block code with data attrs
  // and supplies its own colors there.
  code: (props) =>
    "data-language" in props ? (
      <code {...props} />
    ) : (
      <code className="rounded-sm border border-line bg-surface px-1 py-0.5 text-sm text-ink">
        {props.children}
      </code>
    ),
  table: ({ children }) => (
    <div className="mb-4 overflow-x-auto rounded-lg border border-line">
      <table className="w-full text-sm">{children}</table>
    </div>
  ),
  th: ({ children }) => (
    <th className="border-b border-line bg-surface px-3 py-2 text-left text-xs tracking-caps text-faint uppercase">
      {children}
    </th>
  ),
  td: ({ children }) => (
    <td className="border-b border-line px-3 py-2 text-muted last:border-0">
      {children}
    </td>
  ),
};

export function useMDXComponents(): MDXComponents {
  return components;
}
