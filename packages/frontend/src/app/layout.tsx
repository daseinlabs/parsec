import type { Metadata } from "next";
import { JetBrains_Mono } from "next/font/google";
import "./globals.css";

// The identity is monospace — JetBrains Mono carries display, UI, and code.
// (BRANDING.md §3; IBM Plex Sans stays an unloaded fallback in globals.css.)
const jetbrainsMono = JetBrains_Mono({
  variable: "--font-jetbrains-mono",
  subsets: ["latin"],
  weight: ["400", "500", "700", "800"],
});

export const metadata: Metadata = {
  title: "parsec",
  description: "2× the context. ½ the cost.",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html
      lang="en"
      className={`${jetbrainsMono.variable} h-full antialiased`}
      // The inline script below sets data-theme before React hydrates; this
      // tells React to accept the DOM's version of <html> (see the Next.js
      // "preventing flash before hydration" guide).
      suppressHydrationWarning
    >
      <head>
        {/* Re-apply the persisted theme choice before first paint. No saved
            choice ⇒ no attribute ⇒ globals.css follows the OS preference. */}
        <script
          dangerouslySetInnerHTML={{
            __html: `(function(){try{var t=localStorage.getItem("theme");if(t==="dark"||t==="light")document.documentElement.setAttribute("data-theme",t)}catch(e){}})()`,
          }}
        />
      </head>
      <body className="min-h-full flex flex-col bg-void text-ink">{children}</body>
    </html>
  );
}
