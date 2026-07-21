//! Display-column width, for the handful of layout decisions that measure
//! text ourselves instead of letting a ratatui widget wrap it.
//!
//! Why not `str::chars().count()`: that counts Unicode scalar values, but a
//! terminal cell is a display column — CJK ideographs and most emoji render
//! as 2 columns, not 1. Anything that gates layout on char count (the fold
//! threshold, most notably) silently under-estimates real width for any
//! non-ASCII body, so a Japanese message folds far later than an
//! ASCII-equivalent one of the same rendered size.
use unicode_width::UnicodeWidthStr;

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_display_width_matches_char_count() {
        assert_eq!(display_width("claude-main"), "claude-main".chars().count());
    }

    #[test]
    fn japanese_mixed_header_width_is_calculated_correctly() {
        // "田中" is 2 CJK characters, each 2 display columns wide, so the
        // header occupies 2 more columns than its char count would suggest —
        // exactly the gap `chars().count()`-based fold/header math used to miss.
        let header = "[12:00] 田中-claude → codex-worker";
        let cjk_chars = 2;
        assert_eq!(display_width(header), header.chars().count() + cjk_chars);
    }

    #[test]
    fn pure_ascii_body_has_no_width_gap() {
        let body = "status update: deploy finished, all green";
        assert_eq!(display_width(body), body.chars().count());
    }
}
