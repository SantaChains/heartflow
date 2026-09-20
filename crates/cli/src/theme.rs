//! Single source of truth for the terminal's visual language.
//!
//! The REPL draws through *two* different terminal backends: the streaming line
//! renderer ([`crate::render`]) emits crossterm ANSI, while the idle input
//! viewport ([`crate::editor`] and any future ratatui overlay) speaks ratatui
//! styles. Those layers expose incompatible `Color` types, so before this module
//! each hardcoded its own hues and the two drifted (a loud 16-color ANSI palette
//! on one side, ad-hoc `Color::DarkGray` on the other). [`Theme`] fixes the
//! palette once as [`Rgb`] tokens and projects the *same* hue into either
//! backend via [`Rgb::crossterm`] / [`Rgb::ratatui`], so headings, the prompt
//! gutter, hints, and status all stay in one coordinated color family.
//!
//! Glyphs and layout (prompt gutter, bullet, box corners, spinner marks) are
//! centralized here too, so proportion and typography can be tuned in one place
//! rather than hunted across render/editor/main.
//!
//! The palette is overridable: `theme.toml` under `~/.heartflow` (user) and
//! `.heartflow` (project, wins per token) may restate any semantic role as a
//! `#RRGGBB` string. Overrides are layered onto the built-in palette with the
//! same field-by-field fault isolation as `config.toml` — a bad or missing file
//! never blocks startup, a bad color is skipped with a warning — and the
//! resolved theme is cached process-wide in [`Theme::current`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing::warn;

/// An RGB color stored independently of any terminal backend so the same hue can
/// be projected into both crossterm and ratatui without a second palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    /// Construct from 8-bit red/green/blue channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Project into a crossterm color (24-bit truecolor), used by the line
    /// renderer and spinner.
    #[must_use]
    pub const fn crossterm(self) -> crossterm::style::Color {
        crossterm::style::Color::Rgb {
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }

    /// Project into a ratatui color (24-bit truecolor), used by the inline
    /// input editor and any ratatui overlay.
    #[must_use]
    pub const fn ratatui(self) -> ratatui::style::Color {
        ratatui::style::Color::Rgb(self.r, self.g, self.b)
    }
}

/// The coordinated semantic palette plus the fixed glyph/layout tokens.
///
/// Hues are derived from concrete objects in Akira Kurosawa's films rather than
/// picked off a wheel, which is what keeps the set coherent: every warm tone is
/// lacquer or leaf metal from Ran / Kagemusha / Rashomon, every cool tone is a
/// glaze, moss or fog from Rashomon / Throne of Blood, and the neutrals are the
/// hemp and mist that carry the frames. Two disciplines hold it together — one
/// hue per role, and a deliberate lightness ladder (heading mid, strong bright,
/// muted dim) so scanning order is readable before color is even noticed. The
/// single high-chroma accent is Third Son's ultramarine banner in Ran, the one
/// cool event against a golden field; making it the prompt and spinner is what
/// gives the REPL its signature. Body text stays uncolored so it inherits the
/// user's own foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    heading: Rgb,
    emphasis: Rgb,
    strong: Rgb,
    inline_code: Rgb,
    link: Rgb,
    quote: Rgb,
    accent: Rgb,
    muted: Rgb,
    success: Rgb,
    error: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        // Kurosawa-derived (RGB), one object per role. Kept as literals so both
        // backends receive byte-identical hues; see the module note on why one
        // palette matters.
        Self {
            heading: Rgb::new(228, 97, 60), // 朱漆 vermilion lacquer (Rashomon gate)
            emphasis: Rgb::new(169, 139, 216), // 梦・紫 twilight lavender (Dreams)
            strong: Rgb::new(232, 180, 74), // 金箔 gold leaf (Kagemusha screens)
            inline_code: Rgb::new(143, 191, 106), // 苔 wet moss (Rashomon forest)
            link: Rgb::new(111, 191, 168),  // 青磁 celadon glaze
            quote: Rgb::new(138, 147, 163), // 蜘蛛巢城・雾 fog (Throne of Blood)
            accent: Rgb::new(85, 136, 238), // 乱・三郎的蓝旗 ultramarine banner (Ran)
            muted: Rgb::new(125, 114, 105), // 麻布 hemp (deltas, secondary)
            success: Rgb::new(143, 191, 106), // 苔 jade, shared with `inline_code`
            error: Rgb::new(225, 75, 99),   // 绯 rose-crimson (High and Low)
        }
    }
}

