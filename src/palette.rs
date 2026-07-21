//! Centralized color post-processing: `NO_COLOR` stripping and a
//! color-blind-safe palette, applied once to the rendered [`Buffer`] instead
//! of at every `Style::default().fg(...)` call site.
//!
//! Why not thread a palette argument through all ~90 `Color::` call sites in
//! `ui/*.rs`: this codebase's Cyan is de-facto "focus border/selection" and
//! its Red/Green are de-facto "danger/ok" everywhere they're used — there is
//! no call site where a blanket semantic remap would be wrong — so a single
//! buffer-level pass gets the same correctness as auditing every site, stays
//! correct as new UI is added, and is one function to test instead of ninety
//! call sites to keep in sync. Two call sites where the semantics genuinely
//! need a mode-aware choice instead of a fixed remap (the agent-name hash
//! palette in `color.rs`, and the audit score thresholds in `ui/audit.rs`)
//! are handled directly with a `PaletteMode` argument rather than relying on
//! this pass.
use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::style::Color;

use crate::highlight::CODE_BG;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    Default,
    Safe,
}

#[derive(Clone, Copy, Debug)]
pub struct PaletteConfig {
    pub no_color: bool,
    pub mode: PaletteMode,
}

impl Default for PaletteConfig {
    fn default() -> Self {
        Self { no_color: false, mode: PaletteMode::Default }
    }
}

static CONFIG: OnceLock<PaletteConfig> = OnceLock::new();

/// Sets the process-wide palette config. Called once at startup from
/// `main.rs` after resolving CLI flags / env vars; a no-op on any later call
/// (tests that never call this get the all-defaults config below, which
/// matches every existing snapshot test's expectations).
pub fn init(no_color: bool, mode: PaletteMode) {
    let _ = CONFIG.set(PaletteConfig { no_color, mode });
}

pub fn current() -> PaletteConfig {
    CONFIG.get().copied().unwrap_or_default()
}

/// Okabe–Ito categorical palette — the standard colorblind-safe (deuteranopia
/// / protanopia) 8-color set, used verbatim rather than hand-picked so the
/// safety claim rests on a citable source instead of this codebase's own
/// judgment of "looks distinguishable."
pub const SAFE_ORANGE: Color = Color::Rgb(230, 159, 0);
pub const SAFE_SKY_BLUE: Color = Color::Rgb(86, 180, 233);
pub const SAFE_BLUISH_GREEN: Color = Color::Rgb(0, 158, 115);
pub const SAFE_YELLOW: Color = Color::Rgb(240, 228, 66);
pub const SAFE_BLUE: Color = Color::Rgb(0, 114, 178);
pub const SAFE_VERMILLION: Color = Color::Rgb(213, 94, 0);
pub const SAFE_REDDISH_PURPLE: Color = Color::Rgb(204, 121, 167);

pub const SAFE_AGENT_PALETTE: [Color; 8] = [
    SAFE_ORANGE,
    SAFE_SKY_BLUE,
    SAFE_BLUISH_GREEN,
    SAFE_YELLOW,
    SAFE_BLUE,
    SAFE_VERMILLION,
    SAFE_REDDISH_PURPLE,
    Color::Gray,
];

/// Audit overall-score color (90+/70-89/<70), mode-aware because the spec
/// calls out this specific triad ("青/黄/濃橙") rather than leaving it to the
/// generic buffer remap.
pub fn overall_score_color(score: u16, mode: PaletteMode) -> Color {
    match mode {
        PaletteMode::Default => match score {
            90.. => Color::Green,
            70..=89 => Color::Yellow,
            _ => Color::Red,
        },
        PaletteMode::Safe => match score {
            90.. => SAFE_BLUE,
            70..=89 => SAFE_YELLOW,
            _ => SAFE_VERMILLION,
        },
    }
}

/// Same triad, scaled to the per-axis 0-10 score used in the AXES panel.
pub fn axis_score_color(score: u16, mode: PaletteMode) -> Color {
    match mode {
        PaletteMode::Default => match score {
            9.. => Color::Green,
            7..=8 => Color::Yellow,
            _ => Color::Red,
        },
        PaletteMode::Safe => match score {
            9.. => SAFE_BLUE,
            7..=8 => SAFE_YELLOW,
            _ => SAFE_VERMILLION,
        },
    }
}

