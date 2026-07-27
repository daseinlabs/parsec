//! Parsec terminal branding — the phosphor palette and the notice panel.
//!
//! Colours are the CLI subset of `brand/parsec-brand-kit/tokens/tokens.json`;
//! keep them in sync with `brand/parsec-brand-kit/cli/cli-theme.js`, which is
//! the same palette for Node consumers.
//!
//! Everything here emits truecolor SGR. Hook stdout is a pipe into Claude
//! Code, never a tty, so colour must NOT be gated on `isatty` — Claude Code
//! renders the string we hand it. `NO_COLOR` (any value, per no-color.org)
//! and `TERM=dumb` still turn it off.

/// `#4AF626` — the brand phosphor green.
const PHOSPHOR: (u8, u8, u8) = (0x4A, 0xF6, 0x26);
/// `#2E7D46` — dim green, for rules and borders.
const DIM: (u8, u8, u8) = (0x2E, 0x7D, 0x46);
/// `#8CA897` — muted text, for the secondary line.
const MUTED: (u8, u8, u8) = (0x8C, 0xA8, 0x97);

/// The parsec mark. U+2301 is what the status line already ships, so it is
/// the one glyph we know renders at width 1 in the terminals users are on.
pub const MARK: char = '⌁';

/// Widest panel we will draw. Narrow enough to survive a split pane.
const MAX_WIDTH: usize = 68;

/// How a panel body line is coloured.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Muted,
    Brand,
}