impl Theme {
    /// Markdown heading color.
    #[must_use]
    pub const fn heading(&self) -> Rgb {
        self.heading
    }
    /// Italic/emphasized text color.
    #[must_use]
    pub const fn emphasis(&self) -> Rgb {
        self.emphasis
    }
    /// Bold/strong text color.
    #[must_use]
    pub const fn strong(&self) -> Rgb {
        self.strong
    }
    /// Inline `code` span color.
    #[must_use]
    pub const fn inline_code(&self) -> Rgb {
        self.inline_code
    }
    /// Link color.
    #[must_use]
    pub const fn link(&self) -> Rgb {
        self.link
    }
    /// Block-quote bar/text color.
    #[must_use]
    pub const fn quote(&self) -> Rgb {
        self.quote
    }
    /// Primary accent: prompt gutter, active spinner, completion highlight.
    #[must_use]
    pub const fn accent(&self) -> Rgb {
        self.accent
    }
    /// Secondary/dimmed text: live deltas, idle hints, neutral cancel.
    #[must_use]
    pub const fn muted(&self) -> Rgb {
        self.muted
    }
    /// Success (turn/tool done) color.
    #[must_use]
    pub const fn success(&self) -> Rgb {
        self.success
    }
    /// Failure color.
    #[must_use]
    pub const fn error(&self) -> Rgb {
        self.error
    }
}

/// Process-wide resolved theme, populated on the first [`Theme::current`] call
/// and reused thereafter (terminal rendering happens on every frame, so the
/// files are read once, not per line).
static THEME: OnceLock<Theme> = OnceLock::new();

impl Theme {
    /// Build the palette by layering user then project `theme.toml` overrides
    /// (project wins per token) onto the built-in defaults. Fault-isolated: an
    /// unreadable/unparseable file contributes nothing, a malformed color is
    /// skipped, so a broken theme file can never abort startup.
    #[must_use]
    pub fn load(cwd: &Path, home: &Path) -> Self {
        let mut theme = Self::default();
        for path in theme_file_paths(cwd, home) {
            read_theme_file(&path).apply_to(&mut theme);
        }
        theme
    }

    /// The active palette for this process: [`Theme::load`] of the on-disk
    /// overrides, resolved once and cached. Returns a reference so hot render
    /// paths read a pointer rather than rebuild the palette.
    #[must_use]
    pub fn current() -> &'static Theme {
        THEME.get_or_init(|| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            Theme::load(&cwd, &crate::home_dir())
        })
    }
}

/// Raw `[theme]` overrides, one optional `#RRGGBB` per semantic role. Every
/// field is optional so a partial file (or older schema with unknown keys)
/// loads unchanged; a field that fails to parse is dropped in isolation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ThemeSettings {
    heading: Option<Rgb>,
    emphasis: Option<Rgb>,
    strong: Option<Rgb>,
    inline_code: Option<Rgb>,
    link: Option<Rgb>,
    quote: Option<Rgb>,
    accent: Option<Rgb>,
    muted: Option<Rgb>,
    success: Option<Rgb>,
    error: Option<Rgb>,
}

impl ThemeSettings {
    fn from_table(table: &toml::Table, source: &str) -> Self {
        let Some(theme) = table.get("theme").and_then(toml::Value::as_table) else {
            return Self::default();
        };
        for key in theme.keys() {
            if !matches!(
                key.as_str(),
                "heading"
                    | "emphasis"
                    | "strong"
                    | "inline_code"
                    | "link"
                    | "quote"
                    | "accent"
                    | "muted"
                    | "success"
                    | "error"
            ) {
                tracing::debug!(file = %source, field = %key, "unknown theme token; ignored");
            }
        }
        Self {
            heading: rgb_field(theme, "heading", source),
            emphasis: rgb_field(theme, "emphasis", source),
            strong: rgb_field(theme, "strong", source),
            inline_code: rgb_field(theme, "inline_code", source),
            link: rgb_field(theme, "link", source),
            quote: rgb_field(theme, "quote", source),
            accent: rgb_field(theme, "accent", source),
            muted: rgb_field(theme, "muted", source),
            success: rgb_field(theme, "success", source),
            error: rgb_field(theme, "error", source),
        }
    }

