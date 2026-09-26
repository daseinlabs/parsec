//! Human-authored content must reach the model byte-for-byte.
//!
//! Regression suite for bugs/parsec_codex_user_message_trimming_report.md:
//! `freeze::parse` types `role:"user"` and `role:"tool"` alike as
//! observations, which is ~true on the Anthropic wire (user turns carry
//! `tool_result`) and FALSE on the Responses wire (tool output is its own
//! `function_call_output` item), so ported unchanged the curator digested
//! Codex user prompts down to a head line, an omission marker, and a tail
//! line. `protect::restore_protected` puts the original bytes back before
//! fold-back; these tests hold that line.
//!
//! Every test drives the real projection → Freezer → fold-back path with a
//! CUT-EVERYTHING scorer — the adversarial upper bound, so a pass means no
//! scoring outcome can reach human text.

use parsec_engine::freeze::{
    BirthQuery, ChunkScorer, FreezeConfig, Freezer, ScoreError, ScoreResult, SCORE_SCALE,
};
use parsec_proxy::{internal, protect, responses, splice};
use serde_json::{json, Value};

/// Scores every live chunk 0 against a maximal tau: cut everything cuttable.
/// Reference cut-at-birth policy: these tests assert that a cut-everything
/// curator still cannot touch human-authored content on the very request
/// that births it, so the current turn must be cut-eligible here.
fn cut_at_birth() -> FreezeConfig {
    FreezeConfig {
        protect_current: false,
        ..FreezeConfig::default()
    }
}

struct CutAll;
impl ChunkScorer for CutAll {
    fn score(&mut self, q: &BirthQuery) -> Result<ScoreResult, ScoreError> {
        Ok(ScoreResult {
            scores_q: vec![0; q.live.len()],
            tau_q: SCORE_SCALE,
            tau_by_kind: None,
        })
    }
}

/// The full Responses path: project, curate with CutAll, protect, fold back.
fn serve_responses(body: &Value) -> (Value, i64) {
    let internal = responses::to_internal(body);
    let mask = responses::protected_mask(body);
    assert_eq!(
        mask.len(),
        internal.len(),
        "responses mask must align with the internal view"
    );
    let mut fz = Freezer::new(cut_at_birth(), CutAll);
    let mut served = fz.serve(&internal).expect("freezer rejected the view");
    let refused = protect::restore_protected(&internal, &mut served, &mask);
    (responses::apply_curation(body, &served, None), refused)
}

/// The full Anthropic path: project, curate with CutAll, protect, fold back.
fn serve_anthropic(body: &Value) -> (Value, i64) {
    let internal = internal::to_internal(body);
    let mask = internal::protected_mask(body);
    assert_eq!(
        mask.len(),
        internal.len(),
        "anthropic mask must align with the internal view"
    );
    let mut fz = Freezer::new(cut_at_birth(), CutAll);
    let mut served = fz.serve(&internal).expect("freezer rejected the view");
    let refused = protect::restore_protected(&internal, &mut served, &mask);
    (splice::apply_curation(body, &served, None), refused)
}

/// A prompt with sentinels at the head, throughout the middle, and at the
/// very end — the report's probe shape.
fn sentinel_prompt(blocks: usize) -> String {
    let mut s = String::from("U00_HEAD_SENTINEL\n");
    for i in 1..blocks {
        for j in 0..40 {
            s.push_str(&format!(
                "padding line {i}-{j}: neutral filler so the message carries real mass\n"
            ));
        }
        s.push_str(&format!("U{i:02}_MID_SENTINEL\n"));
    }
    s.push_str("CRITICAL: do not deploy. Reply with TAILSAFE_6R8M2 and nothing else.\n");
    s
}

fn assert_all_sentinels(text: &str, blocks: usize, what: &str) {
    assert!(
        text.contains("U00_HEAD_SENTINEL"),
        "{what}: head sentinel lost"
    );
    for i in 1..blocks {
        let s = format!("U{i:02}_MID_SENTINEL");
        assert!(text.contains(&s), "{what}: {s} lost — user content was cut");
    }
    assert!(
        text.contains("TAILSAFE_6R8M2"),
        "{what}: tail instruction lost — the constraint at the end disappeared"
    );
}

