//! Edge-case tests for `chunking` — grep-line parsing and the
//! observation/assistant chunkers. `parse_grep_candidate`, `chunk_observation`
//! and `chunk_assistant` are the module's public surface; `sed_base` is
//! private and is tested inside `src/chunking.rs`'s own `mod tests` instead.

use parsec_engine::chunking::{
    chunk_assistant, chunk_observation, parse_grep_candidate, ChunkMode,
};

// ---------------------------------------------------------------------------
// parse_grep_candidate — standard format
// ---------------------------------------------------------------------------

#[test]
fn test_parse_grep_candidate_standard_grep_format() {
    assert_eq!(
        parse_grep_candidate("src/app.py:42:    return x"),
        Some(("app.py".to_string(), Some(42)))
    );
}

#[test]
fn test_parse_grep_candidate_strips_directory_to_basename() {
    let got = parse_grep_candidate("a/b/c/deep/module.py:1:import os").unwrap();
    assert_eq!(got.0, "module.py");
}

/// A colon inside the matched content (e.g. a Python dict literal or a
/// ternary) must not confuse the file:line split — splitn(3, ':') caps the
/// number of splits so everything after the second colon stays together.
#[test]
fn test_parse_grep_candidate_extra_colons_in_content_are_preserved_as_one_field() {
    assert_eq!(
        parse_grep_candidate("src/app.py:10:if x == 1: return {\"a\": 1}"),
        Some(("app.py".to_string(), Some(10)))
    );
}

#[test]
fn test_parse_grep_candidate_missing_line_number() {
    // grep -l style: just a filename, or file:content with no numeric field.
    assert_eq!(
        parse_grep_candidate("src/app.py:no matches found here"),
        Some(("app.py".to_string(), None))
    );
}

#[test]
fn test_parse_grep_candidate_non_numeric_line_field_falls_back_to_file_only() {
    assert_eq!(
        parse_grep_candidate("src/app.py:abc:some content"),
        Some(("app.py".to_string(), None))
    );
}

#[test]
fn test_parse_grep_candidate_tolerates_whitespace_around_the_line_number() {
    assert_eq!(
        parse_grep_candidate("src/app.py: 42 :content"),
        Some(("app.py".to_string(), Some(42)))
    );
}

/// A match on an empty line still carries its line number.
#[test]
fn test_parse_grep_candidate_empty_trailing_content_keeps_line_number() {
    assert_eq!(
        parse_grep_candidate("src/app.py:5:"),
        Some(("app.py".to_string(), Some(5)))
    );
}

#[test]
fn test_parse_grep_candidate_empty_and_reranked_lines_return_none() {
    assert_eq!(parse_grep_candidate(""), None);
    assert_eq!(parse_grep_candidate("   "), None);
    assert_eq!(parse_grep_candidate("[reranked results below]"), None);
}

/// A line with no path-like token at all (no '/' and no dotted extension).
#[test]
fn test_parse_grep_candidate_no_path_token_returns_none() {
    assert_eq!(parse_grep_candidate("just some prose, no file here"), None);
}

// ---------------------------------------------------------------------------
// parse_grep_candidate — Windows paths (genuine limitation found in the
// current implementation, not a hypothetical)
// ---------------------------------------------------------------------------

/// `is_path` only recognizes `/` as a directory separator, and the fallback
/// branch's `EXT` check requires a dotted extension at the literal end of
/// the whitespace-delimited token. A backslash-only Windows path followed by
/// `:line:content` satisfies neither: no `/` anywhere, and the token's last
/// character is the tail of the match content, not an extension. The whole
/// line is silently dropped rather than misparsed.
#[test]
fn test_parse_grep_candidate_backslash_windows_path_is_not_recognized() {
    assert_eq!(parse_grep_candidate(r"C:\project\file.rs:10:code"), None);
    // Trailing colon with empty content behaves the same way.
    assert_eq!(parse_grep_candidate(r"C:\project\file.rs:10:"), None);
}

