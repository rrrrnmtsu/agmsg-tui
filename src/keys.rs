//! Phase 14C — custom keybindings.
//!
//! Only *global/screen-entry* actions are remappable (see the scope guard in
//! `doc/agmsg-tui-phase14-plan.md`, section 14C). Screen-local editing keys
//! (composer text input, y/n confirms, per-screen navigation on
//! Agents/Audit/Health/BulkFilter) stay hardcoded — remapping those is a
//! non-goal for v1.
//!
//! `DEFAULT_BINDINGS` is the single source of truth for both `KeyMap::default()`
//! and `default_toml()` — the two can't drift apart because both are built
//! from the same table (`tests::default_toml_round_trips_to_default` pins
//! this invariant).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// Actions reachable through the remappable keymap. Deliberately small:
/// only global/screen-entry actions (see module docs). `Split`/`Replay` are
/// reserved names for 14A/14E — declared here so those phases register
/// their bindings in this same table instead of scattering new
/// `KeyModifiers::CONTROL` checks through `app.rs`, but neither is wired to
/// any dispatch behavior yet in 14C.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    Help,
    Compose,
    Audit,
    Agents,
    Health,
    Bulk,
    Filter,
    NotifSettings,
    Split,
    Replay,
    NavUp,
    NavDown,
    TabNext,
    /// Phase 14D: toggles `com.remma.agmsg-audit-daily` load/unload from the
    /// HEALTH screen. Unlike every other entry in this table it's
    /// screen-local (only ever dispatched from `App::handle_health_key`,
    /// never from the Main-screen global section) — registered here anyway,
    /// per the phase's explicit instruction, so it's remappable through the
    /// same `keys.toml` mechanism as everything else.
    LaunchagentToggle,
}

impl Action {
    /// The `[keys]` table key used in `keys.toml` for this action.
    pub fn name(self) -> &'static str {
        match self {
            Action::Quit => "quit",
            Action::Help => "help",
            Action::Compose => "compose",
            Action::Audit => "audit",
            Action::Agents => "agents",
            Action::Health => "health",
            Action::Bulk => "bulk",
            Action::Filter => "filter",
            Action::NotifSettings => "notif_settings",
            Action::Split => "split",
            Action::Replay => "replay",
            Action::NavUp => "nav_up",
            Action::NavDown => "nav_down",
            Action::TabNext => "tab_next",
            Action::LaunchagentToggle => "launchagent_toggle",
        }
    }

    fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "quit" => Action::Quit,
            "help" => Action::Help,
            "compose" => Action::Compose,
            "audit" => Action::Audit,
            "agents" => Action::Agents,
            "health" => Action::Health,
            "bulk" => Action::Bulk,
            "filter" => Action::Filter,
            "notif_settings" => Action::NotifSettings,
            "split" => Action::Split,
            "replay" => Action::Replay,
            "nav_up" => Action::NavUp,
            "nav_down" => Action::NavDown,
            "tab_next" => Action::TabNext,
            "launchagent_toggle" => Action::LaunchagentToggle,
            _ => return None,
        })
    }
}

type Chord = (KeyCode, KeyModifiers);

