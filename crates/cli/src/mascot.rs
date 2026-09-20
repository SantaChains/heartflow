//! The REPL companion: a tiny terminal "machine" figure rendered from pure
//! geometry, with no image assets and no third-party animation framework.
//!
//! This is a from-scratch Rust port of the *technique* studied in
//! `archive/grok-icon-study` (a spring-physics + polygon-eye character
//! animation), deliberately reproducing none of its data: the motion is a
//! standard critically-damped spring ([`Spring`]) — the same integrator that
//! gives the JS original its settle — and every "pixel" here is an original
//! geometric glyph chosen to read well on any UTF-8 terminal. That keeps the
//! mascot free of xAI's trademark/copyright, near-zero CPU (frames are computed
//! only when the clock or mood actually changes), and consistent with the
//! project's no-emoji / box-drawing visual language (see [`crate::theme`]).
//!
//! Two renderers share one [`Mascot`] state machine:
//!   * [`Mascot::render`] draws the full 7-wide rounded blob (breathing body +
//!     blinking eyes) as plain lines for the startup banner and the future
//!     ratatui status bar.
//!   * [`Mascot::badge`] draws a one-line eye pair that fits the fixed 2-row
//!     inline input viewport, where a 5-6 row body would not.
//!
//! The figure is *interactive* in the sense the agent can drive it: the REPL
//! sets a [`Mood`] at each phase (idle / thinking / running a tool / done /
//! error) and the mascot's eyes, mouth, blink, and breathing react.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// A 1-D critically-dampable spring integrated with semi-implicit Euler.
///
/// `k` is stiffness, `c` damping, `m` mass. With `c = 2*sqrt(k*m)` the system is
/// critically damped (settles without oscillation); a smaller `c` gives a
/// springy overshoot (used for the little "pop" when the mood flips to Done).
/// Stable for the fixed timestep the mascot uses (`dt` around 0.1s).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    pub value: f64,
    pub velocity: f64,
    pub target: f64,
    pub k: f64,
    pub c: f64,
    pub m: f64,
}

impl Spring {
    /// A spring resting at `value` with a gentle critically-damped response.
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self {
            value,
            velocity: 0.0,
            target: value,
            k: 180.0,
            c: 26.0,
            m: 1.0,
        }
    }

    /// A bouncier response (under-damped) for the Done "pop".
    #[must_use]
    pub fn bouncy(value: f64) -> Self {
        Self {
            k: 320.0,
            c: 18.0,
            ..Self::new(value)
        }
    }

    /// Advance the simulation by `dt` seconds toward `target`.
    pub fn step(&mut self, dt: f64) {
        let force = -self.k * (self.value - self.target) - self.c * self.velocity;
        let accel = force / self.m;
        self.velocity += accel * dt;
        self.value += self.velocity * dt;
    }

    /// Snapshot the current value (for tests / renderers).
    #[must_use]
    pub fn value(&self) -> f64 {
        self.value
    }
}

/// The agent phase the mascot is reacting to.
// The mascot is a complete, unit-tested widget whose full mood surface is
// provided ahead of its host. Only `Idle` (the live input editor) and `Busy`
// (the tool-run status line) are wired into today's REPL; `Thinking`/`Done`/
// `Error`, the multi-row colored render, and the getters land with the ratatui
// status bar (P4-c.3). `allow(dead_code)` marks that intentional forward API in
// this binary crate — it is exercised by the module tests, not abandoned.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    /// Waiting at the prompt: slow breathing + occasional blink.
    Idle,
    /// Streaming a response / reasoning: wide, focused eyes.
    Thinking,
    /// Running a tool: scanning eyes.
    Busy,
    /// Turn finished cleanly: happy closed arcs.
    Done,
    /// Turn failed: distressed X eyes.
    Error,
}

