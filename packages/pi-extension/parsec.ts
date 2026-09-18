/**
 * parsec-pi — thin pi extension for the parsec curating proxy.
 * sentinel: parsec-managed-extension (do not remove — `parsec setup pi`
 * and `parsec disable pi` identify their managed file drop by it).
 *
 * Deliberately thin, exactly like the opencode shim: all curation happens in
 * the proxy that `parsec setup pi` points pi's built-in `anthropic` provider
 * at (`<pi dir>/models.json` → `providers.anthropic.baseUrl`). This file only
 * (1) revives the proxy at session start so a killed supervisor self-heals,
 * (2) tags requests with `x-parsec-tool: pi` so the savings ledger can
 * attribute per tool, and (3) bridges pi's tool events to the same
 * `parsec hook` subcommands the Claude Code plugin runs, so the no-reread
 * gate and the command-loop breaker work here too. Every step fails open: no
 * proxy, no binary, no ledger → pi behaves exactly as if this file were
 * absent.
 *
 * No dependencies, no build step: only `node:` builtins, loaded from source by
 * pi's jiti loader out of `<pi dir>/extensions/parsec.ts`.
 *
 * PARITY GAPS vs the Claude Code hooks (packages/plugin/hooks/parsec-hooks.json).
 * These are contract limits, not omissions — do not "fix" them by inventing
 * fields the Rust side ignores (packages/proxy/src/hook.rs):
 *
 *   - Stop / the adjudicator is NOT bridged. `parsec hook Stop` reads
 *     `transcript_path` and parses a Claude Code JSONL transcript
 *     (hook.rs `run_stop` → `adjudicator::messages_from_transcript`). pi has
 *     no such file, so the hook would adjudicate an empty message list and
 *     mint a meaningless row. Worse, its only useful verdict is "block" —
 *     forcing the agent to continue — and pi's `agent_end` has no documented
 *     return value to steer the run with (`agent_settled` is the event that
 *     reports pi will not auto-continue, and it is equally unable to inject a
 *     continuation). Bridging it would cost a process spawn per turn and buy
 *     nothing, so it is left out.
 *   - `session_id` here is `pi-<pid>`, not a Claude Code session id. It only
 *     has to be stable for the life of one pi process and unique between
 *     them, which is what the hook's per-session state file needs.
 *   - pi's `powershell` tool is deliberately NOT translated to `Bash`. The
 *     Rust gate parses a command string with bash semantics
 *     (noreread.rs `gate_bash`), and feeding PowerShell to it could mint a
 *     wrong DENY — a user-visible block. It is forwarded under its own name
 *     and treated as Allow, so Windows users lose the command-loop breaker
 *     rather than getting a wrong answer from it.
 *   - The no-reread gate is DEFAULT OFF on the Rust side (it needs an API key
 *     plus `PARSEC_NOREREAD=on`), so in a default install both tool hooks
 *     below return "allow" after a few milliseconds. That is the same
 *     posture Claude Code runs in.
 *
 * A throw inside a `tool_call` handler BLOCKS the tool in pi (its documented
 * fail-safe). That inverts parsec's fail-open rule, so every handler below
 * catches everything and returns undefined.
 */