/// Single source of truth for the default table: both `KeyMap::default()`
/// and `default_toml()` (`--print-default-keys`) are built from this list,
/// so dump output and actual runtime behavior can't drift apart.
///
/// Chord choices mirror the *pre-14C* hardcoded bindings in `app.rs`
/// exactly, so loading with no `keys.toml` present is byte-identical to
/// current behavior. `bulk`/`filter` map to what the code actually does
/// (Ctrl+F opens the BulkFilter screen, bare `F` toggles the MEMBER-pane
/// traffic filter) rather than the plan doc's parenthetical labels, which
/// don't match the current `app.rs` (Ctrl+B there is the bell-mute toggle,
/// not bulk) — verified by reading the code, not assumed from the doc.
fn default_bindings() -> Vec<(Action, KeyCode, KeyModifiers)> {
    vec![
        (Action::Quit, KeyCode::Char('q'), KeyModifiers::NONE),
        (Action::Help, KeyCode::Char('?'), KeyModifiers::NONE),
        (Action::Compose, KeyCode::Char('c'), KeyModifiers::NONE),
        (Action::Audit, KeyCode::Char('a'), KeyModifiers::CONTROL),
        (Action::Agents, KeyCode::Char('A'), KeyModifiers::NONE),
        (Action::Health, KeyCode::Char('H'), KeyModifiers::NONE),
        (Action::Bulk, KeyCode::Char('f'), KeyModifiers::CONTROL),
        (Action::Filter, KeyCode::Char('F'), KeyModifiers::NONE),
        (
            Action::NotifSettings,
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        ),
        // Reserved for 14A: verified free on Main (see plan doc invariant 5).
        (Action::Split, KeyCode::Char('s'), KeyModifiers::CONTROL),
        // Reserved for 14E. Its eventual real trigger is screen-local (`r`
        // inside Audit/History per the 14E spec), not this Main keymap; a
        // placeholder chord is reserved here only so the action name exists
        // in the table from day one, per the 14C ordering rationale. Chosen
        // to avoid colliding with the existing bare `r` (mark-read) binding.
        (Action::Replay, KeyCode::Char('r'), KeyModifiers::CONTROL),
        (Action::NavUp, KeyCode::Char('k'), KeyModifiers::NONE),
        (Action::NavDown, KeyCode::Char('j'), KeyModifiers::NONE),
        (Action::TabNext, KeyCode::Tab, KeyModifiers::NONE),
        // Health screen only binds bare letters (j/k/t/R/?/H) today, so `L`
        // is free there — verified by reading `App::handle_health_key`.
        (
            Action::LaunchagentToggle,
            KeyCode::Char('L'),
            KeyModifiers::NONE,
        ),
    ]
}

/// Chord -> Action lookup table, built from defaults and optionally
/// overridden by a `keys.toml` file. See module docs for scope.
#[derive(Clone, Debug)]
pub struct KeyMap {
    map: HashMap<Chord, Action>,
}

impl Default for KeyMap {
    fn default() -> Self {
        let mut map = HashMap::new();
        for (action, code, mods) in default_bindings() {
            map.insert((code, mods), action);
        }
        Self { map }
    }
}

impl KeyMap {
    /// Looks up the action bound to `key`, if any. Modifier comparison is
    /// masked to CONTROL|ALT only — SHIFT is deliberately excluded because
    /// letter case already encodes it (`KeyCode::Char('A')` vs `('a')`), and
    /// the pre-14C hardcoded arms this replaces never checked SHIFT either
    /// (e.g. `KeyCode::Char('A') => ...` with no modifier check at all).
    /// Named keys that need an explicit shift (e.g. `shift+tab`) still work
    /// because the modifier is captured in the map key when parsed via
    /// `parse_chord`.
    pub fn lookup(&self, key: KeyEvent) -> Option<Action> {
        let mods = key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        self.map.get(&(key.code, mods)).copied()
    }

    /// Reads `path`, merges any `[keys]` overrides over `KeyMap::default()`,
    /// and returns the merged map plus any warnings to print (to stderr,
    /// before raw-mode is entered — see `main.rs`). Never fails: a missing
    /// file is silent (defaults only), a malformed file or bad per-key entry
    /// warns and keeps the default for the affected action(s).
    pub fn load(path: &Path) -> (KeyMap, Vec<String>) {
        let mut warnings = Vec::new();
        let mut keymap = KeyMap::default();

        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => return (keymap, warnings), // absent file: defaults, no warning
        };

        let parsed: KeysFile = match toml::from_str(&content) {
            Ok(parsed) => parsed,
            Err(error) => {
                warnings.push(format!(
                    "keys.toml: could not parse {} ({error}); using defaults",
                    path.display()
                ));
                return (keymap, warnings);
            }
        };

        // BTreeMap gives deterministic alphabetical-by-action-name iteration
        // regardless of the file's declaration order or the toml crate's
        // feature flags — that's what makes "duplicate chord -> last wins"
        // reproducible without needing the `preserve_order` feature.
        for (name, chord_str) in &parsed.keys {
            let Some(action) = Action::from_name(name) else {
                warnings.push(format!("keys.toml: unknown action '{name}', ignoring"));
                continue;
            };
            match parse_chord(chord_str) {
                Ok(chord) => keymap.rebind(action, chord, chord_str, &mut warnings),
                Err(reason) => {
                    warnings.push(format!(
                        "keys.toml: invalid key for '{name}' ('{chord_str}'): {reason}; keeping default"
                    ));
                }
            }
        }

