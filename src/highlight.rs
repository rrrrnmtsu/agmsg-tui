//! Message body → styled `Line`s for the room pane.
//!
//! Why not a hand-rolled regex highlighter for fenced code: syntect gives us
//! real tokenizers for the languages agents actually paste (rust/python/ts/json/
//! yaml/bash), and `default-syntaxes` + `default-themes` keeps the binary small
//! (no bundled Sublime package downloads at build time).
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const DEFAULT_THEME_NAME: &str = "base16-ocean.dark";
/// syntect theme lookups happen against `AGMSG_TUI_THEME` when set (S10-4),
/// falling back to [`DEFAULT_THEME_NAME`] — the app is written dark-bg-first
/// (README calls this out explicitly), so a light theme still won't fix
/// unrelated dark-bg-assuming colors elsewhere in the UI, but at least the
/// syntax highlighting itself becomes theme-switchable.
const THEME_ENV_VAR: &str = "AGMSG_TUI_THEME";

// Phase 5 visual-strengthening palette. Kept as named consts (not theme
// lookups) because syntect themes describe token colors, not chrome like
// gutters/badges — there's no "gutter background" concept to borrow from
// `theme()` here. `pub(crate)` so `palette.rs`'s NO_COLOR buffer pass can
// recognize "this cell is fenced-code chrome" and skip stripping it.
pub(crate) const CODE_BG: Color = Color::Rgb(30, 30, 46);
const CODE_BAR_COLOR: Color = Color::Cyan;
const LABEL_BG: Color = Color::Rgb(180, 180, 50);
const LABEL_FG: Color = Color::Black;
const INLINE_CODE_BG: Color = Color::Rgb(50, 50, 60);
const LINE_NUMBER_THRESHOLD: usize = 5;

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

fn requested_theme_name() -> Option<String> {
    std::env::var(THEME_ENV_VAR).ok().filter(|value| !value.trim().is_empty())
}

/// `Some(name)` iff `AGMSG_TUI_THEME` names a theme syntect doesn't ship —
/// used by `App::load` to surface a one-line startup warning instead of
/// silently rendering with the default theme.
pub fn requested_theme_missing() -> Option<String> {
    let requested = requested_theme_name()?;
    (!theme_set().themes.contains_key(&requested)).then_some(requested)
}

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let themes = theme_set();
        requested_theme_name()
            .and_then(|name| themes.themes.get(&name).cloned())
            .or_else(|| themes.themes.get(DEFAULT_THEME_NAME).cloned())
            .unwrap_or_else(|| themes.themes.values().next().cloned().expect("theme"))
    })
}

fn syntax_for_token(token: &str) -> &'static SyntaxReference {
    let set = syntax_set();
    let normalized = normalize_lang(token);
    set.find_syntax_by_token(&normalized)
        .unwrap_or_else(|| set.find_syntax_plain_text())
}

/// syntect ships canonical tokens ("rust", "python", "yaml", "json", "bash");
/// this maps the aliases agents commonly type after ``` to those tokens.
/// Why owned String not `&'static str` via `Box::leak`: this runs once per
/// fenced block, not per frame, so the tiny allocation beats leaking memory
/// for every distinct unrecognized language tag a message ever used.
fn normalize_lang(token: &str) -> String {
    match token.trim().to_ascii_lowercase().as_str() {
        "rs" => "rust".to_owned(),
        "py" => "python".to_owned(),
        "ts" | "tsx" | "typescript" => "ts".to_owned(),
        "js" | "jsx" | "javascript" => "js".to_owned(),
        "sh" | "shell" | "zsh" => "bash".to_owned(),
        "yml" => "yaml".to_owned(),
        "" => "txt".to_owned(),
        other => other.to_owned(),
    }
}

