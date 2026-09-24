//! Tunable shell behavior, on disk as `settings.toml`.
//!
//! Where [`crate::theme`] owns how the shell looks and [`crate::keymap`] owns
//! how it is driven, [`Settings`] owns a handful of behavior knobs that were
//! previously compile-time constants (scroll stride, the tool-output fold
//! threshold, whether reasoning starts folded, the redraw throttle). Exposing
//! them is what lets a user tune the shell's feel without a rebuild, and keeping
//! them in one struct held by the shell means a turn-boundary hot-reload can
//! swap them just like the keymap.
//!
//! The file mirrors `theme.toml`/`keymap.toml` exactly: a `[settings]` table,
//! layered user (`~/.heartflow/settings.toml`) then project
//! (`.heartflow/settings.toml`, which wins per key), every field optional with a
//! default matching the built-in behavior. Loading is fault-isolated — a
//! missing/unreadable/unparseable file contributes nothing, a wrong-typed or
//! out-of-range value is skipped with a warning — so a broken settings file can
//! never abort startup or silently change behavior the user did not ask for.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;

/// Behavior knobs for the full-screen shell. Every default reproduces the
/// built-in behavior, so an absent or partial `settings.toml` changes nothing
/// until the user deliberately restates a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Lines scrolled per PageUp/PageDown press. Small enough to keep
    /// surrounding context visible, large enough to move meaningfully.
    pub scroll_step: u16,
    /// Tool outputs with at most this many lines start expanded; longer ones
    /// fold to a one-row marker (the fold toggle still reveals them). Raise it
    /// to see more inline, or to `0` to fold every tool result.
    pub tool_inline_lines: usize,
    /// Whether streamed reasoning starts folded. Reasoning is process, not
    /// output, so it defaults to folded; set `false` to open it inline.
    pub fold_thinking: bool,
    /// Redraw throttle in milliseconds: the frame-merge window for the event
    /// loop. Lower is smoother but busier; higher is calmer but laggier. Applied
    /// when the shell starts (a mid-session edit takes effect on the next run).
    pub frame_budget_ms: u64,
    /// When true, disable decorative animations (mascot bounce, progress
    /// shimmer, float effects) so the UI is static except for content
    /// updates. An accessibility / low-distraction knob.
    pub reduced_motion: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scroll_step: 3,
            tool_inline_lines: 3,
            fold_thinking: true,
            frame_budget_ms: 80,
            reduced_motion: false,
        }
    }
}

impl Settings {
    /// Layer user then project `settings.toml` overrides (project wins per key)
    /// onto the defaults. Fault-isolated: a bad file or value contributes
    /// nothing, so a broken settings file can never abort startup.
    #[must_use]
    pub fn load(cwd: &Path, home: &Path) -> Self {
        let mut settings = Self::default();
        for path in settings_file_paths(cwd, home) {
            read_settings_file(&path).apply_to(&mut settings);
        }
        settings
    }

    /// Serialize the effective settings as a `settings.toml` document (every
    /// knob), the inverse of [`Settings::load`]. Powers `hf config export
    /// settings` so a user gets a fully-specified, editable starting point.
    #[must_use]
    pub fn to_toml_string(self) -> String {
        format!(
            "[settings]\nscroll_step = {}\ntool_inline_lines = {}\nfold_thinking = {}\nframe_budget_ms = {}\nreduced_motion = {}\n",
            self.scroll_step, self.tool_inline_lines, self.fold_thinking, self.frame_budget_ms, self.reduced_motion
        )
    }
}

/// Raw `[settings]` overrides, one optional value per knob. Every field is
/// optional so a partial file (or a newer schema with unknown keys) loads
/// unchanged; a field that fails to parse is dropped in isolation.
#[derive(Debug, Clone, Copy, Default)]
struct SettingsOverrides {
    scroll_step: Option<u16>,
    tool_inline_lines: Option<usize>,
    fold_thinking: Option<bool>,
    frame_budget_ms: Option<u64>,
    reduced_motion: Option<bool>,
}