/// Codex's shape: a preamble user item (which consumes `parse`'s single
/// first-observation exemption), a tool round trip, then the real human turn.
fn codex_body(prompt: &str) -> Value {
    json!({
        "model": "gpt-5.6-codex",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "<environment_context>cwd=/repo</environment_context>"}]},
            {"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAAopaque", "summary": []},
            {"type": "function_call", "name": "shell", "call_id": "c1",
             "arguments": "{\"command\":[\"bash\",\"-lc\",\"cat src/parser.py\"]}"},
            {"type": "function_call_output", "call_id": "c1",
             "output": (0..300).map(|i| format!("{i}: line of file content\n")).collect::<String>()},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": prompt}]},
        ],
    })
}

// ── criterion 1 + 4 + 5: role protection, boundaries, instruction precedence ─

#[test]
fn codex_user_prompt_survives_a_cut_everything_curator() {
    let prompt = sentinel_prompt(12);
    let (out, refused) = serve_responses(&codex_body(&prompt));
    let served = out["input"][4]["content"][0]["text"].as_str().unwrap();
    assert_all_sentinels(served, 12, "codex user turn");
    assert_eq!(served, prompt, "user turn must be byte-identical");
    assert!(
        refused > 0,
        "the curator did try to cut it — this test would be vacuous otherwise"
    );
}

#[test]
fn anthropic_user_prompt_survives_a_cut_everything_curator() {
    let prompt = sentinel_prompt(12);
    let body = json!({
        "model": "claude-opus-5",
        "system": "You are Claude Code.",
        "messages": [
            {"role": "user", "content": "fix the failing test"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/repo/parser.py"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": (0..300).map(|i| format!("{i}: line of file content\n")).collect::<String>()}]}]},
            {"role": "user", "content": prompt.clone()},
        ],
    });
    let (out, refused) = serve_anthropic(&body);
    let served = out["messages"][3]["content"].as_str().unwrap();
    assert_all_sentinels(served, 12, "anthropic user turn");
    assert_eq!(served, prompt);
    assert!(refused > 0);
}

/// Product decision (2026-08-29): the agent's own prose responses are served
/// verbatim on both wires — only observations (and reasoning blobs) are
/// cuttable mass. Same fold-back mechanism as the user-turn guard, so brain
/// checksum parity is untouched.
#[test]
fn assistant_prose_survives_a_cut_everything_curator() {
    let prose = format!(
        "A00_HEAD_SENTINEL\n{}A99_TAIL_SENTINEL\n",
        (0..120)
            .map(|k| format!("finding {k}: a load-bearing line of the agent's answer\n"))
            .collect::<String>()
    );
    let body = json!({
        "model": "claude-opus-5",
        "messages": [
            {"role": "user", "content": "investigate the bug"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/repo/a.py"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": (0..300).map(|i| format!("{i}: file line\n")).collect::<String>()}]}]},
            {"role": "assistant", "content": prose.clone()},
            {"role": "user", "content": "now fix it"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "/repo/b.py"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t2", "content": [
                    {"type": "text", "text": (0..300).map(|i| format!("{i}: more file line\n")).collect::<String>()}]}]},
        ],
    });
    let (out, refused) = serve_anthropic(&body);
    assert_eq!(
        out["messages"][3]["content"].as_str().unwrap(),
        prose,
        "assistant prose must be byte-identical"
    );
    let obs = serde_json::to_string(&out["messages"][2]["content"]).unwrap();
    assert!(
        obs.contains(" omitted"),
        "tool output no longer curated: {obs}"
    );
    assert!(refused > 0, "the curator did try to cut the prose");

    // Responses wire: parse spares prose (cut_assistant=false on that path),
    // but the mask must protect it independently of freeze config — this
    // suite's serve_responses runs the default config, which DOES chunk it.
    let mut input = codex_body("go on")["input"].as_array().unwrap().clone();
    input.push(json!({"type": "message", "role": "assistant",
                      "content": [{"type": "output_text", "text": prose.clone()}]}));
    input.push(
        json!({"type": "function_call", "name": "shell", "call_id": "c2",
                      "arguments": "{\"command\":[\"bash\",\"-lc\",\"ls\"]}"}),
    );
    input.push(json!({"type": "function_call_output", "call_id": "c2",
                      "output": (0..200).map(|i| format!("{i}: output line\n")).collect::<String>()}));
    let body = json!({"model": "gpt-5.6-codex", "instructions": "You are Codex.", "input": input});
    let (out, _) = serve_responses(&body);
    assert_eq!(
        out["input"][5]["content"][0]["text"].as_str().unwrap(),
        prose,
        "responses-wire assistant prose must be byte-identical"
    );
}

