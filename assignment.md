# Parsec Intern Assignment: Test Engineering 🧪

Welcome to the Parsec team! In this repository, **determinism, reliability, and edge-case correctness** are critical. Rather than building new features, this assignment focuses on a core engineering skill: **understanding existing code and writing thorough, robust unit tests**.

---

## 🎯 Objectives

1. **Fork** the repository and set up your local development environment.
2. Verify that the existing test suite passes cleanly.
3. Explore the codebase and write comprehensive unit tests for core modules where edge cases and coverage can be improved.
4. Ensure all linters, formatting, and tests pass (`cargo fmt`, `cargo clippy`, `cargo test`).
5. Open a Pull Request on your fork with clear explanations of the edge cases you identified and tested.

---

## 🛠️ Step 1: Environment Setup

### Prerequisites
- **Git**
- **Rust toolchain** (Rust 1.80+, managed via [rustup](https://rustup.rs/))

### Clone & Verify Build
1. Fork the repo on GitHub, then clone your fork:
   ```bash
   git clone https://github.com/<your-username>/parsec.git
   cd parsec
   ```
2. Verify that the existing tests build and pass:
   ```bash
   cargo test --workspace
   ```
3. Verify that code formatting and linter checks pass:
   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --no-deps -- -D warnings
   ```

---

## 📝 Step 2: The Assignment Tasks

Your goal is to add new unit tests targeting edge cases across one or more of the following key modules in `packages/engine`:

### Area 1: Python String Semantics (`packages/engine/src/pystr.rs`)

`pystr.rs` mirrors Python string behavior in Rust so that chunk boundaries and token counts match Python reference implementations byte-for-byte.

**Target Functions to Test**:
- `py_splitlines`:
  - Splits on all 11 line break characters (e.g. `\n`, `\r\n`, `\x0b`, `\x0c`, `\x1c..\x1e`, `\u{85}`, `\u{2028}`, `\u{2029}`).
  - Correctly consumes `\r\n` as a single line break without creating empty trailing lines.
  - Consecutive newlines and strings starting/ending with newlines.
- `py_strip` and `py_is_space`:
  - Python whitespace set: standard unicode whitespace plus ASCII `\x1c..\x1f`.
  - Empty strings, strings with only whitespace, and strings with no whitespace.
- `char_prefix` and `char_len`:
  - Multi-byte UTF-8 sequences (e.g. accents, Japanese/Chinese characters, emoji with surrogate code points).
  - Prefix lengths exceeding string character length.
- `py_json_dumps_opts`:
  - JSON serialization with `sort_keys=true/false` and `ensure_ascii=true/false`.
  - Non-ASCII characters escaping vs. raw UTF-8 output.

---

### Area 2: Observation & Assistant Chunking (`packages/engine/src/chunking.rs`)

`chunking.rs` splits tool observations and assistant turns into discrete chunks. Every character must land in exactly one chunk in its original order (the full-coverage invariant).

**Target Functions to Test**:
- `parse_grep_candidate`:
  - Standard grep format `path/to/file.rs:42:matching line`.
  - Windows file paths (e.g. `C:\project\file.rs:10:code`).
  - Inputs with multiple colons in the content or file name.
  - Missing line numbers, non-numeric line numbers, or lines with empty text.
- `sed_base` and `head_window`:
  - Various `sed -n '10,20p'` command formats and flags.
  - Edge cases like inverted line ranges, non-numeric inputs, or unusual spacing.
- `chunk_observation` and `chunk_assistant`:
  - Observations with varying window sizes (`win`).
  - Empty strings vs. whitespace-only strings.
  - Very long single-line outputs vs. large multi-line outputs.
  - Verifying the **full-coverage invariant**: concatenating all chunk texts must reconstruct the original content without dropped characters.

---

### Area 3: Message Parsing & Extraction (`packages/engine/src/messages.rs`)

`messages.rs` parses conversation turns and Anthropic API message blocks.

**Target Functions to Test**:
- Text content extraction from single string blocks and arrays of content blocks.
- Tool call (`tool_use`) and tool response (`tool_result`) content parsing.
- Handling malformed JSON structures gracefully (e.g., unexpected data types or missing fields) without panicking.

---

## 🌟 Bonus / Stretch Goals (Optional)

- **Property-based Testing**: Use `proptest` (if you are familiar) to test invariants like `char_len(s) <= s.len()` or `py_splitlines` full-string reconstruction.
- **Bug Discovery**: If you find an edge case where a function produces unexpected behavior or panics, document it in a test with `#[ignore = "known edge case / bug: ..."]` or fix it with a minimal PR!

---

## 🔍 Step 3: Test Quality Guidelines

When writing your unit tests:
1. **Descriptive test names**: Name tests clearly after the scenario they test (e.g. `test_py_splitlines_handles_crlf_and_unicode_breaks`).
2. **Clear assertions**: Use `assert_eq!`, `assert!`, or descriptive error messages in assertions.
3. **No flakiness**: Tests must be deterministic and run in memory without network or filesystem dependencies.
4. **Locate tests cleanly**: Place unit tests in the respective module's `#[cfg(test)] mod tests { ... }` block, or create a dedicated integration test file under `packages/engine/tests/`.

---

## ✅ Step 4: Verification Checklist

Before submitting, run these commands to verify that your code is formatted, lint-free, and passes all tests:

```bash
# 1. Check code formatting
cargo fmt --all --check

# 2. Run clippy linter (warnings are treated as errors)
cargo clippy --workspace --all-targets --no-deps -- -D warnings

# 3. Run all tests (including your new tests)
cargo test --workspace
```

---

## 📬 Step 5: Submission Instructions

1. Create a feature branch for your work:
   ```bash
   git checkout -b intern/<your-name>-test-suite
   ```
2. Commit your new tests with a descriptive commit message:
   ```bash
   git add .
   git commit -m "engine: add unit tests for pystr and chunking edge cases"
   ```
3. Push the branch to your fork and create a Pull Request against `main`.
4. In your PR description, summarize:
   - Which modules you wrote tests for.
   - What tricky edge cases or boundary conditions you tested.
   - The test run output (`cargo test` summary).

---

## 💡 Helpful Hints

- Look at existing tests in `packages/engine/src/chunking.rs` and `packages/engine/tests/` for inspiration.
- You can run tests for just one crate or test name:
  ```bash
  cargo test -p parsec-engine pystr
  cargo test -p parsec-engine chunking
  ```
- Have fun hunting down edge cases! 🎯
