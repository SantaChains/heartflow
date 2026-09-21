//! User-customizable key bindings for the full-screen shell.
//!
//! The shell's reducer speaks in semantic [`Action`]s, never raw keys: a key
//! press is resolved through the [`Keymap`] into an `Action`, or into `None`
//! meaning "no shell binding claims this key, forward it to the input editor".
//! Decoupling the physical key from the verb is what lets bindings live on disk
//! (`keymap.toml`) and be remapped without touching dispatch logic, and it gives
//! every future shell verb (queue, guide, section switch) one uniform place to
//! hang a key.
//!
//! The on-disk format mirrors `theme.toml`: a `[keymap]` table mapping each
//! action name to a key string (`"ctrl+c"`) or a list of them, layered user
//! (`~/.heartflow/keymap.toml`) then project (`.heartflow/keymap.toml`, which
//! wins per action). Loading is fault-isolated exactly like the other config
//! surfaces — a missing/unreadable/unparseable file contributes nothing, an
//! unknown action or malformed key string is skipped with a warning — so a
//! broken keymap can never abort startup or strand the shell without its default
//! bindings. Unlike the palette (a process-wide `OnceLock`), the keymap is an
//! ordinary owned value held by the shell, so it can be hot-reloaded at a turn
//! boundary without a global singleton.

use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tracing::warn;

/// A semantic verb the shell can perform. The reducer matches on these rather
/// than on keys, so remapping a key never edits dispatch. Every variant is both
/// produced by [`Keymap::default`] and consumed by the shell's key router, so
/// the set grows only alongside a wired feature (no speculative dead actions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Take the input editor's text as a submission (queued if a turn is live).
    Submit,
    /// The interrupt key: cancel a running turn (double-tap) or quit when idle.
    Interrupt,
    /// The escape key: dismiss an overlay or quit when idle; inert mid-turn.
    Escape,
    /// Fold or unfold the most recent process entry (reasoning or tool result).
    ToggleFold,
    /// Scroll the transcript up, away from the newest line.
    ScrollUp,
    /// Scroll the transcript down, toward the newest line.
    ScrollDown,
    /// Open the guide overlay: preview the zero-token next-task draft (prior
    /// work + current state + the task) and send it, queued behind a running
    /// turn so it never interrupts.
    Guide,
    /// Switch to the next conversation section (tab) in the shell. Idle-only: a
    /// turn runs inline and holds its section's runtime, so the reducer guards
    /// these mid-turn instead of switching under a live turn.
    NextSection,
    /// Switch to the previous conversation section (tab) in the shell.
    PrevSection,
    /// Open a new, independent conversation section with its own runtime,
    /// session file and follow-up queue.
    NewSection,
    /// Toggle the in-shell key-reference overlay: every action with its *live*
    /// bound key and a one-line description, so the full keymap (including
    /// bindings the input bar has no room to list) is discoverable without
    /// leaving the shell. Read-only, so it is safe to raise mid-turn.
    Help,
}

/// Every action in canonical order, used for name lookup and (later) stable
/// export. Kept as a slice so adding an action is a one-line, review-friendly
/// change that automatically flows into `from_name`.
const ALL_ACTIONS: &[Action] = &[
    Action::Submit,
    Action::Interrupt,
    Action::Escape,
    Action::ToggleFold,
    Action::ScrollUp,
    Action::ScrollDown,
    Action::Guide,
    Action::NextSection,
    Action::PrevSection,
    Action::NewSection,
    Action::Help,
];