// ── the guard must not gut the product ──────────────────────────────────────

#[test]
fn tool_output_is_still_curated_on_both_wires() {
    let (out, _) = serve_responses(&codex_body("go on"));
    let obs = out["input"][3]["output"].as_str().unwrap();
    assert!(
        obs.contains(" omitted"),
        "codex tool output not curated: {obs}"
    );
    assert!(obs.len() < 1000, "codex tool output not actually shrunk");

    let body = json!({
        "model": "claude-opus-5",
        "messages": [
            {"role": "user", "content": "fix it"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/repo/a.py"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": (0..300).map(|i| format!("{i}: file line\n")).collect::<String>()}]}]},
            {"role": "assistant", "content": [{"type": "text", "text": "reading"}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t2", "content": [
                    {"type": "text", "text": (0..300).map(|i| format!("{i}: more file line\n")).collect::<String>()}]}]},
        ],
    });
    let (out, refused) = serve_anthropic(&body);
    let obs = serde_json::to_string(&out["messages"][4]["content"]).unwrap();
    assert!(
        obs.contains(" omitted"),
        "anthropic tool_result not curated: {obs}"
    );
    assert_eq!(refused, 0, "no human content was in play here");
}

/// A `tool_result` with a system-reminder text block stapled on is an
/// OBSERVATION, not a human turn — it stays curatable, or the Anthropic path
/// loses the win it exists for.
#[test]
fn anthropic_mixed_tool_result_plus_reminder_stays_curatable() {
    let body = json!({
        "model": "claude-opus-5",
        "messages": [
            {"role": "user", "content": "start"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/repo/a.py"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": (0..300).map(|i| format!("{i}: file line\n")).collect::<String>()}]},
                {"type": "text", "text": "<system-reminder>be concise</system-reminder>"}]},
        ],
    });
    let mask = internal::protected_mask(&body);
    // No `system` key, so the mask aligns 1:1 with `messages`: the mixed
    // tool_result carrier is entry 2 (entry 1 is the assistant tool_use turn,
    // protected since the assistant-prose guard).
    assert!(!mask[2], "mixed tool_result message must stay curatable");
}

// ── criterion 2: byte integrity across content shapes ───────────────────────

#[test]
fn user_content_shapes_are_byte_identical() {
    let long_line = "x".repeat(20_000);
    let cases: Vec<(&str, String)> = vec![
        ("short", "hi".to_string()),
        ("unicode", "修正してください 🚀 — ünïcödé ✅\n".repeat(200)),
        (
            "json",
            format!(
                "```json\n{}\n```",
                json!({"k": (0..500).collect::<Vec<i32>>()})
            ),
        ),
        (
            "code block",
            format!(
                "```rust\n{}\n```",
                (0..400)
                    .map(|i| format!("    let v{i} = {i};\n"))
                    .collect::<String>()
            ),
        ),
        ("one long line", long_line),
        ("crlf", "line one\r\nline two\r\n".repeat(400)),
        (
            "trailing whitespace",
            "do the thing   \n\n\n   ".repeat(400),
        ),
    ];
    for (name, text) in cases {
        let (out, _) = serve_responses(&codex_body(&text));
        assert_eq!(
            out["input"][4]["content"][0]["text"].as_str().unwrap(),
            text,
            "responses wire mangled {name}"
        );

        let body = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": "start"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/repo/a.py"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": (0..300).map(|i| format!("{i}: file line\n")).collect::<String>()}]}]},
                {"role": "user", "content": [{"type": "text", "text": text}]},
            ],
        });
        let (out, _) = serve_anthropic(&body);
        assert_eq!(
            out["messages"][3]["content"][0]["text"].as_str().unwrap(),
            text,
            "anthropic wire mangled {name}"
        );
    }
}

