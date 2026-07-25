import type { NextRequest } from "next/server";

/**
 * Public origin of the current request.
 *
 * Behind a proxy (Cloud Run), the Next standalone server binds 0.0.0.0:8080 and
 * the proxy forwards to it over plain HTTP, so `new URL(request.url).origin` is
 * that INTERNAL address — redirecting to it sends the browser to
 * https://0.0.0.0:8080. The browser-facing origin rides in the X-Forwarded-*
 * headers the proxy sets, so prefer those. In development (no proxy) there are
 * no forwarded headers, so fall back to the request origin (localhost).
 */
export function publicOrigin(request: NextRequest): string {
  const host = request.headers.get("x-forwarded-host");
  if (process.env.NODE_ENV !== "development" && host) {
    const proto = request.headers.get("x-forwarded-proto") ?? "https";
    return `${proto}://${host}`;
  }
  return new URL(request.url).origin;
}
