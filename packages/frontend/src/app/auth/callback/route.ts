// OAuth code exchange: Supabase redirects here after GitHub sign-in; the
// code becomes a session cookie and the user lands on the dashboard. The
// landing origin comes from publicOrigin (X-Forwarded-*), not request.url —
// behind Cloud Run the latter is the internal 0.0.0.0:8080 bind address.
import { NextResponse, type NextRequest } from "next/server";
import { supabaseServer } from "@/lib/supabase/server";
import { publicOrigin } from "@/lib/site-url";

export async function GET(request: NextRequest) {
  const params = new URL(request.url).searchParams;
  const code = params.get("code");
  if (code) {
    const supabase = await supabaseServer();
    if (supabase) await supabase.auth.exchangeCodeForSession(code);
  }
  // Same-origin relative paths only — "//evil.com" and absolute URLs would
  // turn this into an open redirect.
  const next = params.get("next");
  const dest = next && /^\/(?!\/)/.test(next) ? next : "/";
  return NextResponse.redirect(`${publicOrigin(request)}${dest}`);
}