/// Applies the active [`PaletteConfig`] to an already-rendered frame buffer.
/// Call once per frame, after every widget has drawn — see `ui::render`.
pub fn apply_to_buffer(buffer: &mut Buffer) {
    let config = current();
    if config.no_color {
        strip_buffer(buffer);
    } else if config.mode == PaletteMode::Safe {
        remap_buffer_safe(buffer);
    }
}

/// Removes all fg/bg color, keeping every cell's `Modifier` bits (BOLD /
/// ITALIC / UNDERLINED / DIM / REVERSED, etc — `Cell::modifier` is a
/// separate field this loop never touches). The one exception is the fenced
/// code block's dark background + left-edge bar (`highlight.rs` sets both to
/// `CODE_BG`/`bar` on every cell of a code line): those stay untouched
/// because they're structural ("this is a code block"), not per-token
/// decoration — `highlight.rs` has already stripped syntax-highlight token
/// colors when `NO_COLOR` is set, so every other cell in a code block already
/// carries no fg color by the time this pass runs.
fn strip_buffer(buffer: &mut Buffer) {
    for cell in buffer.content.iter_mut() {
        if cell.bg == CODE_BG {
            continue;
        }
        cell.set_fg(Color::Reset);
        cell.set_bg(Color::Reset);
    }
}

/// Remaps the handful of named colors that form red/green pairs (or, for
/// Cyan, the focus-border convention) elsewhere in the UI. `Color::Rgb(..)`
/// values — including every color this module itself hands out via
/// [`SAFE_AGENT_PALETTE`], [`overall_score_color`], and [`axis_score_color`]
/// — never match these arms, so already-safe colors pass through unchanged.
fn remap_buffer_safe(buffer: &mut Buffer) {
    for cell in buffer.content.iter_mut() {
        if let Some(mapped) = remap_named(cell.fg) {
            cell.set_fg(mapped);
        }
        if let Some(mapped) = remap_named(cell.bg) {
            cell.set_bg(mapped);
        }
    }
}

fn remap_named(color: Color) -> Option<Color> {
    match color {
        Color::Red => Some(SAFE_VERMILLION),
        Color::LightRed => Some(SAFE_ORANGE),
        Color::Green => Some(SAFE_BLUE),
        Color::LightGreen => Some(SAFE_SKY_BLUE),
        // Focus border cyan -> yellow (spec §S10-2).
        Color::Cyan => Some(SAFE_YELLOW),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn strip_buffer_clears_color_but_keeps_modifier() {
        use ratatui::style::Modifier;

        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let cell = &mut buffer.content[0];
        cell.set_fg(Color::Red);
        cell.set_bg(Color::Blue);
        cell.modifier = Modifier::BOLD;

        strip_buffer(&mut buffer);

        let cell = &buffer.content[0];
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
        assert_eq!(cell.modifier, Modifier::BOLD);
    }

    #[test]
    fn strip_buffer_preserves_code_block_background_exception() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer.content[0].set_fg(Color::Cyan).set_bg(CODE_BG);

        strip_buffer(&mut buffer);

        assert_eq!(buffer.content[0].bg, CODE_BG);
        assert_eq!(buffer.content[0].fg, Color::Cyan);
    }

    #[test]
    fn safe_remap_replaces_red_green_and_cyan_with_okabe_ito_colors() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer.content[0].set_fg(Color::Red).set_bg(Color::Green);
        remap_buffer_safe(&mut buffer);
        assert_eq!(buffer.content[0].fg, SAFE_VERMILLION);
        assert_eq!(buffer.content[0].bg, SAFE_BLUE);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer.content[0].set_fg(Color::Cyan);
        remap_buffer_safe(&mut buffer);
        assert_eq!(buffer.content[0].fg, SAFE_YELLOW);
    }

    #[test]
    fn safe_remap_leaves_rgb_colors_untouched() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer.content[0].set_fg(SAFE_BLUISH_GREEN);
        remap_buffer_safe(&mut buffer);
        assert_eq!(buffer.content[0].fg, SAFE_BLUISH_GREEN);
    }

    #[test]
    fn overall_score_color_avoids_red_green_pair_in_safe_mode() {
        let high = overall_score_color(95, PaletteMode::Safe);
        let low = overall_score_color(10, PaletteMode::Safe);
        assert_ne!(high, Color::Red);
        assert_ne!(high, Color::Green);
        assert_ne!(low, Color::Red);
        assert_ne!(low, Color::Green);
        assert_ne!(high, low);
    }
}