impl SettingsOverrides {
    fn from_table(table: &toml::Table, source: &str) -> Self {
        let Some(settings) = table.get("settings").and_then(toml::Value::as_table) else {
            return Self::default();
        };
        for key in settings.keys() {
            if !matches!(
                key.as_str(),
                "scroll_step"
                    | "tool_inline_lines"
                    | "fold_thinking"
                    | "frame_budget_ms"
                    | "reduced_motion"
            ) {
                tracing::debug!(file = %source, field = %key, "unknown setting; ignored");
            }
        }
        Self {
            scroll_step: opt_u16(settings, "scroll_step", source),
            tool_inline_lines: opt_usize(settings, "tool_inline_lines", source),
            fold_thinking: opt_bool(settings, "fold_thinking", source),
            frame_budget_ms: opt_u64(settings, "frame_budget_ms", source),
            reduced_motion: opt_bool(settings, "reduced_motion", source),
        }
    }

    /// Apply present overrides onto `settings`; `None` knobs keep their default.
    fn apply_to(self, settings: &mut Settings) {
        if let Some(value) = self.scroll_step {
            settings.scroll_step = value;
        }
        if let Some(value) = self.tool_inline_lines {
            settings.tool_inline_lines = value;
        }
        if let Some(value) = self.fold_thinking {
            settings.fold_thinking = value;
        }
        if let Some(value) = self.frame_budget_ms {
            settings.frame_budget_ms = value;
        }
        if let Some(value) = self.reduced_motion {
            settings.reduced_motion = value;
        }
    }
}

/// Settings file merge order mirrors `theme.toml`/`config.toml`: user then
/// project (project wins).
pub(crate) fn settings_file_paths(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".heartflow").join("settings.toml"),
        cwd.join(".heartflow").join("settings.toml"),
    ]
}

/// Read one settings file with full fault isolation, mirroring `read_theme_file`.
fn read_settings_file(path: &Path) -> SettingsOverrides {
    let source = path.display().to_string();
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SettingsOverrides::default()
        }
        Err(error) => {
            warn!(file = %source, error = %error, "settings file unreadable; skipped");
            return SettingsOverrides::default();
        }
    };
    match toml::from_str::<toml::Table>(&contents) {
        Ok(table) => SettingsOverrides::from_table(&table, &source),
        Err(error) => {
            warn!(file = %source, error = %error, "settings file unparseable; skipped");
            SettingsOverrides::default()
        }
    }
}

/// Extract one integer setting as a `u16`, warning and skipping a wrong type or
/// an out-of-range value so a typo falls back to the default.
fn opt_u16(table: &toml::Table, key: &str, source: &str) -> Option<u16> {
    let value = table.get(key)?;
    let Some(n) = value.as_integer() else {
        warn!(file = %source, field = key, "setting must be an integer; skipped");
        return None;
    };
    match u16::try_from(n) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            warn!(file = %source, field = key, value = n, "setting out of range; skipped");
            None
        }
    }
}

/// Extract one integer setting as a `usize` (rejects negatives), warning and
/// skipping a wrong type or an out-of-range value.
fn opt_usize(table: &toml::Table, key: &str, source: &str) -> Option<usize> {
    let value = table.get(key)?;
    let Some(n) = value.as_integer() else {
        warn!(file = %source, field = key, "setting must be an integer; skipped");
        return None;
    };
    match usize::try_from(n) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            warn!(file = %source, field = key, value = n, "setting out of range; skipped");
            None
        }
    }
}

/// Extract one integer setting as a `u64` (rejects negatives), warning and
/// skipping a wrong type or an out-of-range value.
fn opt_u64(table: &toml::Table, key: &str, source: &str) -> Option<u64> {
    let value = table.get(key)?;
    let Some(n) = value.as_integer() else {
        warn!(file = %source, field = key, "setting must be an integer; skipped");
        return None;
    };
    match u64::try_from(n) {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            warn!(file = %source, field = key, value = n, "setting out of range; skipped");
            None
        }
    }
}

