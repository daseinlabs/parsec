# Parsec trims Codex user messages

**Status:** Confirmed defect
**Severity:** Release blocker for Codex routing
**Observed:** 2026-08-18
**Environment:** Windows, Codex CLI 0.147.0, Parsec plugin `0.0.10-alpha-26-gdf00a1e`, Parsec proxy 0.1.0

## Executive finding

Parsec is modifying and removing content from ordinary Codex `role: user` messages before they reach the model.

In a controlled synthetic test, Codex stored and displayed the complete user prompt, but the model received only the beginning. Eleven distributed sentinels and a critical instruction at the end were absent from the model-visible input. Parsec recorded `freeze_cut_tokens: 1398` for the same request.

This creates a particularly dangerous failure mode: the user sees their full instructions in the transcript, while the agent behaves as though later requirements were never provided. The resulting behavior looks like agent confusion, inattentiveness, or refusal, even though the instructions were removed in transit.

User messages must be protected content. For Codex, trimming should be limited to explicitly eligible reasoning/thinking content and tool outputs. Human-authored instructions must remain byte-for-byte intact.

## Confirmed evidence

### 1. The request traversed the current Parsec proxy

Before the definitive test, a stale Parsec `alpha-9` supervisor was found occupying `127.0.0.1:8082`. It returned `404` for Codex endpoints such as `/chatgpt/models` and `/chatgpt/responses`.

That stale process was stopped and replaced with the current Codex plugin binary:

```text
C:\Users\nicks\.codex\plugins\cache\parsec-marketplace\parsec\0.0.10-alpha-26-gdf00a1e\bin\win-x64\parsec.exe
```

The replacement supervisor owned port 8082, `/health` returned HTTP 200, and a fresh Codex request completed through the proxy. The ledger identified the request with `"tool":"codex"` and model `gpt-5.6-sol`.

### 2. Definitive `role: user` integrity probe

A fresh, read-only Codex session received a synthetic user prompt containing:

- twelve unpredictable sentinels, `U00` through `U11`, distributed across the message;
- substantial neutral padding between each sentinel;
- a critical instruction at the absolute end;
- a unique tail codeword, `TAILSAFE_6R8M2`;
- an explicit instruction not to infer or reconstruct missing content.

The raw Codex transcript recorded the complete prompt as a normal user message. The model reported:

```text
No. In the message I actually received, only `U00_Q7A1` was visible.
Content was omitted after that, so I cannot verify exact sentinels `U01`
through `U11` or claim that all were present.
```

The model did not output `TAILSAFE_6R8M2`, confirming that the critical tail instruction was unavailable to it.

The corresponding Parsec ledger row was:

```json
{
  "request_id": "req_30f396ee08be1a83e015f70887e101d3",
  "ts": "2026-08-18T20:36:00Z",
  "conv_id": "ff1edaf09ae509dced08be2d",
  "billed_input_tokens": 9669,
  "billed_output_tokens": 156,
  "billed_cache_read_tokens": 6912,
  "fail_open": false,
  "model": "gpt-5.6-sol",
  "tool": "codex",
  "brain_ms": 772.7,
  "freeze_cut_tokens": 1398
}
```

`freeze_cut_tokens` is a request-level total and does not independently identify which role was cut. Attribution to the user message is established by the distributed sentinel loss, the missing tail codeword, the model's direct report, and the complete raw user message retained in the Codex transcript.

### 3. Corroborating fresh-session failure

An earlier fresh Codex test asked the agent to:

1. generate 4,000 lines of synthetic tool output;
2. run two unrelated commands;
3. repeat the identical 4,000-line command;
4. report any omission marker.

The model stated that the request tail after `Then run the iden...` was omitted. It inferred the missing instruction and completed the second command anyway. Four Parsec ledger rows for that conversation each recorded `freeze_cut_tokens: 32`.

This was weaker evidence because the agent inferred the missing instruction, but it is consistent with the definitive distributed-sentinel result.

### 4. Subagent behavior differs and can create false confidence

Two collaboration-subagent probes preserved all sentinels, including exact final phrases, while their corresponding Parsec requests recorded cuts of 1,173 and 1,028 tokens.

However, collaboration task delivery is out-of-band and was not serialized in the saved session as an ordinary `role: user` record. It therefore does not invalidate the direct user-message failure. Instead, it exposes another sharp edge: subagent tests can pass while top-level Codex user prompts fail.

## Expected behavior

