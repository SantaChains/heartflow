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
/// Hues follow the "Tokyo Night" family: a cohesive cool set (soft blue accent,
/// teal headings, muted lavender/amber emphasis) chosen for low eye strain on a
/// dark terminal while keeping every role visually distinct. Body text is left
/// uncolored so it inherits the user's own terminal foreground.
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
        // Tokyo Night derived (RGB). Kept as literals so both backends receive
        // byte-identical hues; see the module note on why one palette matters.
        Self {
            heading: Rgb::new(42, 195, 222),      // cyan
            emphasis: Rgb::new(187, 154, 247),    // lavender
            strong: Rgb::new(224, 175, 104),      // amber
            inline_code: Rgb::new(158, 206, 106), // green
            link: Rgb::new(122, 162, 247),        // blue
            quote: Rgb::new(86, 95, 137),         // slate
            accent: Rgb::new(122, 162, 247),      // blue (prompt, spinner, focus)
            muted: Rgb::new(120, 130, 160),       // dimmed slate (deltas, secondary)
            success: Rgb::new(158, 206, 106),     // green
            error: Rgb::new(247, 118, 142),       // rose
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
    /// Active (in-progress) spinner state.
    pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
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
}
