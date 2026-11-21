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
//! `#RRGGBB` string, and a `palette` key selects the built-in base family
//! (`kurosawa`, the default, or `dracula`) those overrides layer onto — an
//! unknown name warns and falls back to Kurosawa. Overrides are layered onto
//! the built-in palette with the
//! same field-by-field fault isolation as `config.toml` — a bad or missing file
//! never blocks startup, a bad color is skipped with a warning — and the
//! resolved theme is cached process-wide behind a lock in [`Theme::current`],
//! which hands back a copy so a live edit can be swapped in without a restart
//! and the effective palette can be re-serialized via [`Theme::to_toml_string`].

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, PoisonError, RwLock};

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

    /// Render as a `#rrggbb` string, the inverse of [`parse_rgb`]; used to
    /// export the effective palette back to `theme.toml`.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// The coordinated semantic palette plus the fixed glyph/layout tokens.
///
/// Hues are derived from concrete objects in Akira Kurosawa's films rather than
/// picked off a wheel, which is what keeps the set coherent: every warm tone is
/// lacquer or leaf metal from Ran / Kagemusha / Rashomon, every cool tone is a
/// glaze, moss or fog from Rashomon / Throne of Blood, and the neutrals are the
/// mist that carries the frames. Two disciplines hold it together — one hue per
/// role, and a deliberate lightness ladder (heading mid, strong bright) so
/// scanning order is readable before color is even noticed; `muted` is the one
/// deliberate exception, sharing `strong`'s gold leaf instead of dimming. The
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
    /// Fenced-code keyword token (`fn`, `def`, `const`, ...).
    syntax_kw: Rgb,
    /// Fenced-code string-literal token.
    syntax_str: Rgb,
    /// Fenced-code comment token.
    syntax_comment: Rgb,
    /// Fenced-code numeric-literal token.
    syntax_num: Rgb,
    /// Fenced-code type-name token (`String`, `Result`, capitalized ids...).
    syntax_type: Rgb,
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
            muted: Rgb::new(232, 180, 74),  // 金箔 gold leaf, shared with `strong`
            success: Rgb::new(143, 191, 106), // 苔 jade, shared with `inline_code`
            error: Rgb::new(225, 75, 99),   // 绯 rose-crimson (High and Low)
            // Syntax tokens reuse the family's own hues so a highlighted code
            // block stays inside the same color world as the prose around it:
            // keywords take the lavender, literals the gold leaf, comments the
            // fog, numbers the celadon, and type names the ultramarine banner.
            syntax_kw: Rgb::new(169, 139, 216), // 梦・紫 lavender (cool: structure)
            syntax_str: Rgb::new(232, 180, 74), // 金箔 gold (warm: literal)
            syntax_comment: Rgb::new(138, 147, 163), // 雾 fog (dim: asides)
            syntax_num: Rgb::new(111, 191, 168), // 青磁 celadon (cool: literal)
            syntax_type: Rgb::new(85, 136, 238), // 蓝旗 ultramarine (cool: structure)
        }
    }
}

/// The built-in palette families a `theme.toml` `[theme] palette` key can
/// select. The chosen family is only the *base*: per-role `#RRGGBB` overrides
/// still win over it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Palette {
    /// The Kurosawa-derived default family ([`Theme::default`]).
    #[default]
    Kurosawa,
    /// The Dracula official family ([`Theme::dracula`]).
    Dracula,
}

impl Palette {
    /// Parse a `palette` config value, warning and yielding `None` (the loader
    /// then falls back to Kurosawa) on a wrong type or unknown name so a typo
    /// can never block startup.
    fn from_config(text: &str, source: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "kurosawa" => Some(Self::Kurosawa),
            "dracula" => Some(Self::Dracula),
            _ => {
                warn!(
                    file = %source,
                    value = %text,
                    "unknown theme palette; falling back to kurosawa"
                );
                None
            }
        }
    }
}