    /// Apply present overrides onto `theme`; `None` roles keep their current hue.
    fn apply_to(self, theme: &mut Theme) {
        let Self {
            heading,
            emphasis,
            strong,
            inline_code,
            link,
            quote,
            accent,
            muted,
            success,
            error,
        } = self;
        for (slot, value) in [
            (&mut theme.heading, heading),
            (&mut theme.emphasis, emphasis),
            (&mut theme.strong, strong),
            (&mut theme.inline_code, inline_code),
            (&mut theme.link, link),
            (&mut theme.quote, quote),
            (&mut theme.accent, accent),
            (&mut theme.muted, muted),
            (&mut theme.success, success),
            (&mut theme.error, error),
        ] {
            if let Some(rgb) = value {
                *slot = rgb;
            }
        }
    }
}

/// Theme file merge order mirrors `config.toml`: user then project (project wins).
fn theme_file_paths(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".heartflow").join("theme.toml"),
        cwd.join(".heartflow").join("theme.toml"),
    ]
}

/// Read one theme file with full fault isolation: missing/unreadable/unparseable
/// files yield an empty override layer (a warning for the latter two).
fn read_theme_file(path: &Path) -> ThemeSettings {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ThemeSettings::default()
        }
        Err(error) => {
            warn!(file = %path.display(), error = %error, "theme file unreadable; skipped");
            return ThemeSettings::default();
        }
    };
    match toml::from_str::<toml::Table>(&contents) {
        Ok(table) => ThemeSettings::from_table(&table, &path.display().to_string()),
        Err(error) => {
            warn!(file = %path.display(), error = %error, "theme file unparseable; skipped");
            ThemeSettings::default()
        }
    }
}

/// Extract one `#RRGGBB` token, warning and skipping a wrong type or malformed value.
fn rgb_field(table: &toml::Table, key: &str, source: &str) -> Option<Rgb> {
    let value = table.get(key)?;
    let Some(text) = value.as_str() else {
        warn!(file = %source, field = key, "theme color must be a string; skipped");
        return None;
    };
    let parsed = parse_rgb(text);
    if parsed.is_none() {
        warn!(file = %source, field = key, value = %text, "theme color is not #RRGGBB; skipped");
    }
    parsed
}