fn syn_to_ratatui_color(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

/// Best-effort language detection for unlabeled fences (bare ```` ``` ````
/// with no lang tag), keyed off syntect's own first-line heuristics (shebangs,
/// `-*- Mode: X -*-` comments, XML doctype, etc).
fn detect_language(first_line: &str) -> Option<&'static SyntaxReference> {
    syntax_set().find_syntax_by_first_line(first_line)
}

/// Tokenizes code with syntect, returning per-line spans (no chrome yet —
/// background/bar/numbers are layered on by `decorate_code_block`).
/// Under `NO_COLOR` this skips syntect entirely and falls back to
/// [`plain_code_lines`] — the fenced block's own bg/bar chrome is a spec
/// exception (kept regardless), but per-token syntax colors are not, so
/// there is no reason to pay the tokenizer cost only to discard its colors.
fn tokenize_code_lines(syntax: &'static SyntaxReference, code: &str) -> Vec<Vec<Span<'static>>> {
    if crate::palette::current().no_color {
        return plain_code_lines(code);
    }
    let mut highlighter = HighlightLines::new(syntax, theme());
    let set = syntax_set();
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges: Vec<(SynStyle, &str)> = match highlighter.highlight_line(line, set) {
            Ok(ranges) => ranges,
            Err(_) => vec![(SynStyle::default(), line)],
        };
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| {
                let text = text.trim_end_matches(['\n', '\r']).to_owned();
                Span::styled(text, Style::default().fg(syn_to_ratatui_color(style.foreground)))
            })
            .collect();
        lines.push(spans);
    }
    if lines.is_empty() {
        lines.push(vec![Span::raw(String::new())]);
    }
    lines
}

/// Untokenized fallback for `[plain]` blocks (no lang tag, no first-line
/// match) — one raw span per line, still gets the bg/bar/number treatment.
fn plain_code_lines(code: &str) -> Vec<Vec<Span<'static>>> {
    let mut lines: Vec<Vec<Span<'static>>> =
        code.lines().map(|line| vec![Span::raw(line.to_owned())]).collect();
    if lines.is_empty() {
        lines.push(vec![Span::raw(String::new())]);
    }
    lines
}

/// Layers the Phase 5 "this is a code block" chrome on top of already-
/// tokenized (or plain) line spans: dark navy background on every line, a
/// cyan left-edge bar, a right-aligned `[lang]` badge above the first line,
/// and dim right-aligned line numbers once the block is long enough that
/// losing your place while scrolling is a real risk (>5 lines).
fn decorate_code_block(token_lines: Vec<Vec<Span<'static>>>, label: &str) -> Vec<Line<'static>> {
    let no_color = crate::palette::current().no_color;
    let show_numbers = token_lines.len() > LINE_NUMBER_THRESHOLD;
    // The bar + CODE_BG are the "this is a code block" chrome the NO_COLOR
    // spec explicitly keeps; unlike them, the `[lang]` badge's yellow-on-
    // black is plain decoration, so it drops color under NO_COLOR (BOLD
    // alone still sets it apart from the code text below).
    let bar = || Span::styled("▊", Style::default().fg(CODE_BAR_COLOR).bg(CODE_BG));
    let label_style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(LABEL_BG).fg(LABEL_FG).add_modifier(Modifier::BOLD)
    };

    let mut out = Vec::with_capacity(token_lines.len() + 1);
    out.push(Line::from(Span::styled(format!(" {label} "), label_style)).right_aligned());
    for (idx, spans) in token_lines.into_iter().enumerate() {
        let mut line_spans = vec![bar()];
        if show_numbers {
            line_spans.push(Span::styled(
                format!("{:>3} │ ", idx + 1),
                Style::default().fg(Color::DarkGray).bg(CODE_BG),
            ));
        }
        line_spans.extend(spans.into_iter().map(|span| {
            let mut style = span.style;
            style.bg = Some(CODE_BG);
            Span::styled(span.content, style)
        }));
        out.push(Line::from(line_spans).style(Style::default().bg(CODE_BG)));
    }
    out
}