impl Action {
    /// The stable on-disk name for this action.
    #[must_use]
    pub const fn as_name(self) -> &'static str {
        match self {
            Action::Submit => "submit",
            Action::Interrupt => "interrupt",
            Action::Escape => "escape",
            Action::ToggleFold => "toggle_fold",
            Action::ScrollUp => "scroll_up",
            Action::ScrollDown => "scroll_down",
            Action::Guide => "guide",
            Action::NextSection => "next_section",
            Action::PrevSection => "prev_section",
            Action::NewSection => "new_section",
            Action::Help => "help",
        }
    }

    /// A one-line, key-agnostic description of what this action does, shown
    /// beside the live binding in the in-shell key-reference overlay. Worded as
    /// an affordance (never naming a key) so it stays correct after a remap.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Action::Submit => "send the input (queued while a turn runs)",
            Action::Interrupt => "cancel a running turn (press twice) / quit when idle",
            Action::Escape => "dismiss an overlay / quit when idle",
            Action::ToggleFold => "fold or unfold the latest process entry",
            Action::ScrollUp => "scroll the transcript up",
            Action::ScrollDown => "scroll the transcript down",
            Action::Guide => "preview and send a zero-token next-task draft",
            Action::NextSection => "switch to the next conversation section",
            Action::PrevSection => "switch to the previous conversation section",
            Action::NewSection => "open a new conversation section",
            Action::Help => "show this key reference",
        }
    }

    /// Resolve an on-disk name back to an action; `None` for unknown names so a
    /// typo (or a newer schema's action) is skipped rather than fatal.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        ALL_ACTIONS
            .iter()
            .copied()
            .find(|action| action.as_name() == name)
    }
}

/// A physical key: a code plus the modifiers that must be held. Characters are
/// compared case-insensitively and `SHIFT` is masked out of the match (see
/// [`mod_mask`]), so a binding and a live event agree regardless of caps lock or
/// terminal shift-reporting quirks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    /// The key code, normalized (characters lowercased) at construction.
    pub code: KeyCode,
    /// The required modifiers, pre-masked to [`mod_mask`] at construction.
    pub modifiers: KeyModifiers,
}

impl Binding {
    /// Build a binding, normalizing modifiers and character case so a stored
    /// binding and a live key event compare equal.
    #[must_use]
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            code: normalize_code(code),
            modifiers: modifiers.intersection(mod_mask()),
        }
    }

    /// Whether a live key event triggers this binding.
    #[must_use]
    pub fn matches(&self, key: &KeyEvent) -> bool {
        self.modifiers == key.modifiers.intersection(mod_mask())
            && self.code == normalize_code(key.code)
    }
}

/// Modifiers that participate in a match. `SHIFT` is excluded on purpose:
/// whether an uppercase letter arrives as `Char('C')` or `Char('c') + SHIFT`
/// varies by terminal, so ignoring shift and lowercasing characters keeps
/// `"ctrl+c"` matching regardless of caps. Keys a terminal reports as distinct
/// codes under shift (e.g. `BackTab`) are bound by their own name instead.
fn mod_mask() -> KeyModifiers {
    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER
}

/// Lowercase character codes so `Char('c')` and `Char('C')` are one binding;
/// every other code is returned unchanged.
fn normalize_code(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    }
}

/// The resolved set of key bindings: defaults plus layered `keymap.toml`
/// overrides. Held by the shell (not a global) so it can be hot-reloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    /// Bindings in priority order; a later entry shadows an earlier one on a
    /// shared key, so overrides beat defaults. A `Vec` (not a map) because
    /// `KeyCode` is not `Ord` and the set is tiny, making a linear resolve on
    /// each keypress both simpler and effectively free.
    bindings: Vec<(Binding, Action)>,
}

impl Default for Keymap {
    /// The built-in bindings the shell ships with. These preserve the shell's
    /// original hardcoded behavior, so a keymap-free install is unchanged.
    fn default() -> Self {
        Self {
            bindings: vec![
                (
                    Binding::new(KeyCode::Enter, KeyModifiers::NONE),
                    Action::Submit,
                ),
                (
                    Binding::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    Action::Interrupt,
                ),
                (
                    Binding::new(KeyCode::Esc, KeyModifiers::NONE),
                    Action::Escape,
                ),
                (
                    Binding::new(KeyCode::Tab, KeyModifiers::NONE),
                    Action::ToggleFold,
                ),
                (
                    Binding::new(KeyCode::PageUp, KeyModifiers::NONE),
                    Action::ScrollUp,
                ),
                (
                    Binding::new(KeyCode::PageDown, KeyModifiers::NONE),
                    Action::ScrollDown,
                ),
                (
                    Binding::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
                    Action::Guide,
                ),
                (
                    Binding::new(KeyCode::PageDown, KeyModifiers::CONTROL),
                    Action::NextSection,
                ),
                (
                    Binding::new(KeyCode::PageUp, KeyModifiers::CONTROL),
                    Action::PrevSection,
                ),
                (
                    Binding::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
                    Action::NewSection,
                ),
                (
                    Binding::new(KeyCode::F(1), KeyModifiers::NONE),
                    Action::Help,
                ),
            ],
        }
    }
}