impl Theme {
    /// The Dracula built-in family (official hex values). The role mapping
    /// keeps the same warm/cool discipline as the Kurosawa family: structure
    /// roles take the cool spectrum (heading purple, link cyan, accent the one
    /// loud pink event), prose-dimming roles share the comment blue-gray
    /// (muted, quote), literal/inline roles take the warm spectrum (strong
    /// yellow, emphasis orange, inline_code + success green), and error keeps
    /// the alarm red.
    fn dracula() -> Self {
        Self {
            heading: Rgb::new(0xbd, 0x93, 0xf9),        // purple
            emphasis: Rgb::new(0xff, 0xb8, 0x6c),       // orange
            strong: Rgb::new(0xf1, 0xfa, 0x8c),         // yellow
            inline_code: Rgb::new(0x50, 0xfa, 0x7b),    // green
            link: Rgb::new(0x8b, 0xe9, 0xfd),           // cyan
            quote: Rgb::new(0x62, 0x72, 0xa4),          // comment blue-gray
            accent: Rgb::new(0xff, 0x79, 0xc6),         // pink
            muted: Rgb::new(0x62, 0x72, 0xa4),          // comment blue-gray, shared with `quote`
            success: Rgb::new(0x50, 0xfa, 0x7b),        // green, shared with `inline_code`
            error: Rgb::new(0xff, 0x55, 0x55),          // red
            syntax_kw: Rgb::new(0xff, 0x79, 0xc6),      // pink
            syntax_str: Rgb::new(0xf1, 0xfa, 0x8c),     // yellow
            syntax_comment: Rgb::new(0x62, 0x72, 0xa4), // comment
            syntax_num: Rgb::new(0xbd, 0x93, 0xf9),     // purple
            syntax_type: Rgb::new(0x8b, 0xe9, 0xfd),    // cyan
        }
    }

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
    /// Fenced-code keyword token color.
    #[must_use]
    pub const fn syntax_kw(&self) -> Rgb {
        self.syntax_kw
    }
    /// Fenced-code string-literal token color.
    #[must_use]
    pub const fn syntax_str(&self) -> Rgb {
        self.syntax_str
    }
    /// Fenced-code comment token color.
    #[must_use]
    pub const fn syntax_comment(&self) -> Rgb {
        self.syntax_comment
    }
    /// Fenced-code numeric-literal token color.
    #[must_use]
    pub const fn syntax_num(&self) -> Rgb {
        self.syntax_num
    }
    /// Fenced-code type-name token color.
    #[must_use]
    pub const fn syntax_type(&self) -> Rgb {
        self.syntax_type
    }
}

/// Light-background variants of the text-bearing roles: same hue family,
/// darkened to clear a white-ish background (the built-in `muted` gold leaf
/// is ~1.9:1 on white — far under any readability floor).
const LIGHT_MUTED: Rgb = Rgb::new(0x8a, 0x6a, 0x1c); // 深金 deep gold (~5:1 on white)
const LIGHT_SUCCESS: Rgb = Rgb::new(0x3e, 0x7a, 0x1e); // 深苔 deep moss (~4.6:1)
const LIGHT_QUOTE: Rgb = Rgb::new(0x5a, 0x64, 0x74); // 深雾 deep fog (~5.5:1)

/// Whether the terminal declares a light background, via the de-facto
/// `COLORFGBG` convention (`"fg;bg"` or `"default;bg;default"`, 0-15 indexes).
/// Absent or unparseable means "unknown", which keeps the dark-tuned palette —
/// the conservative default, since forcing light adaptation on a dark terminal
/// would be the worse failure.
fn background_is_light() -> bool {
    parse_colorfgbg_light(env::var("COLORFGBG").ok().as_deref())
}

/// Pure core of [`background_is_light`] (kept separate so tests can drive it
/// without touching process-global environment variables).
fn parse_colorfgbg_light(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    // The background is the last *numeric* field: two-field form is `fg;bg`,
    // three-field form is `default;bg;default` — a trailing `default` is
    // skipped, and the first field that parses as a number (from the right)
    // is the background index.
    let Some(bg) = value
        .rsplit(';')
        .map(str::trim)
        .find_map(|field| field.parse::<u8>().ok())
    else {
        return false;
    };
    // 0-6/8 are dark ANSI indexes; 7 (light gray) and 9-15 (bright) read as a
    // light background.
    bg == 7 || (9..=15).contains(&bg)
}

