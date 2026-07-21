//! Deterministic `from_agent` name → ratatui `Color` mapping for the room pane.
//!
//! Why sum-of-bytes % 8 and not a real hash (SipHash/FNV/etc): stability
//! matters more than distribution at this cardinality — a team roster is
//! 2-8 agents, and the same handful of names repeat in every message, so a
//! cryptographic hash would just add a dependency without a visible benefit.
use ratatui::style::Color;

use crate::palette::{PaletteMode, SAFE_AGENT_PALETTE};

/// Bright, readable-on-dark-bg colors only; the dim/gray tones are reserved
/// for timestamps and `to_agent` so agent color never collides with chrome.
const PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Yellow,
    Color::Green,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
    Color::LightBlue,
    Color::LightGreen,
];

/// `mode: Safe` swaps in [`SAFE_AGENT_PALETTE`] (Okabe–Ito) instead of the
/// default 8 — a straight color-value remap of the default palette can't do
/// this because `PALETTE` mixes Red/Green-adjacent hues (`LightRed`, `Green`,
/// `LightGreen`) that are exactly what a deuteranopia/protanopia palette
/// needs to not pick from at all, not just recolor individually.
pub fn agent_color(name: &str, mode: PaletteMode) -> Color {
    let sum: u32 = name.bytes().map(u32::from).sum();
    let palette: &[Color] = match mode {
        PaletteMode::Default => &PALETTE,
        PaletteMode::Safe => &SAFE_AGENT_PALETTE,
    };
    palette[(sum % palette.len() as u32) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_name_always_maps_to_the_same_color() {
        assert_eq!(
            agent_color("claude-main", PaletteMode::Default),
            agent_color("claude-main", PaletteMode::Default)
        );
        assert_eq!(
            agent_color("codex-worker", PaletteMode::Default),
            agent_color("codex-worker", PaletteMode::Default)
        );
    }

    #[test]
    fn formula_matches_sum_of_bytes_mod_palette_len() {
        // "ab" = 97 + 98 = 195, 195 % 8 = 3 -> Magenta.
        assert_eq!(agent_color("ab", PaletteMode::Default), Color::Magenta);
        // "a" = 97, 97 % 8 = 1 -> Yellow.
        assert_eq!(agent_color("a", PaletteMode::Default), Color::Yellow);
    }

    #[test]
    fn safe_mode_never_returns_a_default_palette_color() {
        for name in ["claude-main", "codex-worker", "opencode-review", "a", "ab", "zzz"] {
            let color = agent_color(name, PaletteMode::Safe);
            assert!(
                SAFE_AGENT_PALETTE.contains(&color),
                "{name} mapped to {color:?}, not in the safe palette"
            );
        }
    }
}