impl Keymap {
    /// Resolve a key event to its action, or `None` when no shell binding claims
    /// it (the caller then forwards the key to the input editor). Scans newest
    /// binding first so an override shadows a default on a shared key.
    #[must_use]
    pub fn resolve(&self, key: &KeyEvent) -> Option<Action> {
        self.bindings
            .iter()
            .rev()
            .find(|(binding, _)| binding.matches(key))
            .map(|(_, action)| *action)
    }

    /// The first key bound to an action, formatted for on-screen hints, or the
    /// action's name when it is unbound. Lets the UI show the *actual* binding
    /// after a remap instead of a hardcoded key name that would go stale.
    #[must_use]
    pub fn hint_for(&self, action: Action) -> String {
        self.bindings
            .iter()
            .find(|(_, bound)| *bound == action)
            .map_or_else(
                || action.as_name().to_string(),
                |(binding, _)| format_binding(binding),
            )
    }

    /// Every action with its live bound key and one-line description, in
    /// canonical order, for the in-shell key-reference overlay. An unbound
    /// action still appears (its name in the key column) so the overlay is a
    /// complete map of the shell's verbs, not just the currently bound ones.
    #[must_use]
    pub fn help_rows(&self) -> Vec<(String, &'static str)> {
        ALL_ACTIONS
            .iter()
            .map(|action| (self.hint_for(*action), action.describe()))
            .collect()
    }

    /// Every key bound to an action, in binding order. Regroups the flat binding
    /// list back into per-action entries for export.
    #[must_use]
    fn keys_for(&self, action: Action) -> Vec<Binding> {
        self.bindings
            .iter()
            .filter(|(_, bound)| *bound == action)
            .map(|(binding, _)| *binding)
            .collect()
    }

    /// Serialize the effective keymap as a `keymap.toml` document (every action
    /// in canonical order), the inverse of [`Keymap::load`]. One key renders as
    /// a bare string, several as an array, and an unbound action as `[]`, so the
    /// export re-loads to the same bindings. Powers `hf config export keymap`.
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        let mut out = String::from("[keymap]\n");
        for action in ALL_ACTIONS {
            let keys: Vec<String> = self
                .keys_for(*action)
                .iter()
                .map(|binding| format!("\"{}\"", format_binding(binding)))
                .collect();
            let value = match keys.as_slice() {
                [] => "[]".to_string(),
                [one] => one.clone(),
                many => format!("[{}]", many.join(", ")),
            };
            out.push_str(&format!("{} = {value}\n", action.as_name()));
        }
        out
    }

    /// Layer user then project `keymap.toml` onto the defaults (project wins per
    /// action). Fault-isolated: an unreadable/unparseable file contributes
    /// nothing, a bad action name or key string is skipped, so a broken keymap
    /// can never abort startup.
    #[must_use]
    pub fn load(cwd: &Path, home: &Path) -> Self {
        let mut keymap = Self::default();
        for path in keymap_file_paths(cwd, home) {
            read_keymap_file(&path).apply_to(&mut keymap);
        }
        keymap
    }

    /// Rebind one action to a set of keys: drop its previous bindings, then
    /// claim each new key, warning when that steals a key from another action.
    /// An empty `keys` list unbinds the action entirely (it falls through to the
    /// editor), which is how a binding is deliberately disabled.
    fn rebind(&mut self, action: Action, keys: Vec<Binding>, source: &str) {
        self.bindings.retain(|(_, existing)| *existing != action);
        for binding in keys {
            if let Some((_, displaced)) = self
                .bindings
                .iter()
                .find(|(existing, _)| *existing == binding)
                .copied()
            {
                warn!(
                    file = %source,
                    key = %format_binding(&binding),
                    from = displaced.as_name(),
                    to = action.as_name(),
                    "keymap key reassigned away from its previous action"
                );
            }
            self.bindings.retain(|(existing, _)| *existing != binding);
            self.bindings.push((binding, action));
        }
    }
}

