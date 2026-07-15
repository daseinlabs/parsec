// Server-side Supabase client (@supabase/ssr cookie pattern). Returns null
// when the env is unconfigured so `next build` and fresh clones render a
// setup notice instead of crashing.
import { createServerClient } from "@supabase/ssr";
import { cookies } from "next/headers";

export async function supabaseServer() {
  const url = process.env.NEXT_PUBLIC_SUPABASE_URL;
  const publishableKey = process.env.NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY;
  if (!url || !publishableKey) return null;
  const cookieStore = await cookies();
  return createServerClient(url, publishableKey, {
    cookies: {
      getAll() {
        return cookieStore.getAll();
      },
      setAll(cookiesToSet) {
        try {
          for (const { name, value, options } of cookiesToSet) {
            cookieStore.set(name, value, options);
          }
        } catch {
          // Server components cannot write cookies; middleware owns refresh.
        }
      },
    },
  });
}

/** The Supabase access token (the JWT the platform API verifies), or null. */
export async function accessToken(): Promise<string | null> {
  const supabase = await supabaseServer();
  if (!supabase) return null;
  const { data } = await supabase.auth.getSession();
  return data.session?.access_token ?? null;
}