// ── criterion 3: historical integrity ──────────────────────────────────────

#[test]
fn earlier_user_turns_stay_intact_as_the_conversation_grows() {
    let first = sentinel_prompt(6);
    let mut input = codex_body(&first)["input"].as_array().unwrap().clone();
    let mut follow_ups: Vec<String> = Vec::new();
    for turn in 0..4 {
        input.push(json!({"type": "message", "role": "assistant",
                          "content": [{"type": "output_text", "text": format!("working, turn {turn}")}]}));
        input.push(
            json!({"type": "function_call", "name": "shell", "call_id": format!("c{turn}"),
                          "arguments": "{\"command\":[\"bash\",\"-lc\",\"ls\"]}"}),
        );
        input.push(json!({"type": "function_call_output", "call_id": format!("c{turn}"),
                          "output": (0..200).map(|i| format!("{i}: output line\n")).collect::<String>()}));
        // Substantial, with the marker in the MIDDLE: a one-liner is too small
        // to be chunked, and a marker on the last line survives the digest's
        // keep-the-tail rule — either would pass with the guard removed.
        let follow = format!(
            "follow-up {turn} begins\n{}KEEPME_{turn}\n{}follow-up {turn} ends\n",
            (0..120)
                .map(|k| format!("requirement {turn}.{k} must hold for the whole change\n"))
                .collect::<String>(),
            (0..120)
                .map(|k| format!("rationale {turn}.{k} explaining why it must hold\n"))
                .collect::<String>()
        );
        input.push(json!({"type": "message", "role": "user",
                          "content": [{"type": "input_text", "text": follow}]}));
        follow_ups.push(follow);
    }
    let body = json!({
        "model": "gpt-5.6-codex",
        "instructions": "You are Codex.",
        "input": input,
    });
    let (out, _) = serve_responses(&body);
    let served = out["input"][4]["content"][0]["text"].as_str().unwrap();
    assert_all_sentinels(served, 6, "first user turn after 4 more turns");
    assert_eq!(served, first, "the first user turn drifted");
    for (turn, follow) in follow_ups.iter().enumerate() {
        let idx = 5 + turn * 4 + 3;
        let t = out["input"][idx]["content"][0]["text"].as_str().unwrap();
        assert_eq!(t, follow, "follow-up user turn {turn} was cut");
    }
}

// ── criterion 9: fail closed on shapes we do not recognize ─────────────────

#[test]
fn unrecognized_and_malformed_shapes_are_left_alone() {
    let body = json!({
        "model": "gpt-5.6-codex",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "start"}]},
            {"type": "computer_call", "call_id": "x1", "action": {"type": "screenshot"}},
            {"type": "item_reference", "id": "ref_1"},
            {"type": "message", "role": "user", "content": "bare string content"},
            {"role": "user", "content": [{"type": "input_text", "text": "typeless but roled"}]},
            {"type": "function_call_output", "call_id": "c9", "output": "small"},
        ],
    });
    let (out, refused) = serve_responses(&body);
    for i in [1usize, 2, 3, 4] {
        assert_eq!(
            out["input"][i], body["input"][i],
            "input[{i}] must ride through untouched"
        );
    }
    assert_eq!(refused, 0);
}

// ── the ledger alarm ───────────────────────────────────────────────────────

#[test]
fn refused_cuts_are_countable_and_roles_are_broken_out() {
    let body = codex_body(&sentinel_prompt(12));
    let internal = responses::to_internal(&body);
    let mask = responses::protected_mask(&body);
    let mut fz = Freezer::new(cut_at_birth(), CutAll);
    let mut served = fz.serve(&internal).unwrap();
    let refused = protect::restore_protected(&internal, &mut served, &mask);
    let by_role = protect::cut_by_role(&internal, &served);
    assert!(refused > 0, "the refusal must be visible in the ledger");
    assert_eq!(
        by_role.get("user"),
        None,
        "post-guard, no user-role tokens may be cut: {by_role:?}"
    );
    assert!(
        by_role.get("tool").copied().unwrap_or(0) > 0,
        "tool output should still show as cut: {by_role:?}"
    );
}