/// Raw `[keymap]` overrides parsed from one file, in file order, plus the path
/// they came from (for warnings). Applied onto a [`Keymap`] action by action.
struct KeymapOverrides {
    source: String,
    entries: Vec<(Action, Vec<Binding>)>,
}

impl KeymapOverrides {
    /// An empty override layer (missing/unreadable/unparseable file).
    fn none(source: String) -> Self {
        Self {
            source,
            entries: Vec::new(),
        }
    }

    /// Extract `[keymap]` entries, skipping unknown action names with a warning.
    fn from_table(table: &toml::Table, source: &str) -> Self {
        let mut entries = Vec::new();
        let Some(keymap) = table.get("keymap").and_then(toml::Value::as_table) else {
            return Self {
                source: source.to_string(),
                entries,
            };
        };
        for (name, value) in keymap {
            let Some(action) = Action::from_name(name) else {
                warn!(file = %source, action = %name, "unknown keymap action; ignored");
                continue;
            };
            entries.push((action, parse_key_value(value, name, source)));
        }
        Self {
            source: source.to_string(),
            entries,
        }
    }

    /// Apply every override onto `keymap` in file order.
    fn apply_to(self, keymap: &mut Keymap) {
        for (action, keys) in self.entries {
            keymap.rebind(action, keys, &self.source);
        }
    }
}

/// Keymap file merge order mirrors `theme.toml`/`config.toml`: user then
/// project (project wins per action).
pub(crate) fn keymap_file_paths(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".heartflow").join("keymap.toml"),
        cwd.join(".heartflow").join("keymap.toml"),
    ]
}

/// Read one keymap file with full fault isolation, mirroring `read_theme_file`.
fn read_keymap_file(path: &Path) -> KeymapOverrides {
    let source = path.display().to_string();
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return KeymapOverrides::none(source)
        }
        Err(error) => {
            warn!(file = %source, error = %error, "keymap file unreadable; skipped");
            return KeymapOverrides::none(source);
        }
    };
    match toml::from_str::<toml::Table>(&contents) {
        Ok(table) => KeymapOverrides::from_table(&table, &source),
        Err(error) => {
            warn!(file = %source, error = %error, "keymap file unparseable; skipped");
            KeymapOverrides::none(source)
        }
    }
}

/// Parse one action's value: a single key string or an array of them. A wrong
/// type or a non-string array entry is skipped with a warning.
fn parse_key_value(value: &toml::Value, action: &str, source: &str) -> Vec<Binding> {
    match value {
        toml::Value::String(text) => parse_keys(&[text.as_str()], action, source),
        toml::Value::Array(items) => {
            let texts: Vec<&str> = items
                .iter()
                .filter_map(|item| match item.as_str() {
                    Some(text) => Some(text),
                    None => {
                        warn!(file = %source, action = %action, "keymap entry must be a string; skipped");
                        None
                    }
                })
                .collect();
            parse_keys(&texts, action, source)
        }
        _ => {
            warn!(file = %source, action = %action, "keymap value must be a string or array; skipped");
            Vec::new()
        }
    }
}

/// Parse each key string, dropping (with a warning) any that are unrecognized.
fn parse_keys(texts: &[&str], action: &str, source: &str) -> Vec<Binding> {
    texts
        .iter()
        .filter_map(|text| {
            parse_binding(text).or_else(|| {
                warn!(file = %source, action = %action, key = %text.trim(), "unrecognized key; skipped");
                None
            })
        })
        .collect()
}