/// The mark as terminal art: two rules converging on the four-point star,
/// which is exactly what `logo/svg/parsec-mark-flat.svg` draws (two lines
/// from x=44 into the star at 194,80). Same five rows as `banner()` in
/// `brand/parsec-brand-kit/cli/cli-theme.js` — that file is the brand's own
/// terminal rendering, so this matches it rather than inventing a second one.
///
/// Returned unstyled, for [`panel`]'s `^` convention to colour. Rows are
/// padded to equal width so the art block stays rectangular.
pub fn logo() -> Vec<String> {
    ["  ╲", "   ╲", "    ✦", "   ╱", "  ╱"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The logo with the wordmark and tagline set beside the star.
pub fn logo_with_wordmark() -> Vec<String> {
    let mut rows = logo();
    rows[2] = format!("{}  parsec", rows[2]);
    rows[3] = format!("{}  2× context · ½ cost", rows[3]);
    rows
}

fn colors_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}

fn paint(s: &str, (r, g, b): (u8, u8, u8), bold: bool) -> String {
    if !colors_enabled() {
        return s.to_string();
    }
    let weight = if bold { "1;" } else { "" };
    format!("\x1b[{weight}38;2;{r};{g};{b}m{s}\x1b[0m")
}

pub fn phosphor(s: &str) -> String {
    paint(s, PHOSPHOR, true)
}
pub fn muted(s: &str) -> String {
    paint(s, MUTED, false)
}

/// One OSC 8 hyperlink: `text` rendered as a click target for `url`.
///
/// The escapes occupy zero columns, so a linkified line still measures with
/// [`display_width`] — that is what lets [`panel`] pad rows from the plain
/// text and hyperlink afterwards. Terminals that do not implement OSC 8
/// ignore it and show `text`; because we only ever link a full `https://…`
/// URL that is also its own display text, those terminals still auto-detect
/// it. Suppressed under `NO_COLOR`/`TERM=dumb` alongside the SGR.
fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\\x1b[4m{text}\x1b[24m\x1b]8;;\x1b\\")
}

/// Make every bare `http(s)://…` token in `s` clickable. Trailing sentence
/// punctuation is left outside the link so `see https://x.dev.` still points
/// at `https://x.dev`.
pub fn linkify(s: &str) -> String {
    linkify_with(s, colors_enabled())
}

/// [`linkify`] with the escapes-allowed decision injected, so it is testable
/// without racing the process-wide `NO_COLOR`.
fn linkify_with(s: &str, escapes: bool) -> String {
    if !escapes {
        return s.to_string();
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = [rest.find("https://"), rest.find("http://")]
        .into_iter()
        .flatten()
        .min()
    {
        let (head, tail) = rest.split_at(start);
        out.push_str(head);
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let (token, after) = tail.split_at(end);
        let url = token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        out.push_str(&hyperlink(url, url));
        out.push_str(&token[url.len()..]);
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A titled panel:
///
/// ```text
/// ╭─ ⌁ parsec ──────────────────────────╮
/// │ first-run setup started              │
/// │ ~1.3 GB embedder downloading         │
/// ╰──────────────────────────────────────╯
/// ```
///
/// Body lines are wrapped to the panel width. Two prefixes are honoured, and
/// stripped before measuring:
///
/// - `~` renders the line muted (the "undo / opt out" convention).
/// - `^` renders it phosphor and skips wrapping — for [`logo`] art, whose
///   leading spaces are load-bearing.
///
/// A body entry containing `\n` becomes that many panel lines: the newlines a
/// call site writes are structure (the get-a-key URL owns its own row), not
/// incidental whitespace, so they survive the wrap.
///
/// Colour and hyperlinks are applied after measuring, never before: SGR and
/// OSC escapes would otherwise count toward the width. Padding uses
/// [`display_width`], not `chars().count()`, because real call sites pass
/// emoji (`apikey::gate_banner`'s ⚠️) and a scalar count leaves those rows
/// short.
pub fn panel(title: &str, body: &[&str]) -> String {
    let wrapped: Vec<(String, Tone)> = body
        .iter()
        .flat_map(|line| match line.strip_prefix('^') {
            // Art: preserved verbatim, leading whitespace and all.
            Some(art) => vec![(art.to_string(), Tone::Brand)],
            None => line
                .split('\n')
                .flat_map(|line| {
                    let (text, tone) = match line.strip_prefix('~') {
                        Some(rest) => (rest.trim_start(), Tone::Muted),
                        None => (line, Tone::Plain),
                    };
                    wrap(text, MAX_WIDTH - 4)
                        .into_iter()
                        .map(move |l| (l, tone))
                })
                .collect(),
        })
        .collect();

    // ╭─ ⌁ parsec ─╮  →  4 fixed cells + mark + space + title
    let head = format!("{MARK} {title}");
    let inner = wrapped
        .iter()
        .map(|(l, _)| display_width(l))
        .chain(std::iter::once(display_width(&head) + 2))
        .max()
        .unwrap_or(0)
        .min(MAX_WIDTH - 4);

    // Every row is `inner + 4` columns: the two border cells plus the two
    // spaces that pad the content. The header spends 4 of those on
    // "╭─ " and "╮", plus one space after the title.
    let mut out = String::new();
    let rule = "─".repeat(inner.saturating_sub(display_width(&head) + 1));
    out.push_str(&phosphor(&format!("╭─ {head} {rule}╮")));
    for (line, tone) in &wrapped {
        let pad = " ".repeat(inner.saturating_sub(display_width(line)));
        let linked = linkify(line);
        let body = match tone {
            Tone::Muted => muted(&linked),
            Tone::Brand => phosphor(&linked),
            Tone::Plain => linked,
        };
        out.push('\n');
        out.push_str(&paint("│", DIM, false));
        out.push_str(&format!(" {body}{pad} "));
        out.push_str(&paint("│", DIM, false));
    }
    out.push('\n');
    out.push_str(&phosphor(&format!("╰{}╯", "─".repeat(inner + 2))));
    out
}

/// Render hook messages as one branded notice panel.
///
/// Call sites write their copy with the `⌁ parsec:` prefix so it still reads
/// correctly if it ever reaches a plain surface; inside the panel that prefix
/// is redundant, so it is stripped here. Multiple messages become multiple
/// paragraphs in one panel — never several stacked boxes.
///
/// **The leading newline is load-bearing.** Claude Code renders a hook's
/// `systemMessage` as `SessionStart:startup says: <text>`, and that prefix
/// indents only the first line — which put the panel's top border out of
/// alignment with every row beneath it. Starting on a fresh line lets the
/// whole box sit at column 0.
pub fn notice(msgs: &[String]) -> String {
    let stripped: Vec<String> = msgs
        .iter()
        .map(|m| {
            m.trim()
                .strip_prefix(MARK)
                .map(|r| r.trim_start())
                .and_then(|r| r.strip_prefix("parsec"))
                .map(|r| r.trim_start_matches([':', ' ']))
                .unwrap_or(m)
                .to_string()
        })
        .collect();
    let mut body: Vec<String> = logo_with_wordmark()
        .into_iter()
        .map(|l| format!("^{l}"))
        .collect();
    body.push(String::new());
    body.extend(stripped);
    let body: Vec<&str> = body.iter().map(String::as_str).collect();
    format!("\n{}", panel("parsec", &body))
}

/// Terminal columns `s` occupies. A deliberately small approximation of
/// UAX #11 — enough for our copy (ASCII, box drawing, an occasional emoji)
/// without taking on a `unicode-width` dependency.
///
/// The emoji case is genuinely terminal-dependent: `⚠` alone is one column,
/// but `⚠\u{FE0F}` requests emoji presentation and renders as two in every
/// modern terminal we target, so VS-16 contributes the second column.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    match c {
        // VS-16: promotes the preceding base character to emoji presentation.
        '\u{FE0F}' => 1,
        // Zero-width: ZWJ, the text-presentation selectors, combining marks.
        '\u{200B}'..='\u{200D}' | '\u{FE00}'..='\u{FE0E}' => 0,
        '\u{0300}'..='\u{036F}' | '\u{20D0}'..='\u{20FF}' => 0,
        // East Asian Wide / Fullwidth, plus the emoji blocks that are wide
        // without needing a selector.
        '\u{1100}'..='\u{115F}'
        | '\u{2E80}'..='\u{303E}'
        | '\u{3041}'..='\u{33FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{A000}'..='\u{A4CF}'
        | '\u{AC00}'..='\u{D7A3}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FE30}'..='\u{FE6F}'
        | '\u{FF00}'..='\u{FF60}'
        | '\u{FFE0}'..='\u{FFE6}'
        | '\u{1F300}'..='\u{1F64F}'
        | '\u{1F900}'..='\u{1F9FF}' => 2,
        _ => 1,
    }
}

/// Greedy word wrap. Words longer than `width` are left over-long rather than
/// broken — they are paths and commands, and a broken path is worse than a
/// ragged edge.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let need = if cur.is_empty() {
            display_width(word)
        } else {
            display_width(&cur) + 1 + display_width(word)
        };
        if !cur.is_empty() && need > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR *and* OSC 8 so tests assert on geometry, not on escapes.
    /// Both are zero-width, so what survives is exactly what the terminal
    /// draws.
    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                // CSI …m (colour) — ends at 'm'.
                '\x1b' if chars.peek() == Some(&'[') => {
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                }
                // OSC 8 …ST — ends at ESC \ (or BEL).
                '\x1b' if chars.peek() == Some(&']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// Rendered column width of every row, escapes removed.
    fn rows(p: &str) -> Vec<usize> {
        plain(p).lines().map(display_width).collect()
    }

    #[test]
    fn panel_rows_are_all_the_same_width() {
        let p = panel(
            "parsec",
            &["short", "a much longer line than the first", "~undo: x"],
        );
        let widths = rows(&p);
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "ragged panel: {widths:?}"
        );
    }

    /// Regression: `apikey::gate_banner` opens with "⚠️", whose U+FE0F is a
    /// zero-width scalar. Counting chars left that row a column short.
    #[test]
    fn emoji_and_wide_characters_do_not_skew_the_border() {
        for line in [
            "⚠️ parsec: NO API KEY — savings are OFF.",
            "plain ascii line",
            "⌁ mark and a 漢字 pair",
        ] {
            let widths = rows(&panel("parsec", &[line]));
            assert!(
                widths.windows(2).all(|w| w[0] == w[1]),
                "ragged for {line:?}: {widths:?}"
            );
        }
    }

    #[test]
    fn display_width_counts_columns_not_scalars() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("⌁"), 1);
        // base + VS-16 = two scalars, two columns.
        assert_eq!("⚠️".chars().count(), 2);
        assert_eq!(display_width("⚠️"), 2);
        assert_eq!(display_width("漢字"), 4);
        // Combining acute is zero-width.
        assert_eq!(display_width("e\u{0301}"), 1);
    }

    #[test]
    fn panel_wraps_and_never_exceeds_max_width() {
        let long = "downloading the local embedder to ~/.parsec/models in the \
                    background and then rewriting settings";
        let p = panel("parsec", &[long]);
        let rendered = plain(&p);
        assert!(rendered.lines().count() > 3, "expected a wrap: {rendered}");
        for l in rendered.lines() {
            assert!(display_width(l) <= MAX_WIDTH, "too wide: {l}");
        }
    }

    #[test]
    fn title_always_fits_even_with_no_body() {
        let widths = rows(&panel("parsec", &[]));
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn no_color_env_suppresses_all_escapes() {
        // Serialised with the other env-reading tests by construction: this
        // is the only test that sets NO_COLOR, and it restores it.
        let prev = std::env::var_os("NO_COLOR");
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let p = panel("parsec", &["hello"]);
        assert!(!p.contains('\x1b'), "escapes leaked under NO_COLOR: {p:?}");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("NO_COLOR", v),
                None => std::env::remove_var("NO_COLOR"),
            }
        }
    }

    #[test]
    fn notice_starts_on_its_own_line_and_keeps_the_box_square() {
        // Claude Code prefixes the first line with "SessionStart:… says: ",
        // so the panel must not begin on that line.
        let out = notice(&["⌁ parsec: hello".into()]);
        assert!(out.starts_with('\n'), "no leading newline: {out:?}");
        let widths: Vec<usize> = plain(&out)
            .lines()
            .filter(|l| !l.is_empty())
            .map(display_width)
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn notice_includes_the_logo_and_wordmark() {
        let out = plain(&notice(&["⌁ parsec: hello".into()]));
        assert!(out.contains('✦'), "no star: {out}");
        assert!(out.contains("╲") && out.contains("╱"), "no chevron: {out}");
        assert!(out.contains("2× context"), "no tagline: {out}");
    }

    #[test]
    fn notice_strips_the_redundant_prefix_and_makes_one_panel() {
        let out = plain(&notice(&[
            "⌁ parsec: routing restored".into(),
            "⌁ parsec — saved 1.2M tokens".into(),
        ]));
        assert_eq!(out.matches('╭').count(), 1, "stacked panels: {out}");
        assert!(out.contains("routing restored"));
        // "⌁ parsec:" gone, but an em-dash variant keeps its separator.
        assert!(!out.contains("parsec: routing"), "{out}");
        assert!(out.contains("— saved 1.2M tokens"), "{out}");
    }

    #[test]
    fn notice_leaves_unprefixed_copy_alone() {
        let out = plain(&notice(&["something else entirely".into()]));
        assert!(out.contains("something else entirely"), "{out}");
    }

    #[test]
    fn urls_are_click_targets_that_do_not_skew_the_border() {
        let url = "https://app.getparsec.ai";
        // The click target is emitted…
        let linked = linkify_with(&format!("→ Get your key:  {url}"), true);
        assert!(
            linked.contains(&format!("\x1b]8;;{url}\x1b\\")),
            "{linked:?}"
        );
        // …the visible text is still the bare URL, so terminals without OSC 8
        // auto-detect it…
        assert!(plain(&linked).contains(url));
        // …and the zero-width escapes did not push the panel row out.
        let widths = rows(&panel(
            "parsec",
            &[&format!("→ Get your key:  {url}"), "plain"],
        ));
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "ragged: {widths:?}"
        );
    }

    #[test]
    fn linkify_leaves_trailing_punctuation_outside_the_link() {
        let out = linkify_with("see https://x.dev, ok", true);
        assert!(out.contains("\x1b]8;;https://x.dev\x1b\\"), "{out:?}");
        assert_eq!(plain(&out), "see https://x.dev, ok");
        // Nothing to link -> byte-identical passthrough.
        assert_eq!(linkify_with("no links here", true), "no links here");
        // Escapes off (NO_COLOR / TERM=dumb) -> untouched.
        assert_eq!(
            linkify_with("see https://x.dev", false),
            "see https://x.dev"
        );
    }

    #[test]
    fn explicit_newlines_become_their_own_rows() {
        // The get-a-key URL must own the first line, not flow into a
        // paragraph with the warning beneath it.
        let p = plain(&panel(
            "parsec",
            &["https://app.getparsec.ai\nsecond\n\nfourth"],
        ));
        let body: Vec<&str> = p.lines().skip(1).take(4).collect();
        assert!(body[0].contains("https://app.getparsec.ai"), "{p}");
        assert!(body[1].contains("second"), "{p}");
        assert!(body[2].trim_matches(['│', ' ']).is_empty(), "{p}");
        assert!(body[3].contains("fourth"), "{p}");
    }

    #[test]
    fn long_word_is_not_broken() {
        let path = "/very/long/path/that/exceeds/the/panel/width/by/quite/a/lot/indeed/parsec";
        let p = plain(&panel("parsec", &[path]));
        assert!(p.contains(path), "path was broken across lines: {p}");
    }
}