/// Whether an on-disk override names any of the adapted roles, in which case
/// the adaptation stands down for the whole palette: an operator who writes a
/// theme.toml hue means it, and a half-adapted palette would be the worst of
/// both worlds.
fn light_background_overridden(cwd: &Path, home: &Path) -> bool {
    theme_file_paths(cwd, home).iter().any(|path| {
        let Ok(contents) = fs::read_to_string(path) else {
            return false;
        };
        let Ok(table) = toml::from_str::<toml::Table>(&contents) else {
            return false;
        };
        table
            .get("theme")
            .and_then(toml::Value::as_table)
            .is_some_and(|theme| {
                theme.contains_key("muted")
                    || theme.contains_key("strong")
                    || theme.contains_key("success")
                    || theme.contains_key("quote")
            })
    })
}

/// Process-wide resolved theme, populated on the first [`Theme::current`] call
/// and reused thereafter (terminal rendering happens on every frame, so the
/// files are read once, not per line). Behind an [`RwLock`] so a config watcher
/// can swap in a freshly loaded palette mid-session; readers take a cheap
/// shared lock and copy the small (`Copy`) value out.
static THEME: OnceLock<RwLock<Theme>> = OnceLock::new();

/// Bumped on every [`Theme::reload`] so a cached, theme-dependent projection
/// (the TUI's rendered transcript) can tell a stale palette from the current
/// one without diffing colors. Starts at 0; a cache keyed on the generation
/// rebuilds exactly when the palette is swapped.
static THEME_GEN: AtomicU64 = AtomicU64::new(0);

impl Theme {
    /// Build the palette by layering user then project `theme.toml` overrides
    /// (project wins per token) onto the built-in base family. The `palette`
    /// key selects that base (Kurosawa by default; the project file wins over
    /// the user file, mirroring the per-token merge); per-role `#RRGGBB`
    /// overrides always win over whichever base was selected. Fault-isolated:
    /// an unreadable/unparseable file contributes nothing, a malformed color or
    /// an unknown palette name is skipped with a warning, so a broken theme
    /// file can never abort startup.
    #[must_use]
    pub fn load(cwd: &Path, home: &Path) -> Self {
        let layers: Vec<ThemeSettings> = theme_file_paths(cwd, home)
            .iter()
            .map(|path| read_theme_file(path))
            .collect();
        let palette = layers
            .iter()
            .rev()
            .find_map(|layer| layer.palette)
            .unwrap_or_default();
        let mut theme = match palette {
            Palette::Kurosawa => Self::default(),
            Palette::Dracula => Self::dracula(),
        };
        for settings in &layers {
            settings.apply_to(&mut theme);
        }
        // Background adaptivity (roadmap C-1/P4): the built-in hues are tuned
        // for the default dark terminal; a detected light background darkens
        // the text-bearing roles so secondary lines stay readable. Applied
        // after the file overrides so an explicit theme.toml hue for a role
        // still wins over the adaptation — an operator who names a color
        // means it.
        if !light_background_overridden(cwd, home) && background_is_light() {
            theme.adapt_for_light_background();
        }
        theme
    }

    /// Darken the text-bearing roles for a light terminal background. Only the
    /// roles that carry running prose (muted/strong/success/quote) are adapted;
    /// emphasis roles (heading/error/accent/...) keep their hue — they mark
    /// structure and events, and their usage is bold enough to survive either
    /// background.
    fn adapt_for_light_background(&mut self) {
        self.muted = LIGHT_MUTED;
        self.strong = LIGHT_MUTED;
        self.success = LIGHT_SUCCESS;
        self.quote = LIGHT_QUOTE;
    }

    /// The active palette for this process: [`Theme::load`] of the on-disk
    /// overrides, resolved once and cached. Returns by value (`Theme` is `Copy`,
    /// ~30 bytes) so hot render paths read a stable snapshot and a mid-session
    /// reload can swap the palette underneath them without a restart. A poisoned
    /// lock (impossible here — `Theme` has no `Drop` and nothing panics while it
    /// is held) is recovered rather than propagated, so rendering never aborts.
    #[must_use]
    pub fn current() -> Theme {
        let lock = THEME.get_or_init(|| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            RwLock::new(Theme::load(&cwd, &crate::home_dir()))
        });
        *lock.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Re-read the on-disk overrides and swap them into the process-wide
    /// palette, so a live `theme.toml` edit takes effect at the next turn
    /// without a restart. The blocking REPL's per-turn renderer and the TUI's
    /// per-frame [`Theme::current`] both pick up the new value; poison-safe for
    /// the same reason as `current`.
    pub fn reload(cwd: &Path, home: &Path) {
        let next = Theme::load(cwd, home);
        let lock = THEME.get_or_init(|| RwLock::new(next));
        *lock.write().unwrap_or_else(PoisonError::into_inner) = next;
        THEME_GEN.fetch_add(1, Ordering::Relaxed);
    }