/// Parse a key string such as `"ctrl+shift+p"`, `"tab"`, or `"c"` into a
/// binding. Modifier segments may appear in any order; the key itself is the
/// one non-modifier segment. Returns `None` for an empty string, two keys, or an
/// unrecognized key name.
fn parse_binding(text: &str) -> Option<Binding> {
    let mut modifiers = KeyModifiers::NONE;
    let mut key_part: Option<&str> = None;
    for segment in text.split('+') {
        let token = segment.trim();
        if token.is_empty() {
            return None;
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "opt" | "option" | "meta" => modifiers |= KeyModifiers::ALT,
            "super" | "cmd" | "command" | "win" => modifiers |= KeyModifiers::SUPER,
            // Shift is accepted syntactically but masked out of the match (see
            // `mod_mask`); bind `backtab` for the shifted-Tab code.
            "shift" => modifiers |= KeyModifiers::SHIFT,
            _ => {
                if key_part.is_some() {
                    return None; // two key names in one binding
                }
                key_part = Some(token);
            }
        }
    }
    Some(Binding::new(parse_key_code(key_part?)?, modifiers))
}

/// Map a bare key name to a [`KeyCode`]: function keys `f1..=f24`, the named
/// navigation/editing keys, `space`, or a single character. Anything else
/// (multi-character, empty) is `None`.
fn parse_key_code(name: &str) -> Option<KeyCode> {
    let lower = name.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix('f') {
        if let Ok(n) = digits.parse::<u8>() {
            if (1..=24).contains(&n) {
                return Some(KeyCode::F(n));
            }
        }
    }
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "esc" | "escape" => KeyCode::Esc,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pagedn" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "insert" | "ins" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        "backspace" | "bs" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return None,
            }
        }
    };
    Some(code)
}

/// Render a binding back to its canonical key string (modifiers first, then the
/// key name). Used in warnings today and by `keymap.toml` export later.
fn format_binding(binding: &Binding) -> String {
    let mut parts: Vec<String> = Vec::new();
    if binding.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if binding.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if binding.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("super".to_string());
    }
    parts.push(key_name(binding.code));
    parts.join("+")
}

