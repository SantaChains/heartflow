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
//! Two renderers share one [`Mascot`] state machine, both driven by the spring:
//!   * [`Mascot::render`] plots a **braille dot canvas** (2×4 sub-pixels per
//!     cell, the same sub-cell technique `obsidian-tui` and ratatui's `Canvas`
//!     use) so the blob reads as a genuinely round, organic head with
//!     spring-animated breathing, a bob, blinking eyes, and mood features —
//!     for the startup banner and the future ratatui status bar.
//!   * [`Mascot::badge`] draws a one-line eye pair that fits the fixed 2-row
//!     inline input viewport, where a 5-row body would not.
//!
//! The figure is *interactive* in the sense the agent can drive it: the REPL
//! sets a [`Mood`] at each phase (idle / thinking / running a tool / done /
//! error) and the mascot's eyes, mouth, blink, and breathing react.

use ratatui::style::Style;
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
    /// The single-width eye pair the inline [`Mascot::badge`] shows when this
    /// mood's eyes are open (the braille body draws richer per-mood shapes).
    const fn eyes(self) -> &'static str {
        match self {
            Self::Idle => "○ ○",
            Self::Thinking => "◉ ◉",
            Self::Busy => "◐ ◑",
            Self::Done => "◡ ◡",
            Self::Error => "× ×",
        }
    }

    /// Whether this calm mood blinks (busy/thinking hold their eyes open).
    const fn blinks(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// The single-width eye pair drawn during a blink (eyes shut).
const BLINK_EYES: &str = "─ ─";

/// Dot-matrix canvas size (in sub-pixels). 9 cells wide x 5 tall, comparable to
/// the status-bar badge footprint.
const DOT_WIDTH: usize = 18;
const DOT_HEIGHT: usize = 20;
/// Braille sub-pixels per cell, horizontally and vertically.
const CELL_COLS: usize = 2;
const CELL_ROWS: usize = 4;

/// A pixel buffer that paints sub-pixels at float coordinates and encodes them
/// as braille cells. Mirrors `TermAVG`'s *pixel-buffer -> terminal-cell encoding*
/// split (see `tmj_core/src/img/halfblock.rs`): the dot matrix uses it for
/// smooth curves; half-block truecolor compositing is deferred to the full-screen
/// refactor (P4).
struct Braille {
    w: usize,
    h: usize,
    dots: Vec<bool>,
}

impl Braille {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            dots: vec![false; w * h],
        }
    }

    /// Set a dot by integer coordinate (out-of-range writes are ignored).
    fn set(&mut self, x: usize, y: usize, on: bool) {
        if x < self.w && y < self.h {
            self.dots[y * self.w + x] = on;
        }
    }

    /// Set a dot by float coordinate (rounds to the nearest dot).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn set_f(&mut self, fx: f64, fy: f64, on: bool) {
        if fx < 0.0 || fy < 0.0 {
            return;
        }
        self.set(fx.round() as usize, fy.round() as usize, on);
    }

    /// Encode to per-row braille strings; an all-dark cell emits a space.
    fn render(&self) -> Vec<String> {
        let cols = self.w.div_ceil(CELL_COLS);
        let rows = self.h.div_ceil(CELL_ROWS);
        (0..rows)
            .map(|cy| {
                (0..cols)
                    .map(|cx| {
                        let mut bits = 0u16;
                        for dy in 0..CELL_ROWS {
                            for dx in 0..CELL_COLS {
                                let x = cx * CELL_COLS + dx;
                                let y = cy * CELL_ROWS + dy;
                                if x < self.w && y < self.h && self.dots[y * self.w + x] {
                                    bits |= dot_bit(dx, dy);
                                }
                            }
                        }
                        if bits == 0 {
                            ' '
                        } else {
                            char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

/// Braille bit map: column 0 is 0x01,0x02,0x04,0x40 (rows 0..3); column 1 is
/// 0x08,0x10,0x20,0x80.
const fn dot_bit(dx: usize, dy: usize) -> u16 {
    match (dx, dy) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

/// Width (display cells) of [`Mascot::render`]'s braille blob.
#[allow(dead_code)] // blob geometry constants; consumed by the status bar (P4-c.3)
pub const RENDER_WIDTH: usize = DOT_WIDTH / CELL_COLS;
/// Height (display cells) of [`Mascot::render`]'s braille blob.
#[allow(dead_code)] // see [`RENDER_WIDTH`]
pub const RENDER_HEIGHT: usize = DOT_HEIGHT / CELL_ROWS;

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

    /// Render the blob as braille dot rows. The head is a spring-squashed
    /// ellipse ring that breathes and bobs; the eyes blink (lid spring) and take
    /// a per-mood shape; the mouth smiles/frowns. This replaces the old box-char
    /// body with genuine sub-cell curvature.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )] // float art -> dot grid
    pub fn render(&self) -> Vec<String> {
        let mut cv = Braille::new(DOT_WIDTH, DOT_HEIGHT);
        // Slow vertical squash (breathing) + a gentler bob, both from `elapsed`.
        let breath = (self.elapsed / 2.6 * std::f64::consts::PI).sin();
        let bob = (self.elapsed / 3.9 * std::f64::consts::PI).sin();
        let cx = (DOT_WIDTH - 1) as f64 / 2.0;
        let cy = (DOT_HEIGHT - 1) as f64 / 2.0 + bob;
        let rx = cx - 0.5;
        let ry = ((DOT_HEIGHT - 1) as f64 / 2.0 - 1.0) * (1.0 + 0.05 * breath);
        for y in 0..DOT_HEIGHT {
            for x in 0..DOT_WIDTH {
                let nx = (x as f64 - cx) / rx;
                let ny = (y as f64 - cy) / ry;
                let r = nx * nx + ny * ny;
                // A thick ring band reads as a round head outline.
                cv.set(x, y, (0.60..=1.0).contains(&r));
            }
        }
        self.plot_eyes(&mut cv, cx, cy, rx);
        self.plot_mouth(&mut cv, cx, cy);
        cv.render()
    }

    /// Plot both eyes with the mood's shape, scaled by blink openness.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn plot_eyes(&self, cv: &mut Braille, cx: f64, cy: f64, rx: f64) {
        let openness = if self.mood.blinks() {
            self.lid.value().clamp(0.0, 1.0)
        } else {
            1.0
        };
        // Pupils sit ~40% out from center, a little above the midline.
        let ex = rx * 0.42;
        let ey = cy - rx * 0.22;
        // Busy scans side to side with the clock.
        let scan = if self.mood == Mood::Busy {
            (self.elapsed * 1.6).sin() * 1.2
        } else {
            0.0
        };
        for sign in [-1.0, 1.0] {
            let ecx = cx + sign * ex + scan;
            match self.mood {
                Mood::Error => {
                    for i in 0..5 {
                        let dx = f64::from(i) - 2.0;
                        cv.set_f(ecx + dx, ey + dx, true);
                        cv.set_f(ecx + dx, ey - dx, true);
                    }
                }
                Mood::Done => {
                    // Happy up-arched eyes: a dome of dots.
                    for i in -2..=2 {
                        let dx = f64::from(i);
                        cv.set_f(ecx + dx, ey - (1.0 - (dx / 2.0).powi(2)), true);
                    }
                }
                _ if openness < 0.35 => {
                    for i in -2..=2 {
                        cv.set_f(ecx + f64::from(i), ey, true);
                    }
                }
                _ => {
                    let er = 2.3;
                    let eh = er * (0.35 + 0.65 * openness);
                    for dy in -3..=3 {
                        for dx in -3..=3 {
                            let nx = f64::from(dx) / er;
                            let ny = f64::from(dy) / eh;
                            cv.set_f(
                                ecx + f64::from(dx),
                                ey + f64::from(dy),
                                nx * nx + ny * ny <= 1.0,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Plot the mouth with the mood's expression.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    fn plot_mouth(&self, cv: &mut Braille, cx: f64, cy: f64) {
        let my = cy + (DOT_HEIGHT as f64) * 0.20;
        match self.mood {
            Mood::Idle | Mood::Thinking | Mood::Busy => {
                for i in -2..=2 {
                    let dx = f64::from(i);
                    cv.set_f(cx + dx, my + (dx / 2.0).powi(2), true); // smile
                }
            }
            Mood::Done => {
                for i in -3..=3 {
                    let dx = f64::from(i);
                    cv.set_f(cx + dx, my + (dx / 3.0).powi(2), true); // bigger smile
                }
            }
            Mood::Error => {
                for i in -2..=2 {
                    let dx = f64::from(i);
                    cv.set_f(cx + dx, my + 2.0 - (dx / 2.0).powi(2), true); // frown
                }
            }
        }
    }

    /// Render the braille blob as colored ratatui lines (mood hue on the whole
    /// figure). The status bar (P4-c.3) drops this into its right region.
    #[allow(dead_code)] // consumed by the ratatui status bar (P4-c.3)
    #[must_use]
    pub fn render_ratatui<'a>(&'a self, theme: &'a Theme) -> Vec<Line<'a>> {
        let color = match self.mood {
            Mood::Done => theme.success(),
            Mood::Error => theme.error(),
            _ => theme.accent(),
        }
        .ratatui();
        self.render()
            .into_iter()
            .map(|row| Line::from(Span::styled(row, Style::default().fg(color))))
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
    fn render_is_braille_blob_with_stable_size() {
        let mut m = Mascot::new();
        m.set_mood(Mood::Thinking);
        let rows = m.render();
        assert_eq!(rows.len(), RENDER_HEIGHT, "row count");
        for row in &rows {
            assert_eq!(row.chars().count(), RENDER_WIDTH, "ragged row: {row:?}");
            for ch in row.chars() {
                assert!(
                    ch == ' ' || ('\u{2800}'..'\u{28FF}').contains(&ch),
                    "non-braille glyph {ch:?}"
                );
            }
        }
        // A round head leaves the bounding-box corners clear.
        assert_eq!(rows[0].chars().next(), Some(' '), "top-left corner");
        assert_eq!(rows[0].chars().last(), Some(' '), "top-right corner");
        // The blob is not blank.
        assert!(
            rows.iter().any(|r| r.chars().any(|c| c != ' ')),
            "blob is blank: {rows:?}"
        );
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
    fn mood_changes_the_face() {
        let mut idle = Mascot::new();
        idle.set_mood(Mood::Idle);
        let mut err = Mascot::new();
        err.set_mood(Mood::Error);
        let mut done = Mascot::new();
        done.set_mood(Mood::Done);
        assert_ne!(idle.render(), err.render(), "error eyes differ from idle");
        assert_ne!(done.render(), err.render(), "done differs from error");
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
