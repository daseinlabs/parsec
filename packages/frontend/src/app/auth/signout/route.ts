import { NextResponse, type NextRequest } from "next/server";
import { supabaseServer } from "@/lib/supabase/server";
import { publicOrigin } from "@/lib/site-url";

export async function POST(request: NextRequest) {
  const supabase = await supabaseServer();
  if (supabase) await supabase.auth.signOut();
  // publicOrigin, not request.url: the latter is 0.0.0.0:8080 behind Cloud Run.
  return NextResponse.redirect(`${publicOrigin(request)}/login`);
}