    /// A monotonic counter that advances on every [`Theme::reload`]. Cache keys
    /// fold it in so a palette swap invalidates theme-dependent projections.
    #[must_use]
    pub fn generation() -> u64 {
        THEME_GEN.load(Ordering::Relaxed)
    }

    /// Serialize the effective palette as a `theme.toml` document (every role,
    /// as `#rrggbb`), the inverse of [`Theme::load`]. Powers `hf config export
    /// theme` so a user gets a fully-specified, editable starting point rather
    /// than an empty file. Every color role is restated, which pins the export
    /// to the exact hues it came from regardless of the base family it used.
    #[must_use]
    pub fn to_toml_string(self) -> String {
        let mut out = String::from("[theme]\n");
        for (key, rgb) in [
            ("heading", self.heading),
            ("emphasis", self.emphasis),
            ("strong", self.strong),
            ("inline_code", self.inline_code),
            ("link", self.link),
            ("quote", self.quote),
            ("accent", self.accent),
            ("muted", self.muted),
            ("success", self.success),
            ("error", self.error),
            ("syntax_kw", self.syntax_kw),
            ("syntax_str", self.syntax_str),
            ("syntax_comment", self.syntax_comment),
            ("syntax_num", self.syntax_num),
            ("syntax_type", self.syntax_type),
        ] {
            out.push_str(&format!("{key} = \"{}\"\n", rgb.to_hex()));
        }
        out
    }
}

/// Raw `[theme]` overrides: an optional palette selector plus one optional
/// `#RRGGBB` per semantic role. Every field is optional so a partial file (or
/// older schema with unknown keys) loads unchanged; a field that fails to parse
/// is dropped in isolation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ThemeSettings {
    palette: Option<Palette>,
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
    syntax_kw: Option<Rgb>,
    syntax_str: Option<Rgb>,
    syntax_comment: Option<Rgb>,
    syntax_num: Option<Rgb>,
    syntax_type: Option<Rgb>,
}

impl ThemeSettings {
    fn from_table(table: &toml::Table, source: &str) -> Self {
        let Some(theme) = table.get("theme").and_then(toml::Value::as_table) else {
            return Self::default();
        };
        for key in theme.keys() {
            if !matches!(
                key.as_str(),
                "palette"
                    | "heading"
                    | "emphasis"
                    | "strong"
                    | "inline_code"
                    | "link"
                    | "quote"
                    | "accent"
                    | "muted"
                    | "success"
                    | "error"
                    | "syntax_kw"
                    | "syntax_str"
                    | "syntax_comment"
                    | "syntax_num"
                    | "syntax_type"
            ) {
                tracing::debug!(file = %source, field = %key, "unknown theme token; ignored");
            }
        }
        Self {
            palette: palette_field(theme, source),
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
            syntax_kw: rgb_field(theme, "syntax_kw", source),
            syntax_str: rgb_field(theme, "syntax_str", source),
            syntax_comment: rgb_field(theme, "syntax_comment", source),
            syntax_num: rgb_field(theme, "syntax_num", source),
            syntax_type: rgb_field(theme, "syntax_type", source),
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
            syntax_kw,
            syntax_str,
            syntax_comment,
            syntax_num,
            syntax_type,
            ..
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
            (&mut theme.syntax_kw, syntax_kw),
            (&mut theme.syntax_str, syntax_str),
            (&mut theme.syntax_comment, syntax_comment),
            (&mut theme.syntax_num, syntax_num),
            (&mut theme.syntax_type, syntax_type),
        ] {
            if let Some(rgb) = value {
                *slot = rgb;
            }
        }
    }
}