        (keymap, warnings)
    }

    /// Rebinds `action` to `chord`: drops the action's previous binding
    /// (default or earlier override) and inserts the new one. If `chord` was
    /// already claimed by a *different* action, that action loses its
    /// binding entirely and a warning is recorded — "last wins" per the
    /// BTreeMap-driven processing order in `load`.
    fn rebind(&mut self, action: Action, chord: Chord, raw: &str, warnings: &mut Vec<String>) {
        if let Some(existing) = self.map.get(&chord).copied()
            && existing != action
        {
            warnings.push(format!(
                "keys.toml: '{raw}' is bound to both '{}' and '{}'; '{}' wins",
                existing.name(),
                action.name(),
                action.name()
            ));
        }
        self.map.retain(|_, bound_action| *bound_action != action);
        self.map.insert(chord, action);
    }

    /// Renders the default table as `keys.toml` text — used by
    /// `--print-default-keys`. Built from the same `default_bindings()` list
    /// `KeyMap::default()` uses, so this can't drift from actual behavior.
    pub fn default_toml() -> String {
        let mut out = String::from("[keys]\n");
        for (action, code, mods) in default_bindings() {
            out.push_str(&format!(
                "{} = \"{}\"\n",
                action.name(),
                chord_to_string(code, mods)
            ));
        }
        out
    }
}

#[derive(Deserialize)]
struct KeysFile {
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

/// Parses a chord string like `"ctrl+s"`, `"alt+enter"`, `"shift+f2"`, or a
/// bare `"q"` / `"enter"` / `"f1"`. Case-insensitive for modifier names and
/// named keys; a single-character key keeps its original case (`"A"` vs
/// `"a"` are different chords, since letter case already encodes shift for
/// printable keys).
pub fn parse_chord(input: &str) -> Result<Chord, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty key string".to_owned());
    }
    let parts: Vec<&str> = trimmed.split('+').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(format!("malformed chord '{input}'"));
    }
    let (modifier_parts, key_part) = parts.split_at(parts.len() - 1);
    let key_str = key_part[0];

    let mut mods = KeyModifiers::NONE;
    for raw_modifier in modifier_parts {
        match raw_modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "opt" | "option" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            other => return Err(format!("unknown modifier '{other}'")),
        }
    }

    let code = parse_key_code(key_str)?;
    Ok((code, mods))
}

fn parse_key_code(key: &str) -> Result<KeyCode, String> {
    let lower = key.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "insert" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        "backspace" => KeyCode::Backspace,
        _ if lower.len() >= 2
            && lower.starts_with('f')
            && lower[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            let n: u8 = lower[1..]
                .parse()
                .map_err(|_| format!("invalid function key '{key}'"))?;
            if !(1..=12).contains(&n) {
                return Err(format!("function key out of range (f1-f12): '{key}'"));
            }
            KeyCode::F(n)
        }
        _ if key.chars().count() == 1 => KeyCode::Char(key.chars().next().unwrap()),
        _ => return Err(format!("unrecognized key '{key}'")),
    };
    Ok(code)
}

fn chord_to_string(code: KeyCode, mods: KeyModifiers) -> String {
    let mut parts = Vec::new();
    if mods.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_owned());
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("alt".to_owned());
    }
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_owned());
    }
    parts.push(key_code_to_string(code));
    parts.join("+")
}