/// Extract one boolean setting, warning and skipping a wrong type.
fn opt_bool(table: &toml::Table, key: &str, source: &str) -> Option<bool> {
    let value = table.get(key)?;
    match value.as_bool() {
        Some(parsed) => Some(parsed),
        None => {
            warn!(file = %source, field = key, "setting must be a boolean; skipped");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn defaults_reproduce_the_builtin_behavior() {
        let settings = Settings::default();
        assert_eq!(settings.scroll_step, 3);
        assert_eq!(settings.tool_inline_lines, 3);
        assert!(settings.fold_thinking, "reasoning folds by default");
        assert_eq!(settings.frame_budget_ms, 80);
        assert!(!settings.reduced_motion, "animations enabled by default");
    }

    #[test]
    fn override_replaces_only_the_named_key() {
        let project = TempDir::new("set-override");
        project.write_settings("[settings]\nscroll_step = 10\n");
        let home = TempDir::new("set-override-home");
        let settings = Settings::load(project.path(), home.path());
        assert_eq!(settings.scroll_step, 10);
        // Untouched knobs keep their defaults.
        assert_eq!(
            settings.tool_inline_lines,
            Settings::default().tool_inline_lines
        );
        assert_eq!(settings.fold_thinking, Settings::default().fold_thinking);
    }

    #[test]
    fn project_layer_wins_over_user_layer_per_key() {
        let home = TempDir::new("set-user");
        home.write_settings("[settings]\nscroll_step = 5\nfold_thinking = false\n");
        let project = TempDir::new("set-proj");
        project.write_settings("[settings]\nscroll_step = 8\n");
        let settings = Settings::load(project.path(), home.path());
        // Project restates scroll_step (wins); user-only fold_thinking applies.
        assert_eq!(settings.scroll_step, 8);
        assert!(!settings.fold_thinking);
    }

    #[test]
    fn wrong_type_is_skipped_without_poisoning_siblings() {
        let project = TempDir::new("set-type");
        project.write_settings("[settings]\nscroll_step = \"fast\"\nframe_budget_ms = 120\n");
        let home = TempDir::new("set-type-home");
        let settings = Settings::load(project.path(), home.path());
        // The bad value falls back to default; the good sibling still applies.
        assert_eq!(settings.scroll_step, Settings::default().scroll_step);
        assert_eq!(settings.frame_budget_ms, 120);
    }

    #[test]
    fn out_of_range_and_negative_integers_are_skipped() {
        let project = TempDir::new("set-range");
        project.write_settings("[settings]\nscroll_step = -1\ntool_inline_lines = 99999\n");
        let home = TempDir::new("set-range-home");
        let settings = Settings::load(project.path(), home.path());
        // A negative scroll_step is out of range for u16, so it keeps default.
        assert_eq!(settings.scroll_step, Settings::default().scroll_step);
        // A large-but-valid usize still applies.
        assert_eq!(settings.tool_inline_lines, 99999);
    }

    #[test]
    fn zero_tool_inline_lines_folds_every_result() {
        let project = TempDir::new("set-zero");
        project.write_settings("[settings]\ntool_inline_lines = 0\n");
        let home = TempDir::new("set-zero-home");
        let settings = Settings::load(project.path(), home.path());
        assert_eq!(settings.tool_inline_lines, 0);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let project = TempDir::new("set-unknown");
        project.write_settings("[settings]\nlaunch_missiles = true\nfold_thinking = false\n");
        let home = TempDir::new("set-unknown-home");
        let settings = Settings::load(project.path(), home.path());
        assert!(!settings.fold_thinking, "the known sibling still applies");
    }

    #[test]
    fn missing_settings_files_yield_defaults() {
        let cwd = TempDir::new("set-none-cwd");
        let home = TempDir::new("set-none-home");
        assert_eq!(Settings::load(cwd.path(), home.path()), Settings::default());
    }

    #[test]
    fn unparseable_settings_file_does_not_abort() {
        let project = TempDir::new("set-broken");
        project.write_settings("this is = = not valid toml [[[\n");
        let home = TempDir::new("set-broken-home");
        assert_eq!(
            Settings::load(project.path(), home.path()),
            Settings::default()
        );
    }

    #[test]
    fn exported_settings_round_trip_through_the_loader() {
        // The export must re-load to the exact knobs it came from, so
        // `config export settings` yields a faithful, editable template.
        let settings = Settings {
            scroll_step: 7,
            tool_inline_lines: 2,
            fold_thinking: false,
            frame_budget_ms: 120,
            reduced_motion: true,
        };
        let exported = settings.to_toml_string();
        let table = toml::from_str::<toml::Table>(&exported).expect("export emits valid TOML");
        let mut reloaded = Settings::default();
        SettingsOverrides::from_table(&table, "export").apply_to(&mut reloaded);
        assert_eq!(reloaded, settings);
    }

    /// A self-deleting temp dir holding a `.heartflow/settings.toml` for tests.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("hf-settings-{tag}-{nanos}"));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
        fn write_settings(&self, contents: &str) {
            let heart = self.0.join(".heartflow");
            fs::create_dir_all(&heart).expect("create .heartflow");
            fs::write(heart.join("settings.toml"), contents).expect("write settings.toml");
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