/// Theme file merge order mirrors `config.toml`: user then project (project wins).
pub(crate) fn theme_file_paths(cwd: &Path, home: &Path) -> Vec<PathBuf> {
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

/// Read the `[theme] palette` selector. A wrong type or an unknown name warns
/// and yields `None` — the loader then keeps the Kurosawa base, never blocking
/// startup on a typo.
fn palette_field(table: &toml::Table, source: &str) -> Option<Palette> {
    let value = table.get("palette")?;
    let Some(text) = value.as_str() else {
        warn!(file = %source, field = "palette", "theme palette must be a string; ignored");
        return None;
    };
    Palette::from_config(text, source)
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
    /// Table grid, light box drawing: corners, tees, cross and the two edges.
    /// Kept here so the grid is one typographic decision alongside the code
    /// fence and rule marks above.
    pub const TBL_TL: &str = "┌";
    pub const TBL_TR: &str = "┐";
    pub const TBL_BL: &str = "└";
    pub const TBL_BR: &str = "┘";
    pub const TBL_ML: &str = "├";
    pub const TBL_MR: &str = "┤";
    pub const TBL_TOP_T: &str = "┬";
    pub const TBL_BOT_T: &str = "┴";
    pub const TBL_CROSS: &str = "┼";
    pub const TBL_H: &str = "─";
    pub const TBL_V: &str = "│";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn colorfgbg_detects_light_backgrounds_only_by_declared_value() {
        // Absent/unparseable -> unknown -> keeps the dark-tuned palette.
        assert!(!parse_colorfgbg_light(None));
        assert!(!parse_colorfgbg_light(Some("")));
        assert!(!parse_colorfgbg_light(Some("garbage")));
        // Dark backgrounds: 0-6 and 8.
        assert!(!parse_colorfgbg_light(Some("15;0")));
        assert!(!parse_colorfgbg_light(Some("0;8")));
        // Light backgrounds: 7 and the bright range 9-15, in both the two-field
        // and three-field forms.
        assert!(parse_colorfgbg_light(Some("0;15")));
        assert!(parse_colorfgbg_light(Some("0;7")));
        assert!(parse_colorfgbg_light(Some("default;7;default")));
        assert!(parse_colorfgbg_light(Some("15;9")));
    }

    #[test]
    fn light_adaptation_darkens_text_roles_and_leaves_emphasis_alone() {
        let mut theme = Theme::default();
        let before = theme;
        theme.adapt_for_light_background();
        // Text-bearing roles swap to their deep variants.
        assert_eq!(theme.muted(), LIGHT_MUTED);
        assert_eq!(theme.strong(), LIGHT_MUTED);
        assert_eq!(theme.success(), LIGHT_SUCCESS);
        assert_eq!(theme.quote(), LIGHT_QUOTE);
        // Emphasis roles keep their hue (usage is bold enough on either side).
        assert_eq!(theme.heading(), before.heading());
        assert_eq!(theme.error(), before.error());
        assert_eq!(theme.accent(), before.accent());
    }

    #[test]
    fn explicit_theme_overrides_stand_down_the_light_adaptation() {
        // A theme.toml that names any adapted role wins over adaptation: the
        // operator named a hue, they get that hue verbatim.
        let project = TempDir::new("light-override");
        project.write_theme("[theme]\nmuted = \"#e8b44a\"\n");
        let home = TempDir::new("light-override-home");
        // load() consults the environment, so this asserts the override path
        // only; the env-dependent branch is covered by the pure parser test.
        let theme = Theme::load(project.path(), home.path());
        assert_eq!(theme.muted(), Rgb::new(0xe8, 0xb4, 0x4a));
        assert_eq!(theme.quote(), Theme::default().quote());
    }

    #[test]
    fn dracula_palette_is_selected_from_theme_toml() {
        let project = TempDir::new("dracula");
        project.write_theme("[theme]\npalette = \"dracula\"\n");
        let home = TempDir::new("dracula-home");
        let theme = Theme::load(project.path(), home.path());
        // Accent is outside the light-adaptation set, so this holds regardless
        // of the test environment's COLORFGBG.
        assert_eq!(theme.accent(), Rgb::new(0xff, 0x79, 0xc6));
        assert_eq!(theme.heading(), Rgb::new(0xbd, 0x93, 0xf9));
        assert_eq!(theme.syntax_kw(), Rgb::new(0xff, 0x79, 0xc6));
        assert_eq!(theme.syntax_str(), Rgb::new(0xf1, 0xfa, 0x8c));
        assert_eq!(theme.syntax_comment(), Rgb::new(0x62, 0x72, 0xa4));
        assert_eq!(theme.syntax_num(), Rgb::new(0xbd, 0x93, 0xf9));
        assert_eq!(theme.syntax_type(), Rgb::new(0x8b, 0xe9, 0xfd));
    }

    #[test]
    fn dracula_palette_name_is_case_and_space_tolerant() {
        let project = TempDir::new("dracula-tolerant");
        project.write_theme("[theme]\npalette = \"  Dracula  \"\n");
        let home = TempDir::new("dracula-tolerant-home");
        assert_eq!(
            Theme::load(project.path(), home.path()).accent(),
            Rgb::new(0xff, 0x79, 0xc6)
        );
    }

    #[test]
    fn unknown_palette_name_falls_back_to_kurosawa() {
        let project = TempDir::new("bad-palette");
        project.write_theme("[theme]\npalette = \"monokai\"\n");
        let home = TempDir::new("bad-palette-home");
        // Warned, then degraded to the built-in default rather than aborting.
        assert_eq!(Theme::load(project.path(), home.path()), Theme::default());
    }

    #[test]
    fn non_string_palette_value_is_ignored() {
        let project = TempDir::new("pal-nonstring");
        project.write_theme("[theme]\npalette = 3\n");
        let home = TempDir::new("pal-nonstring-home");
        assert_eq!(Theme::load(project.path(), home.path()), Theme::default());
    }

    #[test]
    fn project_palette_wins_over_user_palette() {
        let home = TempDir::new("pal-user");
        home.write_theme("[theme]\npalette = \"dracula\"\n");
        let project = TempDir::new("pal-proj");
        project.write_theme("[theme]\npalette = \"kurosawa\"\n");
        let theme = Theme::load(project.path(), home.path());
        assert_eq!(theme.accent(), Theme::default().accent());
        // The reverse: project selects dracula over the user's kurosawa.
        let home = TempDir::new("pal-user-d");
        home.write_theme("[theme]\npalette = \"kurosawa\"\n");
        let project = TempDir::new("pal-proj-d");
        project.write_theme("[theme]\npalette = \"dracula\"\n");
        assert_eq!(
            Theme::load(project.path(), home.path()).accent(),
            Rgb::new(0xff, 0x79, 0xc6)
        );
    }

    #[test]
    fn per_role_override_beats_the_selected_palette() {
        let project = TempDir::new("pal-override");
        project.write_theme("[theme]\npalette = \"dracula\"\naccent = \"#123456\"\n");
        let home = TempDir::new("pal-override-home");
        let theme = Theme::load(project.path(), home.path());
        // The named role wins over the dracula base…
        assert_eq!(theme.accent(), Rgb::new(0x12, 0x34, 0x56));
        // …while untouched roles keep the selected family.
        assert_eq!(theme.heading(), Rgb::new(0xbd, 0x93, 0xf9));
    }

    #[test]
    fn syntax_roles_are_distinct_within_each_palette() {
        for theme in [Theme::default(), Theme::dracula()] {
            let roles = [
                theme.syntax_kw(),
                theme.syntax_str(),
                theme.syntax_comment(),
                theme.syntax_num(),
                theme.syntax_type(),
            ];
            let mut unique = roles.to_vec();
            unique.dedup();
            assert_eq!(
                unique.len(),
                roles.len(),
                "syntax tokens must not share a hue"
            );
        }
    }

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

    #[test]
    fn hex_export_is_the_inverse_of_parse_rgb() {
        let rgb = Rgb::new(0x0a, 0xbc, 0x1f);
        assert_eq!(rgb.to_hex(), "#0abc1f");
        assert_eq!(parse_rgb(&rgb.to_hex()), Some(rgb));
    }

    #[test]
    fn exported_theme_round_trips_through_the_loader() {
        // The export must re-load to the exact palette it came from, so
        // `config export theme` yields a faithful, editable template.
        let theme = Theme::default();
        let exported = theme.to_toml_string();
        let table = toml::from_str::<toml::Table>(&exported).expect("export emits valid TOML");
        let mut reloaded = Theme::default();
        ThemeSettings::from_table(&table, "export").apply_to(&mut reloaded);
        assert_eq!(reloaded, theme);
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
