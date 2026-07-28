import { redirect } from "next/navigation";
import { supabaseServer } from "@/lib/supabase/server";
import { MintKey } from "@/components/mint-key";
import { NavLink, PageTitle, SignOut } from "@/components/brand";
import { ThemeToggle } from "@/components/theme-toggle";

export const dynamic = "force-dynamic";

export default async function AccountPage() {
  const supabase = await supabaseServer();
  if (!supabase) {
    return (
      <main className="p-10 text-sm text-muted">
        Set NEXT_PUBLIC_SUPABASE_URL / NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY to enable
        the dashboard (see .env.example).
      </main>
    );
  }
  const { data } = await supabase.auth.getUser();
  if (!data.user) redirect("/login");

  return (
    <main className="mx-auto w-full max-w-4xl p-10">
      <div className="mb-8 flex items-center justify-between">
        <PageTitle>account</PageTitle>
        <nav className="flex items-center gap-5 text-sm">
          <ThemeToggle />
          <NavLink href="/">savings</NavLink>
          <SignOut />
        </nav>
      </div>

      <div className="flex flex-col gap-10">
        <section>
          <h2 className="mb-1 text-xs tracking-caps text-faint uppercase">
            Signed in as
          </h2>
          <p className="text-sm text-muted">
            {data.user.email} <span className="text-faint">· {data.user.id}</span>
          </p>
        </section>

        <section>
          <h2 className="mb-3 text-xs tracking-caps text-faint uppercase">
            Brain API key
          </h2>
          <MintKey />
        </section>

      </div>
    </main>
  );
}
