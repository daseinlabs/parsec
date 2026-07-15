// BFF proxy for key minting: the browser calls this same-origin route; the
// platform API sees the Supabase JWT from the session cookie. The key body
// passes straight through — shown once, never stored here.
import { NextResponse } from "next/server";
import { mintKey } from "@/lib/platform";

export async function POST() {
  try {
    const minted = await mintKey();
    if (!minted) return NextResponse.json({ error: "not signed in" }, { status: 401 });
    return NextResponse.json(minted, { status: 201 });
  } catch {
    return NextResponse.json({ error: "platform unreachable" }, { status: 502 });
  }
}