- Every `role: user` message reaches the model byte-for-byte intact.
- Current and historical user instructions are never candidates for freezing, folding, summarization, or omission.
- Only explicitly allowed content classes are trim-eligible, currently reasoning/thinking blocks and tool outputs.
- Any transformed eligible content includes a clear, truthful omission marker visible to the agent.
- Ledger accounting distinguishes cuts by role and content class.

## Actual behavior

- The Codex transcript and UI retain the complete user message.
- The model receives an incomplete version of that message.
- Requirements after the retained prefix can silently disappear.
- The agent has no reliable way to recover the missing requirements.
- The user receives no clear UI indication that Parsec altered their message.
- The ledger reports aggregate cut tokens but does not make the affected role apparent.

## User-facing failure modes

This defect is especially harmful to Codex users and vibe coders because long natural-language prompts commonly put important constraints near the end.

Likely failures include:

- acceptance criteria disappearing while the headline request remains;
- `do not edit`, `do not deploy`, or other safety constraints disappearing;
- output-format requirements disappearing;
- target paths, filenames, environments, or branch constraints disappearing;
- later corrections overriding earlier instructions disappearing;
- an agent asking for information that the user can visibly see they already supplied;
- an agent confidently completing the wrong task because only the opening intent survived;
- users blaming model quality when the proxy removed the relevant instruction;
- inconsistent behavior between top-level agents and collaboration subagents.

The silent transcript/model mismatch is worse than an explicit request failure. A hard error is diagnosable; an apparently inattentive agent is not.

## Additional setup defect discovered during testing

`parsec setup codex` initially reported success because something was listening on port 8082. The listener was actually an obsolete `alpha-9` Parsec process from a Claude plugin cache. It did not implement the Codex ChatGPT-subscription routes expected by the current setup and returned `404`.

Setup therefore treated port occupancy and a generic health response as sufficient compatibility evidence. It should verify that the listener is a compatible Parsec build and that required Codex routes are available before declaring success.

## Reproduction outline

1. Enable Parsec for Codex in ChatGPT-subscription mode.
2. Confirm that the current Parsec binary, not a stale process, owns the configured port.
3. Start a fresh Codex session so the routing configuration is loaded.
4. Send one synthetic `role: user` prompt with unpredictable sentinels distributed from beginning to end.
5. Put a unique, non-inferable instruction at the absolute end.
6. Ask the model to enumerate received sentinels without reconstructing missing text.
7. Compare:
   - the raw user message stored in the Codex transcript;
   - the model's enumeration;
   - the required tail response;
   - the Parsec ledger row and `freeze_cut_tokens`.

A passing implementation must preserve every sentinel and follow the unique tail instruction.

## Required fix criteria

The issue should not be considered fixed until all of the following pass:

1. **Role protection:** `role: user` content is categorically excluded from all trimming paths.
2. **Byte integrity:** short, long, multiline, Unicode, JSON, code-block, attachment-reference, and mixed-content user messages remain byte-identical.
3. **Historical integrity:** earlier user turns remain protected when later requests are curated.
4. **Boundary coverage:** sentinels at the head, middle, tail, chunk boundaries, and immediately after tool results all survive.
5. **Instruction precedence:** corrections and safety constraints at the end of a long prompt survive.
6. **Subagent parity:** top-level tasks, forked agents, collaboration subagents, and resumed sessions follow the same protection rule.
7. **Visible transformations:** every trimmed eligible block has an explicit marker with an accurate omitted-token or omitted-line count.
8. **Role-aware accounting:** the ledger reports cut tokens by role/content class and asserts zero user-message cuts.
9. **Fail closed:** unknown or malformed Codex message shapes remain unmodified.
10. **Setup compatibility:** setup verifies the active proxy version and required Codex routes, not merely that the port is occupied or `/health` responds.

## Recommended immediate mitigation

Until role protection is fixed and the integrity suite passes, Codex routing through Parsec should be considered unsafe for real work. The conservative mitigation is:

```text
parsec disable codex
```

This report documents the recommendation only. Parsec was not disabled as part of producing this report.

## Conclusion

The failure is confirmed, reproducible, and directly user-visible through agent behavior. Parsec is not merely trimming low-value context: in the tested Codex path, it removes ordinary human-authored instructions while leaving the transcript looking complete.

That mismatch can cause lost requirements, unsafe actions, broken acceptance criteria, and severe agent/user confusion. User-message immutability must be enforced before the Codex integration is safe to ship.
