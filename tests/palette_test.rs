//! Phase 10 accessibility: NO_COLOR stripping, the color-blind-safe palette,
//! and the narrow-terminal 1-pane threshold.
//!
//! Why only one test here calls `palette::init`: it's a process-wide
//! `OnceLock` (first call wins), and `cargo test` runs the `#[test]` fns in
//! this file concurrently on multiple threads by default — a second test
//! calling `init` with a different config would silently lose the race
//! instead of failing loudly. Every other test below exercises a pure
//! function that takes its mode as an explicit argument instead of reading
//! the global, so there's nothing to race.
use agmsg_tui::app::Focus;
use agmsg_tui::color::agent_color;
use agmsg_tui::palette::{self, PaletteMode, SAFE_AGENT_PALETTE};
use agmsg_tui::ui::compute_layout;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

#[test]
fn no_color_strips_buffer_colors_but_keeps_modifiers() {
    use ratatui::style::Modifier;

    palette::init(true, PaletteMode::Default);

    let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
    buffer.content[0].set_fg(Color::Red).set_bg(Color::Blue);
    buffer.content[0].modifier = Modifier::BOLD;
    buffer.content[1].set_fg(Color::Cyan);

    palette::apply_to_buffer(&mut buffer);

    assert_eq!(buffer.content[0].fg, Color::Reset);
    assert_eq!(buffer.content[0].bg, Color::Reset);
    assert_eq!(buffer.content[0].modifier, Modifier::BOLD, "modifier must survive NO_COLOR");
    assert_eq!(buffer.content[1].fg, Color::Reset);
}

#[test]
fn safe_agent_palette_has_eight_distinct_colorblind_safe_entries() {
    let unique: std::collections::HashSet<_> = SAFE_AGENT_PALETTE.iter().collect();
    assert_eq!(unique.len(), 8, "Okabe-Ito palette entries must all be distinct");
    for color in SAFE_AGENT_PALETTE {
        assert_ne!(color, Color::Red);
        assert_ne!(color, Color::Green);
        assert_ne!(color, Color::LightRed);
        assert_ne!(color, Color::LightGreen);
    }
}

#[test]
fn safe_palette_agent_hash_is_stable_and_within_the_safe_set() {
    for name in ["claude-main", "codex-worker", "opencode-review", "cursor-1"] {
        let first = agent_color(name, PaletteMode::Safe);
        let second = agent_color(name, PaletteMode::Safe);
        assert_eq!(first, second, "hash must be deterministic");
        assert!(SAFE_AGENT_PALETTE.contains(&first));
    }
}

#[test]
fn narrow_width_threshold_switches_to_single_focused_pane() {
    let area = Rect::new(0, 0, 40, 24); // below NARROW_WIDTH_THRESHOLD (60)
    let layout = compute_layout(area, 30, Focus::Members);

    assert!(layout.members.width > 0 && layout.members.height > 0);
    assert_eq!(layout.teams.width, 0);
    assert_eq!(layout.room.width, 0);
}

#[test]
fn wide_terminal_keeps_all_three_panes_visible_regardless_of_focus() {
    let area = Rect::new(0, 0, 80, 24); // at/above NARROW_WIDTH_THRESHOLD (60)
    let layout = compute_layout(area, 30, Focus::Room);

    assert!(layout.teams.width > 0 && layout.teams.height > 0);
    assert!(layout.members.width > 0 && layout.members.height > 0);
    assert!(layout.room.width > 0 && layout.room.height > 0);
}

#[test]
fn overall_score_color_avoids_red_green_across_the_safe_triad() {
    for score in [95, 100, 80, 70, 60, 0] {
        let color = palette::overall_score_color(score, PaletteMode::Safe);
        assert_ne!(color, Color::Red);
        assert_ne!(color, Color::Green);
    }
    // High and low bands must still be visually distinct from each other.
    assert_ne!(
        palette::overall_score_color(95, PaletteMode::Safe),
        palette::overall_score_color(10, PaletteMode::Safe)
    );
}
