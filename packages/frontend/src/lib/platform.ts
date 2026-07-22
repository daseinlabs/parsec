// Server-side platform API calls (BFF pattern): the browser never talks to
// the platform service directly — no CORS, no tokens in client JS. The JWT
// comes from the Supabase session cookie via lib/supabase/server.
import { accessToken } from "@/lib/supabase/server";

const PLATFORM_URL = () =>
  process.env.PLATFORM_URL ?? "http://127.0.0.1:8080";

export type ModelUsage = {
  model: string | null;
  rows_count: number;
  measured_rows: number;
  tokens_saved: number;
  billed_input_tokens: number;
  billed_output_tokens: number;
  billed_cache_read_tokens: number;
  billed_cache_write_tokens: number;
  cost_usd: number | null;
  currency: string;
};

export type LedgerSummary = {
  rows_count: number;
  counterfactual_input_tokens: number;
  billed_input_tokens: number;
  billed_output_tokens: number;
  billed_cache_read_tokens: number;
  billed_cache_write_tokens: number;
  fail_open_count: number;
  measured_rows: number;
  tokens_saved: number;
  cost_usd: number;
  currency: string;
  by_model: ModelUsage[];
};

export type DayUsage = {
  date: string;
  rows_count: number;
  measured_rows: number;
  tokens_saved: number;
  billed_input_tokens: number;
  billed_output_tokens: number;
  billed_cache_read_tokens: number;
  billed_cache_write_tokens: number;
  cost_usd: number;
};

export type LedgerUsage = {
  days: DayUsage[];
  window_days: number;
  currency: string;
};

async function platformFetch(path: string, init?: RequestInit) {
  const token = await accessToken();
  if (!token) return null;
  const resp = await fetch(`${PLATFORM_URL()}${path}`, {
    ...init,
    headers: { ...init?.headers, Authorization: `Bearer ${token}` },
    cache: "no-store",
  });
  if (!resp.ok) throw new Error(`platform ${path}: ${resp.status}`);
  return resp.json();
}

export async function ledgerSummary(): Promise<LedgerSummary | null> {
  return platformFetch("/ledger/summary");
}

export async function ledgerUsage(days = 30): Promise<LedgerUsage | null> {
  return platformFetch(`/ledger/usage?days=${days}`);
}

export async function mintKey(): Promise<{ key: string } | null> {
  return platformFetch("/keys", { method: "POST" });
}