/// BUG: a forward-slash Windows path (what ripgrep/git-bash actually emit on
/// Windows) has its own failure mode. The drive letter's colon ("C:") is an
/// extra colon the 3-way split doesn't expect: `splitn(3, ':')` consumes it
/// as the file/line separator, leaving `parts[0] == "C"` (fails `is_path`,
/// no extension/slash) and `parts[1] == "/project/file.rs"` (fails the
/// digit check). Both structured branches bail, and the code falls through
/// to the whitespace-token fallback, which DOES see the slashes and accepts
/// the token — but `basename()` then splits on the LAST '/', which lands
/// inside the digits-and-content tail, not before the extension. The result
/// is a mangled "filename" that swallows the line number and part of the
/// match text, and the real line number is lost (`None` instead of `Some`).
#[test]
fn test_parse_grep_candidate_drive_letter_forward_slash_path_is_mishandled() {
    let got = parse_grep_candidate("C:/project/file.rs:10:matching line").unwrap();
    // What actually comes out today:
    assert_eq!(got, ("file.rs:10:matching".to_string(), None));
    // What a correct parse would produce, for reference:
    // Some(("file.rs".to_string(), Some(10)))
    assert_ne!(
        got.0, "file.rs",
        "documents the current mis-split, not the desired one"
    );
}

/// Same failure mode, isolated to a bare file-only line (no extra prose) so
/// the corruption is easier to see: the basename still absorbs the drive
/// letter's line/content suffix instead of stopping at ".rs".
#[test]
fn test_parse_grep_candidate_drive_letter_path_no_trailing_prose() {
    let got = parse_grep_candidate("C:/project/file.rs:7:x").unwrap();
    assert_eq!(got.0, "file.rs:7:x");
    assert_eq!(got.1, None);
}

// ---------------------------------------------------------------------------
// chunk_observation — default (non-search, non-read) windowing
// ---------------------------------------------------------------------------

fn lines(n: usize) -> String {
    (1..=n)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_chunk_observation_default_window_splits_evenly() {
    // "python script.py" matches neither SEARCH nor READ, so it takes the
    // generic windowed path.
    let chunks = chunk_observation("python script.py", &lines(6), 0, 2, None, ChunkMode::Fixed);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "line1\nline2");
    assert_eq!(chunks[1].text, "line3\nline4");
    assert_eq!(chunks[2].text, "line5\nline6");
    assert!(chunks.iter().all(|c| c.file.is_none() && c.kind == "other"));
}

#[test]
fn test_chunk_observation_default_window_remainder_is_a_smaller_final_chunk() {
    let chunks = chunk_observation("python script.py", &lines(5), 0, 2, None, ChunkMode::Fixed);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[2].text, "line5");
}

/// A window larger than the content produces exactly one chunk holding
/// everything, regardless of how large `win` is.
#[test]
fn test_chunk_observation_window_larger_than_content_yields_one_chunk() {
    let chunks = chunk_observation(
        "python script.py",
        &lines(3),
        0,
        1000,
        None,
        ChunkMode::Fixed,
    );
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "line1\nline2\nline3");
}

/// A very long SINGLE-LINE observation (no newlines) is never split by
/// character count — only `py_splitlines` boundaries matter, so one huge
/// line is always exactly one chunk no matter how small `win` is.
#[test]
fn test_chunk_observation_long_single_line_is_never_split_by_length() {
    let huge = "x".repeat(50_000);
    let chunks = chunk_observation("python script.py", &huge, 0, 1, None, ChunkMode::Fixed);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text.len(), 50_000);
}

#[test]
fn test_chunk_observation_empty_string_yields_one_empty_chunk() {
    let chunks = chunk_observation("python script.py", "", 0, 40, None, ChunkMode::Fixed);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "");
}

/// Whitespace-only content has no line that passes `py_has_content`, so the
/// windowed loop pushes nothing — but the empty-output fallback then pushes
/// the ENTIRE original observation as a single chunk (whitespace included),
/// rather than zero chunks.
#[test]
fn test_chunk_observation_whitespace_only_falls_back_to_one_whole_chunk() {
    let ws = "   \n\t\n   \n";
    let chunks = chunk_observation("python script.py", ws, 0, 2, None, ChunkMode::Fixed);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, ws);
}

// ---------------------------------------------------------------------------
// chunk_observation — grep/search windowing
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_observation_grep_isolates_match_lines_from_surrounding_runs() {
    let obs =
        "searching...\nsrc/app.py:5:match one\nmore filler\nmore filler 2\nsrc/app.py:9:match two";
    let chunks = chunk_observation("grep -rn foo src/", obs, 0, 40, None, ChunkMode::Fixed);
    // "searching..." -> one 'other' run chunk before the first match.
    // Each match line -> its own 'grep' chunk with file/line metadata.
    // The filler lines between matches -> one 'other' run chunk.
    let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(kinds, vec!["other", "grep", "other", "grep"]);
    assert_eq!(chunks[1].file, Some("app.py".to_string()));
    assert_eq!(chunks[1].lo, Some(5));
    assert_eq!(chunks[3].lo, Some(9));
}