// ── Codex's actual tool shapes (captured from a real ~/.codex rollout) ──────

/// Codex 0.147 does not emit `function_call`/`function_call_output` for its
/// shell tool — it emits `custom_tool_call` with a JavaScript snippet, and
/// `custom_tool_call_output` whose parts are `input_text`. Those fell to the
/// `opaque` arm, so the curator saw NO tool output on this wire and the only
/// cuttable mass in a Codex conversation was the user's own prose.
#[test]
fn codex_custom_tool_output_is_curatable_and_the_prompt_still_is_not() {
    let prompt = sentinel_prompt(8);
    let big_output: String = (0..400).map(|i| format!("{i}: output line\n")).collect();
    let body = json!({
        "model": "gpt-5.6-sol",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "<recommended_plugins>…</recommended_plugins>"}]},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "reading the file"}]},
            {"type": "custom_tool_call", "id": "ctc_1", "status": "completed",
             "call_id": "call_1", "name": "exec",
             "input": "const r = await tools.exec_command({cmd:\"sed -n '1,240p' src/parser.py\",\"workdir\":\"/repo\"});text(r.output)\n"},
            {"type": "custom_tool_call_output", "id": "ctco_1", "call_id": "call_1",
             "output": [{"type": "input_text", "text": big_output}]},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": prompt}]},
        ],
    });

    let internal = responses::to_internal(&body);
    assert_eq!(
        internal[4]["role"], "tool",
        "tool output must not be opaque"
    );
    assert_eq!(
        internal[3]["extra"]["actions"][0]["command"], "sed -n '1,240p' src/parser.py",
        "the bash twin must see the command inside the JS snippet"
    );

    let (out, _refused) = serve_responses(&body);
    // The tool output IS digested…
    let served_out = serde_json::to_string(&out["input"][3]["output"]).unwrap();
    assert!(
        served_out.contains(" omitted"),
        "tool output not curated: {served_out}"
    );
    assert!(
        served_out.len() < big_output.len() / 2,
        "tool output not shrunk"
    );
    // …and the human turn after it is untouched.
    assert_eq!(
        out["input"][4]["content"][0]["text"].as_str().unwrap(),
        prompt
    );
    // The call item itself rides through verbatim — pairing survives.
    assert_eq!(out["input"][2], body["input"][2]);
}

/// The command scrape handles the shapes actually seen, and refuses to invent
/// one when there is nothing to find.
#[test]
fn tool_call_command_extraction_covers_the_real_shapes() {
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({"type": "custom_tool_call", "name": "exec",
                   "input": "const r = await tools.exec_command({cmd:\"grep -rn foo src/\",\"workdir\":\"/r\"});"}),
            "grep -rn foo src/",
        ),
        (
            json!({"type": "local_shell_call", "action": {"type": "exec",
                   "command": ["bash", "-lc", "cat README.md"]}}),
            "bash -lc cat README.md",
        ),
        (
            json!({"type": "function_call", "name": "shell", "call_id": "c",
                   "arguments": "{\"command\":[\"bash\",\"-lc\",\"ls -la\"]}"}),
            // argv joins as-is on the documented path — unchanged behaviour.
            "bash -lc ls -la",
        ),
        (
            json!({"type": "custom_tool_call", "name": "exec",
                   "input": "const r = await tools.exec_command({cmd:\"echo \\\"hi there\\\"\"});"}),
            "echo \"hi there\"",
        ),
    ];
    for (item, want) in cases {
        let e = responses::to_internal(&json!({"input": [item.clone()]}));
        assert_eq!(
            e[0]["extra"]["actions"][0]["command"].as_str().unwrap(),
            want,
            "wrong command for {item}"
        );
    }
}
