import type { Metadata, Viewport } from "next";
import { JetBrains_Mono } from "next/font/google";
import Script from "next/script";
import { GoogleTagManager } from "@next/third-parties/google";
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
// fallback when matchMedia is unavailable or storage throws.
const themeInit = `(function(){var t;try{t=localStorage.getItem("parsec-theme")}catch(e){}if(t!=="light"&&t!=="dark"){try{t=window.matchMedia("(prefers-color-scheme: light)").matches?"light":"dark"}catch(e){t="dark"}}document.documentElement.dataset.theme=t})()`;

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
      <GoogleTagManager gtmId="GTM-WWJ8KMCL" />
      <head>
        <script dangerouslySetInnerHTML={{ __html: themeInit }} />
        <Script
          src="https://www.googletagmanager.com/gtag/js?id=G-BQPR460BVF"
          strategy="afterInteractive"
        />
        <Script id="gtag-init" strategy="afterInteractive">
          {`window.dataLayer = window.dataLayer || [];
function gtag(){dataLayer.push(arguments);}
gtag('js', new Date());
gtag('config', 'G-BQPR460BVF');`}
        </Script>
        
        <!-- X conversion tracking base code -->
        <script>
        !function(e,t,n,s,u,a){e.twq||(s=e.twq=function(){s.exe?s.exe.apply(s,arguments):s.queue.push(arguments);
        },s.version='1.1',s.queue=[],u=t.createElement(n),u.async=!0,u.src='https://static.ads-twitter.com/uwt.js',
        a=t.getElementsByTagName(n)[0],a.parentNode.insertBefore(u,a))}(window,document,'script');
        twq.integration='gtm-ad-manager';
        twq('config','repbw');
        </script>
        <!-- End X conversion tracking base code -->
          
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
