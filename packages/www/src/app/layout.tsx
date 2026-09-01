import type { Metadata, Viewport } from "next";
import { JetBrains_Mono } from "next/font/google";
import Script from "next/script";
import "./globals.css";
import { Nav } from "@/components/nav";
import { Footer } from "@/components/footer";
import { SITE } from "@/lib/site";

// The identity is monospace — JetBrains Mono carries display, UI, and code.
// (BRANDING.md §3; IBM Plex Sans stays an unloaded fallback in globals.css.)
const jetbrainsMono = JetBrains_Mono({
  variable: "--font-jetbrains-mono",
  subsets: ["latin"],
  weight: ["400", "500", "700", "800"],
});

export const metadata: Metadata = {
  // Every relative URL in page metadata (canonical, og:image, …) resolves
  // against this.
  metadataBase: new URL(SITE.url),
  title: {
    default: `${SITE.name} — ${SITE.tagline}`,
    template: `%s · ${SITE.name}`,
  },
  description: SITE.description,
  // No `url` here — a root og:url would leak the homepage URL onto every
  // page that doesn't override it; each page's canonical does that job.
  // Likewise twitter carries only the card type: setting title/description
  // here would shadow per-page og values on pages that define no twitter
  // block (crawlers fall back to og:* when twitter:* is absent).
  openGraph: {
    type: "website",
    siteName: SITE.name,
    title: `${SITE.name} — ${SITE.tagline}`,
    description: SITE.description,
  },
  twitter: {
    card: "summary_large_image",
  },
};

export const viewport: Viewport = {
  // Browser chrome color per scheme; approximates the data-theme choice for
  // the common case where it follows the system.
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#0a0e0c" },
    { media: "(prefers-color-scheme: light)", color: "#f2f5f1" },
  ],
};

// Stamped on <html> before first paint — a static page that resolved its
// theme in React would flash the wrong palette on every load. Stored choice
// wins; otherwise follow the system; the brand default (dark) is the
// Stamped on <html> before first paint — a static page that resolved its
// theme in React would flash the wrong palette on every load. Stored choice
// wins; otherwise follow the system; the brand default (dark) is the
// fallback when matchMedia is unavailable or storage throws.
const themeInit = `(function(){var t;try{t=localStorage.getItem("parsec-theme")}catch(e){}if(t!=="light"&&t!=="dark"){try{t=window.matchMedia("(prefers-color-scheme: light)").matches?"light":"dark"}catch(e){t="dark"}}document.documentElement.dataset.theme=t})()`;

const gtmInit = `(function(w,d,s,l,i){w[l]=w[l]||[];w[l].push({'gtm.start':new Date().getTime(),event:'gtm.js'});var f=d.getElementsByTagName(s)[0],j=d.createElement(s),dl=l!='dataLayer'?'&l='+l:'';j.async=true;j.src='https://www.googletagmanager.com/gtm.js?id='+i+dl;f.parentNode.insertBefore(j,f);})(window,document,'script','dataLayer','GTM-WWJ8KMCL');`;

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    // suppressHydrationWarning: data-theme is set by the inline script before
    // hydration, so the client attribute legitimately differs from the
    // server-rendered HTML. Scoped to this element only.
    <html
      lang="en"
      className={`${jetbrainsMono.variable} h-full antialiased`}
      suppressHydrationWarning
    >
      <head>
        <script dangerouslySetInnerHTML={{ __html: themeInit }} />
        <script dangerouslySetInnerHTML={{ __html: gtmInit }} />
        <Script
          src="https://www.googletagmanager.com/gtag/js?id=G-BQPR460BVF"
          strategy="afterInteractive"
        />
      </head>
      <body className="flex min-h-full flex-col bg-void text-ink">
        <noscript>
          <iframe
            src="https://www.googletagmanager.com/ns.html?id=GTM-WWJ8KMCL"
            height="0"
            width="0"
            style={{ display: "none", visibility: "hidden" }}
          />
        </noscript>
        <Nav />
        <div className="flex-1">{children}</div>
        <Footer />
      </body>
    </html>
  );
}