/// The canonical name for a key code, inverse of [`parse_key_code`] for every
/// code a binding can hold. Codes with no readable name render as `?` (they are
/// never produced by the parser or the defaults).
fn key_name(code: KeyCode) -> String {
    match code {
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "backtab".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn default_keymap_resolves_the_shipped_bindings() {
        let keymap = Keymap::default();
        assert_eq!(keymap.resolve(&key(KeyCode::Enter)), Some(Action::Submit));
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::Char('c'))),
            Some(Action::Interrupt)
        );
        assert_eq!(keymap.resolve(&key(KeyCode::Esc)), Some(Action::Escape));
        assert_eq!(keymap.resolve(&key(KeyCode::Tab)), Some(Action::ToggleFold));
        assert_eq!(
            keymap.resolve(&key(KeyCode::PageUp)),
            Some(Action::ScrollUp)
        );
        assert_eq!(
            keymap.resolve(&key(KeyCode::PageDown)),
            Some(Action::ScrollDown)
        );
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::Char('g'))),
            Some(Action::Guide)
        );
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::PageDown)),
            Some(Action::NextSection)
        );
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::PageUp)),
            Some(Action::PrevSection)
        );
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::Char('t'))),
            Some(Action::NewSection)
        );
        assert_eq!(keymap.resolve(&key(KeyCode::F(1))), Some(Action::Help));
    }

    #[test]
    fn help_rows_cover_every_action_with_a_live_key() {
        let keymap = Keymap::default();
        let rows = keymap.help_rows();
        assert_eq!(rows.len(), ALL_ACTIONS.len());
        // The Help action's own row shows its bound key (f1) and its description.
        let help = keymap.hint_for(Action::Help);
        assert!(
            rows.iter()
                .any(|(key, desc)| *key == help && *desc == Action::Help.describe()),
            "the overlay lists the help binding itself"
        );
    }

    #[test]
    fn unbound_keys_resolve_to_none_so_the_editor_gets_them() {
        let keymap = Keymap::default();
        // A plain character is not a shell binding; it must fall through to the
        // input editor rather than being swallowed.
        assert_eq!(keymap.resolve(&key(KeyCode::Char('x'))), None);
        // Ctrl+C is bound, but a bare 'c' (no modifier) is not.
        assert_eq!(keymap.resolve(&key(KeyCode::Char('c'))), None);
    }

    #[test]
    fn binding_match_ignores_shift_and_character_case() {
        let binding = Binding::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        // Caps lock / an uppercase char event still matches the ctrl+c binding.
        assert!(binding.matches(&KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        // Without the control modifier it does not.
        assert!(!binding.matches(&key(KeyCode::Char('c'))));
    }

    #[test]
    fn parse_binding_handles_chords_names_and_chars() {
        assert_eq!(
            parse_binding("ctrl+c"),
            Some(Binding::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_binding("Ctrl+Shift+P"),
            Some(Binding::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_binding("alt+up"),
            Some(Binding::new(KeyCode::Up, KeyModifiers::ALT))
        );
        assert_eq!(
            parse_binding("pageup"),
            Some(Binding::new(KeyCode::PageUp, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("f5"),
            Some(Binding::new(KeyCode::F(5), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("space"),
            Some(Binding::new(KeyCode::Char(' '), KeyModifiers::NONE))
        );
    }

    #[test]
    fn parse_binding_rejects_malformed_strings() {
        assert_eq!(parse_binding(""), None); // empty
        assert_eq!(parse_binding("ctrl+"), None); // dangling modifier
        assert_eq!(parse_binding("a+b"), None); // two keys
        assert_eq!(parse_binding("nope"), None); // multi-char, not a name
        assert_eq!(parse_binding("f99"), None); // function key out of range
    }

    #[test]
    fn format_binding_round_trips_through_the_parser() {
        for text in [
            "ctrl+c", "alt+up", "tab", "enter", "esc", "pageup", "pagedown", "f5", "space",
            "backtab",
        ] {
            let binding = parse_binding(text).unwrap_or_else(|| panic!("parse {text}"));
            // The canonical form re-parses to the same binding (aliases like
            // "pgup" normalize to "pageup", so compare bindings, not strings).
            let reparsed = parse_binding(&format_binding(&binding));
            assert_eq!(reparsed, Some(binding), "round trip {text}");
        }
    }

    #[test]
    fn project_override_remaps_an_action_and_drops_its_old_key() {
        let home = TempDir::new("km-user");
        let project = TempDir::new("km-proj");
        project.write_keymap("[keymap]\ntoggle_fold = \"ctrl+space\"\n");
        let keymap = Keymap::load(project.path(), home.path());
        // The new chord folds...
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::Char(' '))),
            Some(Action::ToggleFold)
        );
        // ...and the default Tab no longer does (it now falls through to editor).
        assert_eq!(keymap.resolve(&key(KeyCode::Tab)), None);
        // Untouched actions keep their defaults.
        assert_eq!(keymap.resolve(&key(KeyCode::Enter)), Some(Action::Submit));
    }

    #[test]
    fn project_layer_wins_over_user_layer_per_action() {
        let home = TempDir::new("km-user2");
        home.write_keymap("[keymap]\nsubmit = \"ctrl+j\"\nscroll_up = \"f1\"\n");
        let project = TempDir::new("km-proj2");
        project.write_keymap("[keymap]\nsubmit = \"ctrl+k\"\n");
        let keymap = Keymap::load(project.path(), home.path());
        // Project restates submit (wins); user-only scroll_up still applies.
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::Char('k'))),
            Some(Action::Submit)
        );
        assert_eq!(keymap.resolve(&ctrl(KeyCode::Char('j'))), None);
        assert_eq!(keymap.resolve(&key(KeyCode::F(1))), Some(Action::ScrollUp));
    }

    #[test]
    fn reassigning_a_bound_key_steals_it_from_the_previous_action() {
        let home = TempDir::new("km-conflict");
        home.write_keymap("[keymap]\nscroll_up = \"ctrl+c\"\n");
        let project = TempDir::new("km-conflict-proj");
        let keymap = Keymap::load(project.path(), home.path());
        // ctrl+c now scrolls up; the interrupt binding was displaced.
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::Char('c'))),
            Some(Action::ScrollUp)
        );
    }

    #[test]
    fn an_action_can_bind_several_keys_at_once() {
        let home = TempDir::new("km-multi");
        home.write_keymap("[keymap]\nsubmit = [\"enter\", \"ctrl+j\"]\n");
        let project = TempDir::new("km-multi-proj");
        let keymap = Keymap::load(project.path(), home.path());
        assert_eq!(keymap.resolve(&key(KeyCode::Enter)), Some(Action::Submit));
        assert_eq!(
            keymap.resolve(&ctrl(KeyCode::Char('j'))),
            Some(Action::Submit)
        );
    }

    #[test]
    fn an_empty_list_unbinds_an_action() {
        let home = TempDir::new("km-unbind");
        home.write_keymap("[keymap]\ntoggle_fold = []\n");
        let project = TempDir::new("km-unbind-proj");
        let keymap = Keymap::load(project.path(), home.path());
        assert_eq!(keymap.resolve(&key(KeyCode::Tab)), None);
    }

    #[test]
    fn unknown_actions_and_bad_keys_are_skipped_without_poisoning_the_rest() {
        let home = TempDir::new("km-bad");
        home.write_keymap(concat!(
            "[keymap]\n",
            "launch_missiles = \"ctrl+m\"\n", // unknown action
            "submit = \"not a key\"\n",       // malformed key -> unbinds submit
            "toggle_fold = \"f2\"\n",         // good sibling still applies
        ));
        let project = TempDir::new("km-bad-proj");
        let keymap = Keymap::load(project.path(), home.path());
        assert_eq!(
            keymap.resolve(&key(KeyCode::F(2))),
            Some(Action::ToggleFold)
        );
        // The malformed key dropped submit's only binding; nothing claims Enter.
        assert_eq!(keymap.resolve(&key(KeyCode::Enter)), None);
    }

    #[test]
    fn missing_keymap_files_yield_the_default_bindings() {
        let cwd = TempDir::new("km-none-cwd");
        let home = TempDir::new("km-none-home");
        assert_eq!(Keymap::load(cwd.path(), home.path()), Keymap::default());
    }

    #[test]
    fn unparseable_keymap_file_does_not_abort() {
        let project = TempDir::new("km-broken");
        project.write_keymap("this is = = not valid toml [[[\n");
        let home = TempDir::new("km-broken-home");
        // A corrupt file degrades to the default layer rather than panicking.
        assert_eq!(Keymap::load(project.path(), home.path()), Keymap::default());
    }

    /// Reload an exported `keymap.toml` and return the resulting map, so tests
    /// can assert the export is faithful to its source.
    fn reload_exported(keymap: &Keymap) -> Keymap {
        let exported = keymap.to_toml_string();
        let table = toml::from_str::<toml::Table>(&exported).expect("export emits valid TOML");
        let mut reloaded = Keymap::default();
        KeymapOverrides::from_table(&table, "export").apply_to(&mut reloaded);
        reloaded
    }

    /// Assert two keymaps bind the same keys to the same actions. Binding order
    /// across actions is an implementation detail (a `Vec`), so compare each
    /// action's key set rather than the whole struct.
    fn assert_same_bindings(left: &Keymap, right: &Keymap) {
        for action in ALL_ACTIONS {
            assert_eq!(
                left.keys_for(*action),
                right.keys_for(*action),
                "action {} differs after export+reload",
                action.as_name()
            );
        }
    }

    #[test]
    fn exported_default_keymap_round_trips() {
        let keymap = Keymap::default();
        assert_same_bindings(&reload_exported(&keymap), &keymap);
    }

    #[test]
    fn exported_remap_with_multi_binding_and_unbind_round_trips() {
        let home = TempDir::new("km-export");
        home.write_keymap("[keymap]\nsubmit = [\"enter\", \"ctrl+j\"]\ntoggle_fold = []\n");
        let project = TempDir::new("km-export-proj");
        let keymap = Keymap::load(project.path(), home.path());
        // A multi-key action and a deliberately unbound one both survive export.
        assert_same_bindings(&reload_exported(&keymap), &keymap);
    }

    /// A self-deleting temp dir holding a `.heartflow/keymap.toml` for load tests.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("hf-keymap-{tag}-{nanos}"));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
        fn write_keymap(&self, contents: &str) {
            let heart = self.0.join(".heartflow");
            fs::create_dir_all(&heart).expect("create .heartflow");
            fs::write(heart.join("keymap.toml"), contents).expect("write keymap.toml");
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