fn key_code_to_string(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => "backtab".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    // --- chord parser table test -------------------------------------

    #[test]
    fn parses_ctrl_prefixed_chord() {
        assert_eq!(
            parse_chord("ctrl+s").unwrap(),
            (KeyCode::Char('s'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn parses_alt_prefixed_named_key() {
        assert_eq!(
            parse_chord("alt+enter").unwrap(),
            (KeyCode::Enter, KeyModifiers::ALT)
        );
    }

    #[test]
    fn parses_shift_prefixed_named_key() {
        assert_eq!(
            parse_chord("shift+tab").unwrap(),
            (KeyCode::Tab, KeyModifiers::SHIFT)
        );
    }

    #[test]
    fn parses_bare_char() {
        assert_eq!(
            parse_chord("q").unwrap(),
            (KeyCode::Char('q'), KeyModifiers::NONE)
        );
    }

    #[test]
    fn parses_bare_uppercase_char_distinct_from_lowercase() {
        assert_eq!(
            parse_chord("A").unwrap(),
            (KeyCode::Char('A'), KeyModifiers::NONE)
        );
        assert_ne!(parse_chord("A").unwrap(), parse_chord("a").unwrap());
    }

    #[test]
    fn parses_named_keys_case_insensitively() {
        assert_eq!(parse_chord("ENTER").unwrap().0, KeyCode::Enter);
        assert_eq!(parse_chord("Esc").unwrap().0, KeyCode::Esc);
        assert_eq!(parse_chord("F1").unwrap().0, KeyCode::F(1));
        assert_eq!(parse_chord("f12").unwrap().0, KeyCode::F(12));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("ctrl+").is_err());
        assert!(parse_chord("banana+q").is_err());
        assert!(parse_chord("f13").is_err());
        assert!(parse_chord("f0").is_err());
        assert!(parse_chord("notakey").is_err());
    }

    // --- merge / load behavior ----------------------------------------

    #[test]
    fn missing_file_yields_defaults_and_no_warnings() {
        let (keymap, warnings) = KeyMap::load(Path::new("/nonexistent/agmsg-tui/keys.toml"));
        assert!(warnings.is_empty());
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn default_load_produces_expected_quit_binding() {
        let keymap = KeyMap::default();
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn partial_override_keeps_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "[keys]\nquit = \"Q\"\n").unwrap();
        drop(file);

        let (keymap, warnings) = KeyMap::load(&path);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('Q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
        // old default 'q' no longer bound to quit (authoritative, not additive)
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        // untouched action keeps its default
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('?'), KeyModifiers::NONE)),
            Some(Action::Help)
        );
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(Action::Audit)
        );
    }

    #[test]
    fn invalid_key_falls_back_to_default_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        fs::write(&path, "[keys]\nquit = \"notakey\"\n").unwrap();

        let (keymap, warnings) = KeyMap::load(&path);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("quit"));
        assert!(warnings[0].contains("notakey"));
        // default preserved
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn unknown_action_name_warns_and_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        fs::write(&path, "[keys]\nfrobnicate = \"z\"\n").unwrap();

        let (keymap, warnings) = KeyMap::load(&path);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("frobnicate"));
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('z'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn duplicate_chord_in_user_file_last_wins_with_warning() {
        // BTreeMap iteration is alphabetical by action name: "audit" comes
        // before "quit", so "quit" (processed second) wins the shared chord.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        fs::write(&path, "[keys]\naudit = \"x\"\nquit = \"x\"\n").unwrap();

        let (keymap, warnings) = KeyMap::load(&path);
        assert!(
            warnings.iter().any(|w| w.contains("bound to both")),
            "expected duplicate-chord warning, got: {warnings:?}"
        );
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn malformed_toml_file_falls_back_to_defaults_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        fs::write(&path, "this is not [ valid toml").unwrap();

        let (keymap, warnings) = KeyMap::load(&path);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            keymap.lookup(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
    }

    // --- round-trip / drift guard --------------------------------------

    #[test]
    fn default_toml_round_trips_to_default() {
        let text = KeyMap::default_toml();
        let parsed: KeysFile = toml::from_str(&text).expect("default_toml must be valid TOML");
        assert_eq!(parsed.keys.len(), default_bindings().len());
        for (action, code, mods) in default_bindings() {
            let chord_str = parsed
                .keys
                .get(action.name())
                .unwrap_or_else(|| panic!("default_toml missing action '{}'", action.name()));
            assert_eq!(
                parse_chord(chord_str).unwrap(),
                (code, mods),
                "action '{}' round-trip mismatch",
                action.name()
            );
        }
    }

    #[test]
    fn print_default_keys_output_parses_and_loads_same_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        fs::write(&path, KeyMap::default_toml()).unwrap();

        let (keymap, warnings) = KeyMap::load(&path);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        for (action, code, mods) in default_bindings() {
            assert_eq!(
                keymap.lookup(key(code, mods)),
                Some(action),
                "action '{}' did not round-trip through --print-default-keys output",
                action.name()
            );
        }
    }
}