import { spawn, spawnSync } from "node:child_process";
import { homedir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";

/** Ledger attribution tag; must match `TOOL` in packages/proxy/src/setup_pi.rs. */
const TOOL = "pi";

/**
 * Windows ships `parsec.exe`; the bare name still resolves through PATH
 * (libuv applies PATHEXT), but the conventional drop path must carry the
 * extension or the probe finds nothing — the same rule the opencode shim
 * documents, and the same path `setup_opencode::bin_alias_path()` writes.
 */
const BIN_NAME = process.platform === "win32" ? "parsec.exe" : "parsec";

const parsecHome = () => join(homedir(), ".parsec");

/**
 * Stable for one pi process, distinct between them: all the hook's session
 * state file needs. Not an RNG — this file must not invent entropy that a
 * restart cannot reproduce.
 */
const SESSION_ID = `pi-${process.pid}`;

/**
 * Locate the parsec binary: explicit env wins, then PATH, then the
 * conventional drop location. null = not installed (stay passthrough).
 *
 * Kept in step with `findParsecBin()` in packages/opencode-plugin/index.js —
 * both probe `parsec --version` rather than trusting a path to be runnable.
 */
function findParsecBin(): string | null {
  /** PARSEC_BIN: explicit override for the parsec binary, for dev builds. */
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

/** Resolved once per process: the probe spawns, and tool hooks are hot. */
let binCache: string | null | undefined;
function parsecBin(): string | null {
  if (binCache === undefined) binCache = findParsecBin();
  return binCache;
}

/**
 * A baseUrl is ours iff it is a local parsec-shaped proxy — the Rust
 * `hook::local_proxy_port` rule. pi hands `baseUrl` straight to the Anthropic
 * SDK as `baseURL`, and the SDK concatenates `/v1/messages` onto it, so what
 * setup writes (and what this matches) is a BARE ORIGIN with no `/v1`. pi
 * calls `client.beta.messages.create`, so the request lands on the proxy as
 * `POST /v1/messages?beta=true` — matched on path, query ignored.
 */
function localProxyPort(base: unknown): number | null {
  if (typeof base !== "string") return null;
  const m = /^http:\/\/(127\.0\.0\.1|localhost):(\d+)\/?$/.exec(base.trim());
  return m ? Number(m[2]) : null;
}

let warned = false;
function warnOnce(message: string): void {
  if (warned) return;
  warned = true;
  console.error(`parsec: ${message}`);
}

/**
 * Run one `parsec hook <event>` with a Claude Code-shaped payload on stdin.
 * Returns the parsed stdout object, or null for every failure mode — a hook
 * that cannot run must never fail the tool call.
 */
function runHook(event: string, payload: unknown): Record<string, any> | null {
  const bin = parsecBin();
  if (!bin) return null;
  try {
    const res = spawnSync(bin, ["hook", event], {
      input: JSON.stringify(payload),
      encoding: "utf8",
      timeout: 5000,
    });
    if (res.status !== 0) return null;
    const out = (res.stdout || "").trim();
    if (!out) return null;
    const parsed = JSON.parse(out);
    return parsed && typeof parsed === "object" ? parsed : null;
  } catch {
    return null;
  }
}

function absolutize(path: string, cwd: string): string {
  return isAbsolute(path) ? path : resolve(cwd || ".", path);
}

/**
 * pi tool call → the Claude Code tool name and input shape `hook.rs` gates on.
 *
 * Only `read`, `bash`, `edit` and `write` mean anything to the Rust side
 * (hook.rs `record_post` / the PreToolUse gate); everything else is forwarded
 * under its own pi name and treated as Allow there. Forwarding rather than
 * filtering keeps the policy in one place: if the gate learns a new tool, this
 * file does not have to.
 */
function translate(
  toolName: string,
  input: any,
  cwd: string,
): { tool: string; input: Record<string, unknown> } {
  const i = input && typeof input === "object" ? input : {};
  const filePath = (): string | undefined => {
    const p = i.path ?? i.file_path ?? i.filePath;
    return typeof p === "string" && p.length > 0 ? absolutize(p, cwd) : undefined;
  };
  switch (toolName) {
    case "read": {
      const out: Record<string, unknown> = {};
      const p = filePath();
      if (p !== undefined) out.file_path = p;
      if (Number.isInteger(i.offset)) out.offset = i.offset;
      if (Number.isInteger(i.limit)) out.limit = i.limit;
      return { tool: "Read", input: out };
    }
    case "bash":
      return {
        tool: "Bash",
        input: typeof i.command === "string" ? { command: i.command } : {},
      };
    case "edit":
    case "write": {
      const p = filePath();
      return {
        tool: toolName === "edit" ? "Edit" : "Write",
        input: p !== undefined ? { file_path: p } : {},
      };
    }
    default:
      return { tool: toolName, input: i };
  }
}

/**
 * Only tag a request we know is going to the anthropic provider. pi's
 * `before_provider_headers` payload carries the headers but not the provider,
 * so the model on the context is the only evidence; when there is none we skip
 * rather than leak a parsec header to a third-party API. Attribution still
 * works in that case: `parsec setup pi` also writes the same header into
 * `providers.anthropic.headers` in models.json.
 */
function isAnthropicRequest(ctx: any): boolean {
  const model = ctx?.model;
  if (!model) return false;
  if (model.provider === "anthropic") return true;
  return localProxyPort(model.baseUrl) !== null;
}

export default function (pi: any): void {
  // Self-heal: the proxy is shared across tools and may have been killed
  // since the last session. Same revival contract as the Codex SessionStart
  // hook (`setup_codex::hook_command_line`) and the Claude Code plugin.
  pi.on("session_start", () => {
    try {
      const bin = parsecBin();
      if (!bin) {
        warnOnce("binary not found — running unrouted (install parsec, then `parsec setup pi`)");
        return;
      }
      spawn(bin, ["up", "--session-start"], {
        detached: true,
        stdio: "ignore",
      }).unref();
    } catch {
      /* fail open: pi runs fine against a dead proxy's absence */
    }
  });

  pi.on("before_provider_headers", (event: any, ctx: any) => {
    try {
      const headers = event?.headers;
      if (!headers || typeof headers !== "object") return;
      if (headers["x-parsec-tool"] != null) return;
      if (!isAnthropicRequest(ctx)) return;
      headers["x-parsec-tool"] = TOOL;
    } catch {
      /* fail open: an untagged request still gets curated */
    }
  });

  pi.on("tool_call", (event: any, ctx: any) => {
    try {
      const cwd = ctx?.cwd || process.cwd();
      const { tool, input } = translate(event?.toolName, event?.input, cwd);
      const res = runHook("PreToolUse", {
        session_id: SESSION_ID,
        cwd,
        hook_event_name: "PreToolUse",
        tool_name: tool,
        tool_input: input,
      });
      const decision = res?.hookSpecificOutput;
      if (decision?.permissionDecision === "deny") {
        return {
          block: true,
          reason: decision.permissionDecisionReason || "blocked by parsec",
        };
      }
    } catch {
      /* fail open: never block a tool because the gate misbehaved */
    }
    return undefined;
  });

  // PostToolUse records what actually happened. hook.rs only reads the tool
  // name, its input and cwd — the result content is deliberately not sent.
  pi.on("tool_result", (event: any, ctx: any) => {
    try {
      const cwd = ctx?.cwd || process.cwd();
      const { tool, input } = translate(event?.toolName, event?.input, cwd);
      runHook("PostToolUse", {
        session_id: SESSION_ID,
        cwd,
        hook_event_name: "PostToolUse",
        tool_name: tool,
        tool_input: input,
      });
    } catch {
      /* fail open */
    }
    return undefined;
  });
}