/// A non-match run longer than `win` is itself windowed.
#[test]
fn test_chunk_observation_grep_windows_long_non_match_runs() {
    let obs = format!("{}\nsrc/app.py:1:hit", lines(5));
    let chunks = chunk_observation("grep -n foo x", &obs, 0, 2, None, ChunkMode::Fixed);
    // 5 filler lines windowed by 2 -> 3 run chunks, then 1 grep chunk.
    let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(kinds, vec!["other", "other", "other", "grep"]);
}

#[test]
fn test_chunk_observation_grep_with_no_matches_falls_back_to_whole_observation() {
    let obs = "nothing matched here\nor here either";
    let chunks = chunk_observation("grep -n foo x", obs, 0, 40, None, ChunkMode::Fixed);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, "other");
    assert_eq!(chunks[0].text, obs);
}

// ---------------------------------------------------------------------------
// chunk_observation — structured read (read_lines: Some(g)) path
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_observation_read_lines_groups_by_g_with_file_metadata() {
    let obs = lines(6);
    let chunks = chunk_observation("cat file.py", &obs, 0, 40, Some(2), ChunkMode::Fixed);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].file, Some("file.py".to_string()));
    assert_eq!((chunks[0].lo, chunks[0].hi), (Some(1), Some(2)));
    assert_eq!((chunks[1].lo, chunks[1].hi), (Some(3), Some(4)));
    assert_eq!((chunks[2].lo, chunks[2].hi), (Some(5), Some(6)));
    assert!(chunks.iter().all(|c| c.kind == "read"));
}

/// Blank lines are dropped from the visible text, but the ORIGINAL file line
/// numbers are still preserved on the surviving lines either side of the
/// gap — so `hi - lo + 1` can legitimately exceed the number of text lines
/// actually present in the chunk. This is intentional (citations must point
/// at real file coordinates) but easy to mistake for an off-by-one bug.
#[test]
fn test_chunk_observation_read_lines_drops_blanks_but_keeps_true_coordinates() {
    let obs = "line1\n\nline3";
    let chunks = chunk_observation("cat file.py", obs, 0, 40, Some(2), ChunkMode::Fixed);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].text, "line1\nline3",
        "the blank line is not in the text"
    );
    assert_eq!(chunks[0].lo, Some(1));
    assert_eq!(
        chunks[0].hi,
        Some(3),
        "line3's TRUE file line number, not its visible position"
    );
}

/// `sed -n '5,Np'` shifts the base line number the read-lines grouping
/// starts counting from.
#[test]
fn test_chunk_observation_read_lines_respects_sed_base() {
    let obs = lines(2);
    let chunks = chunk_observation(
        "sed -n '10,11p' file.py",
        &obs,
        0,
        40,
        Some(2),
        ChunkMode::Fixed,
    );
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].lo, chunks[0].hi), (Some(10), Some(11)));
}

// ---------------------------------------------------------------------------
// chunk_observation — cmd/rc/head metadata applied uniformly
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_observation_attaches_rc_and_head_to_every_chunk() {
    let obs = "<returncode>1</returncode>\nsome output\nmore output";
    let chunks = chunk_observation("python script.py", obs, 0, 1, None, ChunkMode::Fixed);
    assert!(chunks.len() > 1, "sanity: multiple chunks were produced");
    for c in &chunks {
        assert_eq!(c.rc, Some(1));
        assert_eq!(c.cmd, "python script.py");
        assert!(c.head.starts_with("<returncode>1</returncode>"));
    }
}

// ---------------------------------------------------------------------------
// chunk_assistant
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_assistant_windows_like_chunk_observation_default_path() {
    let chunks = chunk_assistant(&lines(4), 0, 2);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "line1\nline2");
    assert_eq!(chunks[1].text, "line3\nline4");
    assert!(chunks.iter().all(|c| c.kind == "asst"));
}

/// Unlike `chunk_observation`, `chunk_assistant` has no empty-output
/// fallback: a purely empty or whitespace-only message legitimately
/// produces ZERO chunks, not one chunk containing the whitespace.
#[test]
fn test_chunk_assistant_empty_and_whitespace_only_yield_zero_chunks() {
    assert_eq!(chunk_assistant("", 0, 40), Vec::new());
    assert_eq!(chunk_assistant("   \n\t\n  ", 0, 40), Vec::new());
}

#[test]
fn test_chunk_assistant_single_line_message() {
    let chunks = chunk_assistant("just one line", 0, 40);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "just one line");
}
