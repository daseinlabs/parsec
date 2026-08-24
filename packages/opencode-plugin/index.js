/**
 * parsec-opencode — thin opencode shim for the parsec curating proxy.
 * sentinel: parsec-managed-plugin (do not remove — `parsec setup opencode`
 * and `parsec disable opencode` identify their managed file drop by it).
 *
 * Deliberately thin (docs/tool-integrations-survey.md §3.4): all curation
 * stays in the local Rust proxy — this file only (1) routes opencode's
 * Anthropic provider at the proxy WHEN a live parsec proxy answers a health
 * probe, (2) tags requests with `x-parsec-tool: opencode` so the savings
 * ledger can attribute per tool, (3) surfaces savings as a toast
 * (opencode has no status line), and (4) registers the /parsec-* command
 * surface (the opencode port of the Claude Code plugin skills) through the
 * same config hook. Every step fails open: no proxy, no binary, no ledger →
 * opencode behaves exactly as if this plugin were absent. BYOK API-key
 * users only — never subscription OAuth.
 *
 * No dependencies, no build step: loadable both as an npm plugin
 * ("plugin": ["parsec-opencode"]) and as a file drop in
 * ~/.config/opencode/plugin/parsec.js (what `parsec setup opencode` does).
 */

import { spawn, spawnSync } from "node:child_process";
import { closeSync, openSync, readFileSync, readSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const TOOL = "opencode";
const DEFAULT_PORT = 8082;
/// The ledger contract this reader understands. Gated exactly like the Rust
/// reader (statusline.rs `aggregate_ledger`): a row from another contract
/// version is not ours to interpret, and silently summing one would put a
/// number on screen that no other savings surface agrees with.
const LEDGER_CONTRACT = "savings-ledger/v0";
/// Windows ships `parsec.exe`; the bare name still resolves through PATH
/// (libuv applies PATHEXT), but the conventional drop path must carry the
/// extension or the probe finds nothing and the whole command surface —
/// plus `parsec up` — silently disappears on Windows.
const BIN_NAME = process.platform === "win32" ? "parsec.exe" : "parsec";

const parsecHome = () => join(homedir(), ".parsec");

/** The port setup routed Claude Code at — the proxy is shared across tools. */
function routedPort() {
  try {
    const st = JSON.parse(
      readFileSync(join(parsecHome(), "setup_state.json"), "utf8"),
    );
    if (Number.isInteger(st.port) && st.port > 0) return st.port;
  } catch {
    /* no state file — first run or opencode-only install */
  }
  return DEFAULT_PORT;
}

/** True only when a *parsec* proxy answers /health on the port — never route
 * at a foreign listener. */
async function proxyHealthy(port) {
  try {
    const res = await fetch(`http://127.0.0.1:${port}/health`, {
      signal: AbortSignal.timeout(500),
    });
    return res.ok && (await res.text()).includes("parsec-proxy");
  } catch {
    return false;
  }
}

/** Locate the parsec binary: explicit env wins, then PATH, then the
 * conventional drop location. null = not installed (stay passthrough). */
function findParsecBin() {
  const explicit = process.env.PARSEC_BIN;
  if (explicit) return explicit;
  for (const candidate of ["parsec", join(parsecHome(), "bin", BIN_NAME)]) {
    try {
      const probe = spawnSync(candidate, ["--version"], { timeout: 2000 });
      if (probe.status === 0) return candidate;
    } catch {
      /* not this one */
    }
  }
  return null;
}

/** Best-effort `parsec up`, detached so it outlives us; then poll health
 * briefly. False = proxy never came up (fail open, run unrouted). */
async function ensureProxy(port, bin) {
  if (await proxyHealthy(port)) return true;
  if (!bin) return false;
  try {
    spawn(bin, ["up"], { detached: true, stdio: "ignore" }).unref();
  } catch {
    return false;
  }
  for (let i = 0; i < 8; i++) {
    await new Promise((r) => setTimeout(r, 250));
    if (await proxyHealthy(port)) return true;
  }
  return false;
}

/** A baseURL is ours iff it is a local parsec-shaped proxy (the Rust
 * hook::local_proxy_port rule, plus the `/v1` suffix the AI SDK needs and
 * this plugin writes). A foreign URL is NEVER overwritten. */
function localProxyPort(base) {
  const m = /^http:\/\/(127\.0\.0\.1|localhost):(\d+)(\/v1)?\/?$/.exec(
    (base || "").trim(),
  );
  return m ? Number(m[2]) : null;
}

/** Session savings from the local ledger: rows this plugin's requests minted
 * (tool === "opencode") since plugin load.
 *
 * Incremental by byte offset rather than a whole-file read per call. The
 * ledger is append-only jsonl and grows without bound, and this runs on
 * EVERY session.idle — re-reading and re-parsing the entire file each time
 * is wasted I/O that scales with how long parsec has been installed. The
 * offset is taken at plugin load, so rows appended after it are this
 * session's by construction; that also drops the wall-clock `ts` filter,
 * which could not distinguish same-millisecond rows and depended on the
 * writer's and reader's clocks agreeing.
 *
 * Returns a closure holding the running total. Never throws: a missing,
 * purged, or rotated ledger degrades to the last figure it knew. */
function makeSavingsTail() {
  const path = join(parsecHome(), "ledger.jsonl");
  /** Bytes already folded into `saved`. */
  let offset = 0;
  /** Trailing bytes of a row the writer had not finished appending. Held as
   * a Buffer, not a string: a chunk boundary can land mid-UTF-8-sequence,
   * and decoding the halves separately would corrupt that row. */
  let partial = Buffer.alloc(0);
  let saved = 0;
  try {
    offset = statSync(path).size;
  } catch {
    /* no ledger yet — start at 0 and pick up the first row written */
  }
  return () => {
    let size;
    try {
      size = statSync(path).size;
    } catch {
      return saved; // purged or uninstalled mid-session
    }
    if (size < offset) {
      // Rotated or truncated: the bytes behind our total are gone, so the
      // total is no longer answerable. Start over rather than report a sum
      // over rows that no longer exist.
      offset = 0;
      partial = Buffer.alloc(0);
      saved = 0;
    }
    if (size === offset) return saved;
    let fd;
    let chunk;
    try {
      fd = openSync(path, "r");
      const buf = Buffer.allocUnsafe(size - offset);
      const n = readSync(fd, buf, 0, buf.length, offset);
      chunk = buf.subarray(0, n);
      offset += n;
    } catch {
      return saved;
    } finally {
      if (fd !== undefined) {
        try {
          closeSync(fd);
        } catch {
          /* nothing useful to do */
        }
      }
    }
    const combined = Buffer.concat([partial, chunk]);
    const lastNl = combined.lastIndexOf(0x0a);
    if (lastNl === -1) {
      // No complete row yet. Guard against a pathological newline-free file
      // pinning an ever-growing buffer in memory.
      partial = combined.length > 1 << 20 ? Buffer.alloc(0) : combined;
      return saved;
    }
    partial = combined.subarray(lastNl + 1);
    for (const line of combined.subarray(0, lastNl).toString("utf8").split("\n")) {
      let row;
      try {
        row = JSON.parse(line);
      } catch {
        continue;
      }
      if (row.contract_version !== LEDGER_CONTRACT) continue;
      if (row.tool !== TOOL) continue;
      // §8.4 honesty: null counterfactual = unmeasured — skip, never impute.
      if (typeof row.counterfactual_input_tokens !== "number") continue;
      const billedIn =
        (row.billed_input_tokens || 0) +
        (row.billed_cache_read_tokens || 0) +
        (row.billed_cache_write_tokens || 0);
      saved += row.counterfactual_input_tokens - billedIn;
    }
    return saved;
  };
}

/** The /parsec-* commands — the opencode port of the Claude Code plugin
 * skills (savings, proxy, key, setup, uninstall; `share` has no CLI
 * subcommand yet and is not ported). Injected through the config hook so
 * both install paths (npm and file drop) carry them and disabling the
 * plugin removes them with it — no extra files on disk. Templates instruct
 * the session agent to run the resolved binary; measurement honesty rules
 * (never estimate, never echo secrets) travel with the template. */
function commandsFor(bin) {
  return {
    "parsec-savings": {
      description:
        "Show measured parsec token savings from the local ledger — never estimated",
      template:
        `Run "${bin} savings" and present its output conversationally. ` +
        `Every number in that report is measured — proxy rows are the ` +
        `per-request count_tokens counterfactual vs actually-billed usage; ` +
        `if the report says the ledger is empty, say so plainly — never ` +
        `estimate or extrapolate savings. The report groups by requests, ` +
        `conversations, and sessions where the ledger has session identity; ` +
        `rows tagged tool=opencode are this tool's traffic.`,
    },
    "parsec-proxy": {
      description:
        "Restart the local parsec proxy if it was killed — safe to run anytime",
      template:
        `Run "${bin} up" and report its output. The command is idempotent ` +
        `and detached: a live listener on the routed port is never ` +
        `double-spawned, so it is always safe to run. Reading the result: ` +
        `"proxy already listening" — the port is healthy, and if requests ` +
        `still fail the problem is not a dead proxy; "proxy up on ` +
        `127.0.0.1:PORT" — revived, requests recover on the next turn; ` +
        `"proxy spawned ... but never started listening" — the spawn ` +
        `failed, so read the tail of ~/.parsec/proxy.log and summarize the ` +
        `actual error rather than guessing. Note: this plugin only routes ` +
        `at launch — if opencode started while the proxy was down, the ` +
        `user must restart opencode after the proxy is back up.`,
    },
    "parsec-key": {
      description:
        "Connect the parsec dashboard API key (set / show / clear)",
      template:
        `The user said: "$ARGUMENTS". The parsec proxy reports each ` +
        `request's measured savings to the user's dashboard only when it ` +
        `has their per-account psc_ API key, minted in the dashboard under ` +
        `Account -> Brain API key (if they don't have one, tell them to ` +
        `mint it there first). If they provided a psc_ key, run ` +
        `"${bin} key set <the-key>", then confirm with "${bin} key show" — ` +
        `it takes effect on the next request, no restart. NEVER echo the ` +
        `full key back; refer to it only by the masked form the command ` +
        `prints. If "key show" reports shipping inactive for lack of a ` +
        `platform URL, this is a dev build: ask for their platform URL and ` +
        `re-run set with --platform-url <url>. Otherwise: "${bin} key show" ` +
        `shows the masked key and where it resolves from; "${bin} key clear" ` +
        `stops dashboard reporting. Local savings reports are unaffected ` +
        `by this key.`,
    },
    "parsec-setup": {
      description:
        "Repair or refresh the parsec opencode integration (plugin + proxy)",
      template:
        `Run "${bin} setup opencode" and report its output. It refreshes ` +
        `the parsec-managed plugin file and warms the local proxy; a plugin ` +
        `file it does not own is never touched. Tell the user to restart ` +
        `opencode afterwards — routing happens at launch. Anthropic API-key ` +
        `providers only; subscription OAuth is out of scope. Undo anytime ` +
        `with "${bin} disable opencode".`,
    },
    "parsec-uninstall": {
      description:
        "Remove parsec from opencode (optionally from the whole machine)",
      template:
        `The user said: "$ARGUMENTS". To remove parsec from opencode only, ` +
        `run "${bin} disable opencode" — it deletes exactly the ` +
        `parsec-managed plugin file and nothing else; tell the user to ` +
        `restart opencode to route directly again. Removing parsec from ` +
        `the whole machine (Claude Code routing, the proxy, downloaded ` +
        `models, ledger data) is "${bin} uninstall" — destructive and ` +
        `shared with Claude Code, so confirm the user means the full ` +
        `removal before running it.`,
    },
  };
}

export const ParsecPlugin = async ({ client }) => {
  const port = routedPort();
  const bin = findParsecBin();
  const routed = await ensureProxy(port, bin);
  const savedTokens = makeSavingsTail();
  let lastToasted = 0;

  const toast = async (message, variant = "info") => {
    try {
      await client.tui.showToast({ body: { message, variant } });
    } catch {
      /* older opencode / headless — savings still land in the ledger */
    }
  };

  if (routed) {
    void toast(`parsec: curation active (127.0.0.1:${port})`);
  }

  return {
    config: async (config) => {
      // Commands ride along even when unrouted — /parsec-proxy is most
      // useful exactly when routing failed at launch. No binary → no
      // commands (nothing for them to run).
      if (bin) {
        config.command ??= {};
        for (const [name, def] of Object.entries(commandsFor(bin))) {
          config.command[name] ??= def; // a user's own command always wins
        }
      }
      if (!routed) return; // fail open: never route at a dead port
      config.provider ??= {};
      config.provider.anthropic ??= {};
      const anthropic = config.provider.anthropic;
      anthropic.options ??= {};
      const existing = anthropic.options.baseURL;
      // Absent or a stale local parsec route → point at the live proxy;
      // anything else is the user's own gateway and is not ours to touch.
      if (!existing || localProxyPort(existing) !== null) {
        anthropic.options.baseURL = `http://127.0.0.1:${port}/v1`;
        anthropic.options.headers = {
          ...anthropic.options.headers,
          "x-parsec-tool": TOOL,
        };
      }
    },

    event: async ({ event }) => {
      if (event?.type !== "session.idle" || !routed) return;
      const saved = savedTokens();
      if (saved > 0 && saved !== lastToasted) {
        lastToasted = saved;
        void toast(
          `parsec: ~${saved.toLocaleString()} input tokens avoided this session`,
        );
      }
    },
  };
};