/// Highlights one fenced code block's contents (no ``` fences) with syntect
/// and wraps the result in the Phase 5 code-block chrome. `lang` is the raw
/// text after the opening ```` ``` ```` — empty when the fence has no tag, in
/// which case we try `detect_language` on the first content line before
/// giving up and rendering untokenized `[plain]`.
fn highlight_code_block(lang: &str, code: &str) -> Vec<Line<'static>> {
    let trimmed_lang = lang.trim();
    let (label, token_lines) = if trimmed_lang.is_empty() {
        let first_content_line = code.lines().next().unwrap_or("");
        match detect_language(first_content_line) {
            Some(syntax) => (format!("[{}] (auto)", syntax.name), tokenize_code_lines(syntax, code)),
            None => ("[plain]".to_owned(), plain_code_lines(code)),
        }
    } else {
        let syntax = syntax_for_token(trimmed_lang);
        (format!("[{trimmed_lang}]"), tokenize_code_lines(syntax, code))
    };
    decorate_code_block(token_lines, &label)
}

/// Parses a message body into ratatui `Line`s: fenced code blocks get full
/// syntect highlighting, everything else is scanned for inline `` `code` ``,
/// URLs and `@mentions` inline styling.
pub fn highlight_body(body: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        match rest.find("```") {
            None => {
                out.extend(highlight_plain_lines(rest));
                break;
            }
            Some(open) => {
                let before = &rest[..open];
                out.extend(highlight_plain_lines(before));
                let after_open = &rest[open + 3..];
                let Some(newline) = after_open.find('\n') else {
                    // Unterminated fence marker on the same line: treat rest as plain text.
                    out.extend(highlight_plain_lines(&rest[open..]));
                    break;
                };
                let lang = after_open[..newline].trim().to_owned();
                let after_lang = &after_open[newline + 1..];
                match after_lang.find("```") {
                    Some(close) => {
                        let code = &after_lang[..close];
                        out.extend(highlight_code_block(&lang, code));
                        rest = &after_lang[close + 3..];
                        // Skip a trailing newline right after the closing fence so the next
                        // plain-text segment doesn't start with a blank line.
                        rest = rest.strip_prefix('\n').unwrap_or(rest);
                    }
                    None => {
                        // Unterminated fence: highlight what remains as the code language anyway.
                        out.extend(highlight_code_block(&lang, after_lang));
                        break;
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

fn highlight_plain_lines(text: &str) -> Vec<Line<'static>> {
    text.split('\n').map(highlight_plain_line).collect()
}

/// Splits one plain-text line into spans, styling `` `inline code` ``, bare
/// URLs, and `@mentions`. Not a full markdown parser — just the three shapes
/// agmsg bodies actually contain (memory: agmsg-conventions body regularity).
fn highlight_plain_line(line: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    let flush_plain = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::raw(std::mem::take(buf)));
        }
    };

    while i < chars.len() {
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '`')
        {
            flush_plain(&mut buf, &mut spans);
            let code: String = chars[i + 1..i + 1 + end].iter().collect();
            spans.push(format_inline_code(&code));
            i += end + 2;
            continue;
        }
        // Simplification (deliberate scope reduction from the Phase 5 spec):
        // the URL is rendered inline as dim `<url>` right after the title
        // rather than surfacing on hover/status-line, since a true hover
        // preview needs app-wide mouse-position state plumbed through to
        // this pure `&str -> Vec<Line>` function, which doesn't have access
        // to it.
        if chars[i] == '[' {
            let suffix: String = chars[i..].iter().collect();
            if let Some((title, url)) = parse_markdown_link(&suffix) {
                flush_plain(&mut buf, &mut spans);
                // `[` + title + `]` + `(` + url + `)`.
                let consumed = title.chars().count() + url.chars().count() + 4;
                spans.push(Span::styled(
                    title,
                    Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!("<{url}>"),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                ));
                i += consumed;
                continue;
            }
        }
        if chars[i] == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
            let start = i;
            let mut end = i + 1;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || chars[end] == '-' || chars[end] == '_')
            {
                end += 1;
            }
            if end > start + 1 {
                flush_plain(&mut buf, &mut spans);
                let mention: String = chars[start..end].iter().collect();
                spans.push(Span::styled(
                    mention,
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
                i = end;
                continue;
            }
        }
        if (line[byte_index(&chars, i)..].starts_with("http://")
            || line[byte_index(&chars, i)..].starts_with("https://"))
            && (i == 0 || chars[i - 1].is_whitespace())
        {
            let start = i;
            let mut end = i;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            flush_plain(&mut buf, &mut spans);
            let url: String = chars[start..end].iter().collect();
            spans.push(Span::styled(
                url,
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            i = end;
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush_plain(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    Line::from(spans)
}

fn byte_index(chars: &[char], char_index: usize) -> usize {
    chars[..char_index].iter().map(|c| c.len_utf8()).sum()
}

/// Inline `` `code` `` span styling, shared by the scanner above and tests.
/// Solid background (not just dim fg) so a short inline snippet reads as
/// "code" at a glance the same way a fenced block does post-Phase-5.
fn format_inline_code(text: &str) -> Span<'static> {
    let style = if crate::palette::current().no_color {
        Style::default()
    } else {
        Style::default().bg(INLINE_CODE_BG).fg(Color::White)
    };
    Span::styled(text.to_owned(), style)
}

/// Manual `[title](url)` parser — text must *start* with `[`. Why not the
/// `regex` crate: this repo has zero regex dependency today (syntect's
/// `regex-fancy` feature is a separate `fancy-regex` crate, not `regex`, and
/// isn't exposed as a syntax-scanning primitive we can reuse here), and the
/// grammar is small enough — no nested brackets/parens to worry about in
/// agent-pasted message bodies — that hand-rolling it beats pulling in a new
/// dependency for one call site. The line scanner below is the sole
/// production caller; it re-slices from each `[` it sees and derives how
/// many chars to skip from the returned title/url lengths, so there's one
/// parsing definition instead of a scan-index-aware twin to keep in sync.
fn parse_markdown_link(text: &str) -> Option<(String, String)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.first() != Some(&'[') {
        return None;
    }
    let mut i = 1;
    let title_start = i;
    while i < chars.len() && chars[i] != ']' && chars[i] != '(' && chars[i] != '\n' {
        i += 1;
    }
    if chars.get(i) != Some(&']') || i == title_start {
        return None;
    }
    let title: String = chars[title_start..i].iter().collect();
    i += 1; // skip ']'
    if chars.get(i) != Some(&'(') {
        return None;
    }
    i += 1; // skip '('
    let url_start = i;
    while i < chars.len() && chars[i] != ')' && chars[i] != '\n' {
        i += 1;
    }
    if chars.get(i) != Some(&')') || i == url_start {
        return None;
    }
    let url: String = chars[url_start..i].iter().collect();
    Some((title, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_fence_highlights_with_multiple_styled_tokens() {
        let body = "before\n```rust\nfn main() {\n    let x = 1;\n}\n```\nafter";
        let lines = highlight_body(body);
        // "fn", "main", "let" should each land as their own styled span with
        // syntect's theme colors, not one flat raw span for the whole line.
        let fn_line = lines
            .iter()
            .find(|line| line.spans.iter().any(|s| s.content.contains("fn")))
            .expect("highlighted fn line present");
        assert!(
            fn_line.spans.len() > 1,
            "expected tokenized spans, got {:?}",
            fn_line.spans
        );
    }

    #[test]
    fn inline_code_url_and_mention_are_styled_separately() {
        let line = highlight_plain_line("see `foo.rs` at https://example.com cc @claude-main");
        let contents: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(contents.contains(&"foo.rs"));
        assert!(contents.contains(&"https://example.com"));
        assert!(contents.contains(&"@claude-main"));
    }

    #[test]
    fn plain_body_without_fences_is_unchanged_text() {
        let lines = highlight_body("just a normal message\nsecond line");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn long_code_block_gets_line_number_gutter() {
        // 6 content lines > LINE_NUMBER_THRESHOLD (5), so every line should
        // carry a "  1 │ " style right-aligned 3-digit prefix after the bar.
        let code = "one\ntwo\nthree\nfour\nfive\nsix";
        let body = format!("```rust\n{code}\n```");
        let lines = highlight_body(&body);
        let first_content_line = lines
            .iter()
            .find(|line| line.spans.iter().any(|s| s.content.contains("one")))
            .expect("first content line present");
        let numbered = first_content_line
            .spans
            .iter()
            .any(|s| s.content.as_ref() == "  1 │ ");
        assert!(
            numbered,
            "expected line-number gutter span, got {:?}",
            first_content_line.spans
        );

        let last_content_line = lines
            .iter()
            .find(|line| line.spans.iter().any(|s| s.content.contains("six")))
            .expect("last content line present");
        let numbered_last = last_content_line
            .spans
            .iter()
            .any(|s| s.content.as_ref() == "  6 │ ");
        assert!(
            numbered_last,
            "expected line 6 gutter span, got {:?}",
            last_content_line.spans
        );
    }

    #[test]
    fn short_code_block_has_no_line_numbers() {
        // 3 lines <= threshold: no gutter noise for short snippets.
        let body = "```rust\na\nb\nc\n```";
        let lines = highlight_body(body);
        let has_gutter = lines
            .iter()
            .any(|line| line.spans.iter().any(|s| s.content.contains('│')));
        assert!(!has_gutter, "did not expect a line-number gutter on a short block");
    }

    #[test]
    fn unlabeled_fence_detects_language_from_shebang() {
        let body = "```\n#!/usr/bin/env python3\nprint('hi')\n```";
        let lines = highlight_body(body);
        let label_line = lines
            .iter()
            .find(|line| line.alignment == Some(ratatui::layout::Alignment::Right))
            .expect("label line present");
        let label_text: String = label_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            label_text.contains("(auto)"),
            "expected auto-detected label, got {label_text:?}"
        );
    }

    #[test]
    fn unlabeled_fence_with_no_match_falls_back_to_plain() {
        let body = "```\nsome unrecognizable gibberish 12345\n```";
        let lines = highlight_body(body);
        let label_line = lines
            .iter()
            .find(|line| line.alignment == Some(ratatui::layout::Alignment::Right))
            .expect("label line present");
        let label_text: String = label_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(label_text.contains("[plain]"), "expected [plain] label, got {label_text:?}");
    }

    #[test]
    fn inline_code_span_has_expected_background_style() {
        let span = format_inline_code("foo.rs");
        assert_eq!(span.style.bg, Some(INLINE_CODE_BG));
        assert_eq!(span.style.fg, Some(Color::White));
    }

    #[test]
    fn markdown_link_parses_title_and_url() {
        let parsed = parse_markdown_link("[agmsg conventions](https://example.com/docs)");
        assert_eq!(
            parsed,
            Some(("agmsg conventions".to_owned(), "https://example.com/docs".to_owned()))
        );
    }

    #[test]
    fn markdown_link_renders_underlined_title_and_dim_url() {
        let line = highlight_plain_line("see [the doc](https://example.com/x) for details");
        let contents: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(contents.contains(&"the doc"));
        assert!(contents.contains(&"<https://example.com/x>"));
    }

    #[test]
    fn non_link_brackets_are_left_as_plain_text() {
        // No matching "(url)" after the "]" — must not be mistaken for a link.
        let line = highlight_plain_line("array indexing: arr[0] not a link");
        let contents: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(contents.iter().any(|c| c.contains("arr[0]")));
    }
}
