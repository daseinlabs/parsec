import { redirect } from "next/navigation";

// The dashboard IS the app; daseinlabs.github.io / dasein-frontend own the
// public landing page.
export default function Home() {
  redirect("/dashboard");
}