impl Mood {
    /// Open (unblinking) eye glyphs, left then right.
    const fn eyes(self) -> &'static str {
        match self {
            Self::Idle => "○ ○",
            Self::Thinking => "◉ ◉",
            Self::Busy => "◐ ◑",
            Self::Done => "◡ ◡",
            Self::Error => "× ×",
        }
    }

    /// Mouth glyph.
    const fn mouth(self) -> &'static str {
        match self {
            Self::Idle | Self::Done => "‿",
            Self::Thinking => "·",
            Self::Busy => "▫",
            Self::Error => "﹍",
        }
    }

    /// Whether this calm mood blinks (busy/thinking hold their eyes open).
    const fn blinks(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// The single-width eye pair drawn during a blink (eyes shut).
const BLINK_EYES: &str = "─ ─";

/// Width (display cells) of [`Mascot::render`] body rows.
#[allow(dead_code)] // asserted by tests; consumed by the future status bar
pub const BODY_WIDTH: usize = 7;

/// Time-based state for the companion. Owns the blink cadence, the eye-lid
/// spring, and a slow breathing phase; the REPL drives it by calling
/// [`Mascot::advance`] on each idle tick and [`Mascot::set_mood`] at phase
/// changes.
#[derive(Debug, Clone)]
pub struct Mascot {
    mood: Mood,
    elapsed: f64,
    lid: Spring,
    /// Wall-clock seconds when the current blink began, or `None` when awake.
    blink_at: Option<f64>,
    /// Next time a blink should start (jittered so it never looks metronomic).
    next_blink: f64,
    /// Deterministic LCG for blink jitter (fixed seed => reproducible frames).
    /// `u32` so the float conversion below is exact (no precision loss).
    rng: u32,
}

impl Default for Mascot {
    fn default() -> Self {
        Self::new()
    }
}

impl Mascot {
    /// A freshly-born idle mascot at time zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mood: Mood::Idle,
            elapsed: 0.0,
            lid: Spring::new(1.0),
            blink_at: None,
            next_blink: 3.0,
            rng: 0x9E37_79B9,
        }
    }

    /// The mood currently shown.
    #[allow(dead_code)] // status-bar host (P4-c.3) will query this
    #[must_use]
    pub fn mood(&self) -> Mood {
        self.mood
    }

    /// Transition to a new mood. A flip to [`Mood::Done`] gives the lid a small
    /// springy pop so the change reads as motion, not a teleport.
    #[allow(dead_code)] // the persistent widget's driver; see [`Mood`] note
    pub fn set_mood(&mut self, mood: Mood) {
        if mood == self.mood {
            return;
        }
        self.mood = mood;
        if mood == Mood::Done {
            self.lid = Spring::bouncy(1.0);
            self.lid.target = 1.0;
        } else if mood != Mood::Idle {
            // Non-idle moods hold eyes open.
            self.blink_at = None;
            self.lid.target = 1.0;
        }
    }

    /// Advance elapsed time by `dt` seconds, driving breathing, blink, and the
    /// lid spring. Cheap; call once per idle tick.
    pub fn advance(&mut self, dt: f64) {
        self.elapsed += dt;
        // Blink scheduling (idle only).
        if self.mood.blinks() {
            if self.blink_at.is_none() && self.elapsed >= self.next_blink {
                self.blink_at = Some(self.elapsed);
                self.lid.target = 0.0;
            }
            if let Some(started) = self.blink_at {
                // Lid closes via spring; hold briefly, then reopen and reschedule.
                let since = self.elapsed - started;
                if self.lid.value() < 0.15 && since > 0.08 {
                    self.lid.target = 1.0;
                }
                if self.lid.value() > 0.9 && since > 0.12 {
                    self.blink_at = None;
                    self.next_blink = self.elapsed + self.jitter();
                }
            }
        }
        self.lid.step(dt);
    }

    /// Jittered gap to the next blink: 2.2..4.0s, deterministic per instance.
    fn jitter(&mut self) -> f64 {
        // Numerical-Recipes LCG on `u32`; the u32 -> f64 map is exact, so the
        // fraction carries no precision loss.
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1);
        let unit = f64::from(self.rng) / f64::from(u32::MAX);
        2.2 + unit * 1.8
    }

    /// Whether the eyes are currently shut (blink in progress).
    #[must_use]
    fn eyes_closed(&self) -> bool {
        self.mood.blinks() && self.lid.value() < 0.5
    }

    /// Slow breathing toggle (0 or 1 extra body row) from a 4s sine phase.
    #[must_use]
    fn breath(&self) -> usize {
        // One extra mid row half the time -> a gentle height pulse.
        usize::from((self.elapsed / 4.0).sin() > 0.0)
    }

    /// Render the full blob as plain single-width lines. The outer box is fixed;
    /// breathing adds/removes one interior row and a blink swaps the eye pair,
    /// so the figure visibly idles.
    #[must_use]
    pub fn render(&self) -> Vec<String> {
        let eyes = if self.eyes_closed() {
            BLINK_EYES
        } else {
            self.mood.eyes()
        };
        let mut rows = vec![
            "╭─────╮".to_string(),
            format!("│ {eyes} │"),
            "│     │".to_string(),
        ];
        // Breathing inserts an extra belly row at its peak.
        for _ in 0..self.breath() {
            rows.push("│     │".to_string());
        }
        rows.push(format!("│  {}  │", self.mood.mouth()));
        rows.push("╰─────╯".to_string());
        rows
    }

    /// Render the full blob as colored ratatui lines (body muted, eyes accent,
    /// done/error eyes take the success/error hue). For the future status bar.
    #[allow(dead_code)] // consumed by the ratatui status bar (P4-c.3)
    #[must_use]
    pub fn render_ratatui<'a>(&'a self, theme: &'a Theme) -> Vec<Line<'a>> {
        let eye_color = match self.mood {
            Mood::Done => theme.success(),
            Mood::Error => theme.error(),
            _ => theme.accent(),
        };
        let body = theme.muted().ratatui();
        let eye_color = eye_color.ratatui();
        self.render()
            .into_iter()
            .map(|row| {
                // Split the eye row so only the pupils are accent-colored.
                let eyes = if self.eyes_closed() {
                    BLINK_EYES
                } else {
                    self.mood.eyes()
                };
                if row.starts_with("│ ") && row.contains(eyes) {
                    let (head, tail) = row.split_once(eyes).unwrap_or(("", ""));
                    Line::from(vec![
                        Span::styled(head.to_string(), Style::default().fg(body)),
                        Span::styled(
                            eyes.to_string(),
                            Style::default().fg(eye_color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(tail.to_string(), Style::default().fg(body)),
                    ])
                } else {
                    Line::from(Span::styled(row, Style::default().fg(body)))
                }
            })
            .collect()
    }

    /// One-line eye pair (width 3) for the inline input viewport, where the full
    /// body would not fit. Blinks in idle just like the full render.
    #[must_use]
    pub fn badge(&self) -> String {
        if self.eyes_closed() {
            BLINK_EYES.to_string()
        } else {
            self.mood.eyes().to_string()
        }
    }

    /// A deterministic busy "scanning" face for a given frame index (used by the
    /// tool-run status line, which advances one frame per event, no clock).
    #[must_use]
    pub fn busy_face(frame: usize) -> &'static str {
        // Alternate the half-eye orientation so it reads as left-right scanning.
        if frame.is_multiple_of(2) {
            "◐ ◑"
        } else {
            "◑ ◐"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_converges_to_target_critically_damped() {
        let mut s = Spring::new(1.0);
        s.target = 0.0;
        // Integrate ~1s at the mascot's tick; it should settle near 0 without
        // ringing far past the target (critically damped).
        let mut min: f64 = 1.0;
        for _ in 0..100 {
            s.step(0.01);
            min = min.min(s.value);
        }
        assert!(
            s.value.abs() < 0.05,
            "spring should settle near target: {}",
            s.value
        );
        assert!(
            min > -0.2,
            "critical damping should not overshoot far: {min}"
        );
    }

    #[test]
    fn bouncy_spring_overshoots_below_target() {
        let mut s = Spring::bouncy(1.0);
        s.target = 0.0;
        let mut went_negative = false;
        for _ in 0..200 {
            s.step(0.01);
            if s.value < 0.0 {
                went_negative = true;
            }
        }
        assert!(went_negative, "bouncy spring should overshoot past target");
    }

    #[test]
    fn render_is_box_lined_and_width_stable() {
        let mut m = Mascot::new();
        // Freeze breathing at both phases so width invariance is checked fully.
        m.set_mood(Mood::Thinking);
        let rows = m.render();
        assert!(rows.first().is_some_and(|r| r == "╭─────╮"));
        assert!(rows.last().is_some_and(|r| r == "╰─────╯"));
        // Every row is the same display width (single-width glyphs only).
        let width = rows[0].chars().count();
        assert_eq!(width, BODY_WIDTH, "row width: {rows:?}");
        for row in &rows {
            assert_eq!(row.chars().count(), width, "ragged row: {row}");
        }
    }

    #[test]
    fn idle_blinks_over_time_and_recovers() {
        let mut m = Mascot::new(); // Idle
        let mut saw_closed = false;
        let mut saw_open_again = false;
        for _ in 0..600 {
            m.advance(0.1);
            if m.eyes_closed() {
                saw_closed = true;
            } else if saw_closed {
                saw_open_again = true;
            }
        }
        assert!(saw_closed, "idle mascot should blink at least once in 60s");
        assert!(saw_open_again, "eyes should reopen after a blink");
    }

    #[test]
    fn busy_mood_holds_eyes_open_no_blink() {
        let mut m = Mascot::new();
        m.set_mood(Mood::Busy);
        for _ in 0..600 {
            m.advance(0.1);
            assert!(!m.eyes_closed(), "busy eyes must not blink");
        }
    }

    #[test]
    fn mood_changes_eyes_and_mouth() {
        let mut m = Mascot::new();
        m.set_mood(Mood::Error);
        let rendered = m.render().concat();
        assert!(rendered.contains("× ×"), "error shows X eyes: {rendered}");
        m.set_mood(Mood::Thinking);
        assert!(m.render().concat().contains("◉ ◉"));
    }

    #[test]
    fn badge_is_three_cells_and_tracks_blink() {
        let m = Mascot::new();
        assert_eq!(m.badge().chars().count(), 3, "badge width");
        assert_eq!(m.badge(), "○ ○", "idle badge is open eyes");
    }

    #[test]
    fn busy_face_alternates_for_scanning() {
        assert_ne!(Mascot::busy_face(0), Mascot::busy_face(1));
        assert_eq!(Mascot::busy_face(0), Mascot::busy_face(2));
    }

    #[test]
    fn ratatui_render_preserves_row_count() {
        let theme = Theme::default();
        let m = Mascot::new();
        let lines = m.render_ratatui(&theme);
        // render() is always >= 5 rows, so equality also proves non-emptiness.
        assert_eq!(lines.len(), m.render().len());
        assert!(lines.len() >= 5, "expected a full blob: {lines:?}");
    }
}