/// Parse `#RRGGBB` or `RRGGBB` (case-insensitive) into [`Rgb`]; anything else is
/// rejected so a typo falls back to the default rather than painting garbage.
fn parse_rgb(text: &str) -> Option<Rgb> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
    Some(Rgb::new(channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

/// Fixed glyphs and layout metrics shared by the renderer and editor. Centralized
/// so the REPL's typographic rhythm (gutter width, bullet, box, status marks) is
/// one decision rather than several that must be kept in sync by hand.
pub mod glyphs {
    /// The prompt marker drawn before user input.
    pub const PROMPT: &str = "› ";
    /// Display columns occupied by [`PROMPT`] (its terminal width, for layout).
    pub const PROMPT_WIDTH: u16 = 2;
    /// Unordered-list bullet.
    pub const BULLET: &str = "• ";
    /// Block-quote left bar.
    pub const QUOTE_BAR: &str = "│ ";
    /// Horizontal rule: a short box-drawing run, self-terminating, kept narrow
    /// so it never wraps on ordinary terminal widths (a full-width fixed rule
    /// would break layout on narrow terminals).
    pub const RULE: &str = "──────\n";
    /// Open corner for a fenced code block; the language follows after a space.
    pub const CODE_OPEN: &str = "╭─ ";
    /// Close corner for a fenced code block.
    pub const CODE_CLOSE: &str = "╰─";
    /// Completed/ok state.
    pub const DONE: &str = "✔";
    /// Failed state.
    pub const FAILED: &str = "✘";
    /// User-cancelled (neutral, non-alarming) state.
    pub const CANCELLED: &str = "◌";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rgb_projects_identical_hue_to_both_backends() {
        let sample = Rgb::new(122, 162, 247);
        assert_eq!(
            sample.crossterm(),
            crossterm::style::Color::Rgb {
                r: 122,
                g: 162,
                b: 247
            }
        );
        assert_eq!(sample.ratatui(), ratatui::style::Color::Rgb(122, 162, 247));
    }

    #[test]
    fn default_palette_roles_are_distinct() {
        // A harmonized palette still needs each role distinguishable; guard
        // against two roles accidentally collapsing to the same hue.
        let theme = Theme::default();
        let roles = [
            theme.heading(),
            theme.emphasis(),
            theme.strong(),
            theme.inline_code(),
            theme.link(),
            theme.error(),
        ];
        let mut unique = roles.to_vec();
        unique.dedup();
        assert_eq!(
            unique.len(),
            roles.len(),
            "primary roles must not share a hue"
        );
    }

    #[test]
    fn prompt_width_matches_prompt_glyph() {
        // Layout math assumes the gutter is exactly `PROMPT_WIDTH` cells wide;
        // if the prompt text changes width the input viewport would misalign.
        assert_eq!(
            glyphs::PROMPT.chars().count(),
            usize::from(glyphs::PROMPT_WIDTH)
        );
    }

    #[test]
    fn parse_rgb_accepts_hash_and_bare_hex() {
        assert_eq!(parse_rgb("#FF0000"), Some(Rgb::new(255, 0, 0)));
        assert_eq!(parse_rgb("0a1b2c"), Some(Rgb::new(10, 27, 44)));
        assert_eq!(parse_rgb("  #7aa2f7  "), Some(Rgb::new(122, 162, 247)));
    }

    #[test]
    fn parse_rgb_rejects_malformed_values() {
        assert_eq!(parse_rgb("#12345"), None); // too short
        assert_eq!(parse_rgb("#1234567"), None); // too long
        assert_eq!(parse_rgb("zzzzzz"), None); // not hex
        assert_eq!(parse_rgb("red"), None); // not a color literal
    }

    #[test]
    fn valid_override_replaces_only_named_role() {
        let project = TempDir::new("override");
        project.write_theme("[theme]\naccent = \"#ff0000\"\n");
        let home = TempDir::new("override-home");
        let theme = Theme::load(project.path(), home.path());
        assert_eq!(theme.accent(), Rgb::new(255, 0, 0));
        // Untouched roles keep the built-in hue.
        assert_eq!(theme.heading(), Theme::default().heading());
        assert_eq!(theme.error(), Theme::default().error());
    }

    #[test]
    fn malformed_color_is_skipped_without_poisoning_other_fields() {
        let project = TempDir::new("malformed");
        project.write_theme("[theme]\naccent = \"not-a-color\"\nheading = \"#00ff00\"\n");
        let home = TempDir::new("malformed-home");
        let theme = Theme::load(project.path(), home.path());
        // The bad token falls back to default; the good sibling still applies.
        assert_eq!(theme.accent(), Theme::default().accent());
        assert_eq!(theme.heading(), Rgb::new(0, 255, 0));
    }

    #[test]
    fn project_override_wins_over_user() {
        let home = TempDir::new("user");
        home.write_theme("[theme]\nheading = \"#111111\"\nemphasis = \"#111111\"\n");
        let project = TempDir::new("proj");
        project.write_theme("[theme]\nheading = \"#222222\"\n");
        let theme = Theme::load(project.path(), home.path());
        // Project restates heading (wins); emphasis only set by user still applies.
        assert_eq!(theme.heading(), Rgb::new(34, 34, 34));
        assert_eq!(theme.emphasis(), Rgb::new(17, 17, 17));
    }

    #[test]
    fn missing_theme_files_yield_builtin_palette() {
        let cwd = TempDir::new("none-cwd");
        let home = TempDir::new("none-home");
        assert_eq!(Theme::load(cwd.path(), home.path()), Theme::default());
    }

    #[test]
    fn unparseable_theme_file_does_not_abort() {
        let project = TempDir::new("broken");
        project.write_theme("this is = = not valid toml [[[\n");
        let home = TempDir::new("broken-home");
        // A corrupt file degrades to the default layer rather than panicking.
        assert_eq!(Theme::load(project.path(), home.path()), Theme::default());
    }

    /// A self-deleting temp dir holding a `.heartflow/theme.toml` for load tests.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("hf-theme-{tag}-{nanos}"));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
        fn write_theme(&self, contents: &str) {
            let heart = self.0.join(".heartflow");
            fs::create_dir_all(&heart).expect("create .heartflow");
            fs::write(heart.join("theme.toml"), contents).expect("write theme.toml");
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
