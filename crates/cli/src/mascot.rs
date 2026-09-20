//! The REPL companion: a lively terminal "robot head" drawn from pure geometry,
//! with no image assets and no third-party animation framework.
//!
//! Two renderers share one [`Mascot`] state machine, both driven by the same
//! spring physics, and each uses the dot-matrix packing that suits its size:
//!   * [`Mascot::render_head_cells`] rasterises a **half-block truecolor** head
//!     (`▀`/`▄`/`█`, two full-colour pixels per cell — foreground = top pixel,
//!     background = bottom). The design language is the grok-bot school: a
//!     floating porcelain egg in a calm vertical gradient, two large ink eyes,
//!     no mouth at rest, and an orbiting spark of theme emphasis. This is the
//!     startup banner, drawn in colour via [`draw_banner`].
//!   * [`Mascot::badge`] draws a tiny **braille** face (2×4 sub-pixels per cell,
//!     the same sub-cell technique ratatui's `Canvas` uses) that fits the fixed
//!     inline input viewport, where the extra vertical resolution makes the
//!     eyes read clearly at a few cells wide.
//!
//! The figure is *interactive*: the spring integrator ([`Spring`]) gives the
//! breathing squash, the bob, the eye-lid blink, and the little pop when a mood
//! flips, all computed only when the clock advances — so idle CPU stays near
//! zero. Motion is a standard critically-damped spring; every "pixel" is an
//! original geometric glyph chosen to read on any UTF-8 truecolor terminal, and
//! the base hues come from [`Theme`] so the companion stays in the project's one
//! coordinated colour family.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use crossterm::cursor::{MoveToColumn, MoveToPreviousLine};
use crossterm::queue;
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};

use crate::theme::{Rgb, Theme};

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
// The mascot is a complete, unit-tested widget. `Idle` (typing), `Done`, and
// `Error` are wired into today's REPL through [`Mascot::note_turn`] /
// [`Mascot::resume_idle`]: the badge shows the last turn's outcome and drops
// back to `Idle` the moment the user engages again. `Thinking`/`Busy` stay
// forward API until the fullscreen status bar (P4-c.3) drives them from the
// live event stream (the in-turn spinner already covers `Busy` visually).
// `allow(dead_code)` marks that intentional forward API in this binary crate —
// every variant is exercised by the module tests, not abandoned.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    /// Waiting at the prompt: slow breathing + occasional blink.
    Idle,
    /// Streaming a response / reasoning: wide, focused eyes, small "o" mouth.
    Thinking,
    /// Running a tool: pupils scan side to side.
    Busy,
    /// Turn finished cleanly: happy up-arched eyes, big smile.
    Done,
    /// Turn failed: distressed X eyes.
    Error,
}

impl Mood {
    /// Whether this calm mood blinks (busy/thinking hold their eyes open).
    const fn blinks(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// Half-block canvas size for the banner head, in sub-pixels. Each cell is
/// 1 column x 2 rows of pixels, so the head is `HEAD_W` cells wide and
/// `HEAD_H / 2` cells tall. Because a half-block pixel is ~1.3x taller than
/// wide, the head radii below are tuned so the silhouette reads as a round
/// robot head, not a horizontally-stretched oval.
const HEAD_W: usize = 24;
const HEAD_H: usize = 20;
/// Braille canvas size for the inline badge, in sub-pixels (2 wide x 4 tall per
/// cell), so the face is `BADGE_W / 2` cells wide and `BADGE_H / 4` cells tall.
const BADGE_W: usize = 10;
const BADGE_H: usize = 8;
/// Braille sub-pixels per cell, horizontally and vertically.
const CELL_COLS: usize = 2;
const CELL_ROWS: usize = 4;

/// DEC private mode 2026 (begin/end synchronized output). Terminals that
/// support it (Windows Terminal, kitty, WezTerm, foot) buffer everything
/// between the pair and present it as one atomic repaint, which removes the
/// flicker of redrawing the banner head in place. Terminals that don't
/// recognise the mode ignore the codes, so this degrades harmlessly.
const SYNC_BEGIN: &str = "\u{1b}[?2026h";
const SYNC_END: &str = "\u{1b}[?2026l";

/// An 8-bit colour used only inside the renderer, so shading can read and write
/// channels freely. Base hues are pulled from [`Theme`] via the existing
/// [`Rgb::crossterm`] projection, keeping the companion in the same palette
/// without a second source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Col {
    r: u8,
    g: u8,
    b: u8,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
impl Col {
    const WHITE: Col = Col {
        r: 240,
        g: 245,
        b: 255,
    };
    const SOCKET: Col = Col {
        r: 14,
        g: 16,
        b: 24,
    };

    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Multiply every channel by `f` (a shade/brightness factor), clamped to the
    /// 0..=255 range.
    fn scale(self, f: f64) -> Self {
        let ch = |v: u8| (f64::from(v) * f).clamp(0.0, 255.0).round() as u8;
        Self::new(ch(self.r), ch(self.g), ch(self.b))
    }

    /// Linear blend toward `other` by `t` (0 keeps self, 1 gives `other`).
    fn mix(self, other: Self, t: f64) -> Self {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Self::new(
            lerp(self.r, other.r),
            lerp(self.g, other.g),
            lerp(self.b, other.b),
        )
    }

    fn to_crossterm(self) -> Color {
        Color::Rgb {
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }
}

/// Read a [`Theme`] role's channels back out of its crossterm projection.
fn col_from(rgb: Rgb) -> Col {
    match rgb.crossterm() {
        Color::Rgb { r, g, b } => Col { r, g, b },
        _ => Col {
            r: 150,
            g: 170,
            b: 210,
        },
    }
}

/// The mood-resolved palette the renderers share, so the head, spark, eyes,
/// and mouth all stay harmonised. The design language is deliberately
/// minimal (the grok-bot school): a porcelain shell carrying only two ink
/// eyes, with mood signalled by shell tint and the orbiting spark.
struct Palette {
    /// Porcelain body hue (mood-tinted toward white).
    shell: Col,
    /// Bottom-gradient / soft-rim shade of the shell.
    shade: Col,
    /// Near-black ink for the eyes.
    ink: Col,
    /// Orbiting spark, from the theme's emphasis hue.
    glow: Col,
}

/// One terminal cell produced by half-block encoding: a glyph plus optional
/// foreground (top pixel) and background (bottom pixel) colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    fg: Option<Col>,
    bg: Option<Col>,
}

/// A pixel buffer that paints colours at float coordinates and encodes them as
/// half-block cells (two full-colour pixels per cell). The *pixel-buffer ->
/// terminal-cell encoding* split mirrors how good agent image renderers work:
/// geometry paints pixels, encoding is a separate, testable step.
struct HalfBuf {
    w: usize,
    h: usize,
    px: Vec<Option<Col>>,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
impl HalfBuf {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![None; w * h],
        }
    }

    /// Paint a pixel by integer coordinate (out-of-range writes are ignored).
    fn set(&mut self, x: usize, y: usize, c: Col) {
        if x < self.w && y < self.h {
            self.px[y * self.w + x] = Some(c);
        }
    }

    /// Paint a pixel by signed coordinate (negatives and overflow are dropped).
    fn set_i(&mut self, x: i64, y: i64, c: Col) {
        if let (Ok(xx), Ok(yy)) = (usize::try_from(x), usize::try_from(y)) {
            self.set(xx, yy, c);
        }
    }

    /// Paint a pixel by float coordinate (rounds to the nearest pixel).
    fn set_f(&mut self, fx: f64, fy: f64, c: Col) {
        if fx < 0.0 || fy < 0.0 || !fx.is_finite() || !fy.is_finite() {
            return;
        }
        self.set_i(fx.round() as i64, fy.round() as i64, c);
    }

    /// Encode to per-row half-block cells: two stacked pixels collapse into one
    /// cell, foreground carrying the top pixel and background the bottom.
    fn encode(&self) -> Vec<Vec<Cell>> {
        let mut rows = Vec::with_capacity(self.h / 2);
        for cy in 0..self.h / 2 {
            let mut row = Vec::with_capacity(self.w);
            for cx in 0..self.w {
                let top = self.px[(2 * cy) * self.w + cx];
                let bottom = self.px[(2 * cy + 1) * self.w + cx];
                row.push(match (top, bottom) {
                    (Some(t), Some(b)) => Cell {
                        ch: '\u{2580}',
                        fg: Some(t),
                        bg: Some(b),
                    },
                    (Some(t), None) => Cell {
                        ch: '\u{2580}',
                        fg: Some(t),
                        bg: None,
                    },
                    (None, Some(b)) => Cell {
                        ch: '\u{2584}',
                        fg: Some(b),
                        bg: None,
                    },
                    (None, None) => Cell {
                        ch: ' ',
                        fg: None,
                        bg: None,
                    },
                });
            }
            rows.push(row);
        }
        rows
    }
}

/// Fill an axis-aligned ellipse of solid colour.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn fill_ellipse(buf: &mut HalfBuf, cx: f64, cy: f64, rx: f64, ry: f64, c: Col) {
    let x0 = (cx - rx).floor() as i64;
    let x1 = (cx + rx).ceil() as i64;
    let y0 = (cy - ry).floor() as i64;
    let y1 = (cy + ry).ceil() as i64;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let nx = (x as f64 - cx) / rx;
            let ny = (y as f64 - cy) / ry;
            if nx * nx + ny * ny <= 1.0 {
                buf.set_i(x, y, c);
            }
        }
    }
}

/// A pixel buffer that paints sub-pixels at float coordinates and encodes them
/// as braille cells. Backs the compact inline [`Mascot::badge`] face, where the
/// 2x4 sub-cell resolution keeps small eyes legible.
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
    /// A freshly-born idle mascot at time zero, eyes open.
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

    /// A mascot whose eyes start shut and spring open — used by [`draw_banner`]
    /// so the startup animation reads as the companion waking up.
    #[must_use]
    pub fn booting() -> Self {
        let mut mascot = Self::new();
        mascot.lid = Spring::new(0.0);
        mascot.lid.target = 1.0; // spring the eyes open over the first frames
        mascot
    }

    /// The mood currently shown.
    #[must_use]
    #[allow(dead_code)] // forward API for the status-bar host (P4-c.3)
    pub fn mood(&self) -> Mood {
        self.mood
    }

    /// Transition to a new mood. A flip to [`Mood::Done`] gives the lid a small
    /// springy pop so the change reads as motion, not a teleport.
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

    /// Record how the last turn ended so the badge reflects it until the user
    /// engages again: a clean turn pops into [`Mood::Done`] (success hue), a
    /// failed one into [`Mood::Error`]. This is the whole event->mood seam — the
    /// *appearance* of each mood lives entirely in the renderers, so restyling
    /// the character never touches this call path.
    pub fn note_turn(&mut self, ok: bool) {
        self.set_mood(if ok { Mood::Done } else { Mood::Error });
    }

    /// The user pressed a key: drop any lingering post-turn mood back to the
    /// blinking [`Mood::Idle`] baseline. Idempotent (a no-op once already idle).
    pub fn resume_idle(&mut self) {
        self.set_mood(Mood::Idle);
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

    /// The mood-resolved palette: a porcelain shell washed toward white with
    /// the mood hue, its darker shade, ink eyes, and the theme's spark hue.
    fn palette(&self, theme: &Theme) -> Palette {
        let base = col_from(match self.mood {
            Mood::Done => theme.success(),
            Mood::Error => theme.error(),
            _ => theme.accent(),
        });
        let shell = base.mix(Col::WHITE, 0.62);
        Palette {
            shade: shell.scale(0.82),
            ink: Col::SOCKET,
            glow: col_from(theme.emphasis()),
            shell,
        }
    }

    /// The theme colour the inline badge should be drawn in for this mood.
    #[must_use]
    pub fn badge_color(&self, theme: &Theme) -> Rgb {
        match self.mood {
            Mood::Done => theme.success(),
            Mood::Error => theme.error(),
            _ => theme.accent(),
        }
    }

    /// Render the banner head as half-block truecolor cells: a floating
    /// porcelain egg with two ink eyes and an orbiting spark. Flat vertical
    /// gradient only — radial shading read as banding at this size, which is
    /// what made the old lit dome look like a striped bee.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )] // float art -> pixel grid
    fn render_head_cells(&self, theme: &Theme) -> Vec<Vec<Cell>> {
        let pal = self.palette(theme);
        let breath = (self.elapsed / 2.6 * std::f64::consts::PI).sin();
        let bob = (self.elapsed / 3.3 * std::f64::consts::PI).sin();
        let mut buf = HalfBuf::new(HEAD_W, HEAD_H);
        let cx = f64::from((HEAD_W - 1) as u32) / 2.0;
        let cy = HEAD_H as f64 * 0.58 + bob * 0.7;
        // Egg ratio (rx < 1.3 * ry) and a low centre leave generous negative
        // space above the head for the orbiting spark.
        let rx = 7.6;
        let ry = 6.5 * (1.0 + 0.02 * breath);

        // Porcelain shell: a calm top-to-bottom gradient plus a barely-there
        // soft rim at the silhouette edge. Nothing else — no dome shading, no
        // specular term, no stripes.
        for y in 0..HEAD_H {
            for x in 0..HEAD_W {
                let nx = (x as f64 - cx) / rx;
                let ny = (y as f64 - cy) / ry;
                let e = nx * nx + ny * ny;
                if e > 1.0 {
                    continue;
                }
                let t = ((y as f64 - (cy - ry)) / (2.0 * ry)).clamp(0.0, 1.0);
                let mut c = pal.shell.mix(pal.shade, t * 0.8);
                if e > 0.88 {
                    c = c.mix(pal.shade, ((e - 0.88) / 0.12).clamp(0.0, 1.0) * 0.5);
                }
                buf.set(x, y, c);
            }
        }

        self.paint_spark(&mut buf, &pal, cx, cy, ry);
        self.paint_eyes(&mut buf, &pal, cx, cy);
        self.paint_mouth(&mut buf, &pal, cx, cy);
        buf.encode()
    }

    /// A spark of theme emphasis orbiting above the head: one bright core in a
    /// four-point halo, drifting on slow sines. This is the character's "alive"
    /// accent — it replaced the antenna, which read as clutter.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn paint_spark(&self, buf: &mut HalfBuf, pal: &Palette, cx: f64, cy: f64, ry: f64) {
        let t = self.elapsed;
        let sx = cx + (t / 1.9).sin() * 6.2;
        let sy = cy - ry - 2.4 + (t / 2.9).sin() * 0.9;
        buf.set_f(sx, sy, pal.glow);
        buf.set_f(sx - 1.0, sy, pal.glow.scale(0.55));
        buf.set_f(sx + 1.0, sy, pal.glow.scale(0.55));
        buf.set_f(sx, sy - 1.0, pal.glow.scale(0.55));
        buf.set_f(sx, sy + 1.0, pal.glow.scale(0.55));
    }

    /// Both eyes with the mood's shape: large ink ovals with a single glint,
    /// scaled by blink openness and nudged by look-around (idle), an up-left
    /// gaze (thinking), or a fast scan (busy).
    #[allow(clippy::cast_precision_loss)]
    fn paint_eyes(&self, buf: &mut HalfBuf, pal: &Palette, cx: f64, cy: f64) {
        let openness = if self.mood.blinks() {
            self.lid.value().clamp(0.0, 1.0)
        } else {
            1.0
        };
        let (mut dx, mut dy) = match self.mood {
            Mood::Busy => ((self.elapsed * 1.6).sin() * 1.5, 0.0),
            Mood::Thinking => (-0.7, -0.6),
            _ => ((self.elapsed * 0.5).sin() * 0.5, 0.0),
        };
        if openness < 0.3 {
            // Lids: shut eyes become short horizontal lines pressed onto the
            // shell, so a blink reads as a calm close, not a vanish.
            dx = 0.0;
            dy = 0.0;
        }
        let ecy = cy - 0.6 + dy;
        for sign in [-1.0, 1.0] {
            let ecx = cx + sign * 3.1 + dx;
            match self.mood {
                Mood::Error => {
                    for i in -2..=2 {
                        let d = f64::from(i);
                        buf.set_f(ecx + d, ecy + d, pal.ink);
                        buf.set_f(ecx + d, ecy - d, pal.ink);
                    }
                }
                Mood::Done => {
                    for i in -2..=2 {
                        let d = f64::from(i);
                        buf.set_f(
                            ecx + d,
                            ecy + 0.9 - (1.0 - (d / 2.0).powi(2)) * 1.7,
                            pal.ink,
                        );
                    }
                }
                _ if openness < 0.3 => {
                    for i in -2..=2 {
                        buf.set_f(ecx + f64::from(i), ecy, pal.shade.mix(pal.ink, 0.55));
                    }
                }
                _ => {
                    let ery = 2.6 * (0.25 + 0.75 * openness);
                    fill_ellipse(buf, ecx, ecy, 1.6, ery, pal.ink);
                    buf.set_f(ecx - 0.6, ecy - ery * 0.45, Col::WHITE);
                }
            }
        }
    }

    /// The mouth, only when a mood truly needs one. Idle and thinking are
    /// mouthless (the grok-bot minimalism — the eyes carry the expression);
    /// done/error earn a small arc.
    #[allow(clippy::cast_precision_loss)]
    fn paint_mouth(&self, buf: &mut HalfBuf, pal: &Palette, cx: f64, cy: f64) {
        let my = cy + 3.4;
        let ink = pal.shade.mix(pal.ink, 0.7);
        match self.mood {
            Mood::Error => {
                for i in -1..=1 {
                    let d = f64::from(i);
                    buf.set_f(cx + d, my + 1.0 - d * d * 0.9, ink);
                }
            }
            Mood::Done => {
                for i in -1..=1 {
                    let d = f64::from(i);
                    buf.set_f(cx + d, my - 0.4 + d * d * 0.9, ink);
                }
            }
            _ => {}
        }
    }

    /// Render the inline badge as two braille rows: a tiny rounded face whose
    /// eyes blink and follow the mood. Braille (not half-block) is used here
    /// because the 2x4 sub-cell keeps the eyes legible at only a few cells wide.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )] // float art -> braille dot grid
    pub fn badge(&self) -> Vec<String> {
        let mut cv = Braille::new(BADGE_W, BADGE_H);
        let cx = (BADGE_W - 1) as f64 / 2.0;
        let cy = (BADGE_H - 1) as f64 / 2.0;
        let rx = f64::from(BADGE_W as u32) / 2.0 - 0.6;
        let ry = f64::from(BADGE_H as u32) / 2.0 - 0.4;
        for y in 0..BADGE_H {
            for x in 0..BADGE_W {
                let nx = (x as f64 - cx) / rx;
                let ny = (y as f64 - cy) / ry;
                let e = nx * nx + ny * ny;
                cv.set(x, y, (0.55..=1.05).contains(&e)); // head ring
            }
        }
        // Eyes: two verticals, or a shut line during a blink.
        if self.eyes_closed() {
            for x in [2usize, 3, 6, 7] {
                cv.set(x, 4, true);
            }
        } else {
            for x in [3usize, 6] {
                cv.set(x, 3, true);
                cv.set(x, 4, true);
            }
        }
        // A small smile.
        cv.set(4, 6, true);
        cv.set(5, 6, true);
        cv.render()
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

/// Paint half-block cells to `out` with truecolor, or as plain glyphs when
/// `color` is false (so piped/non-terminal output stays free of escapes).
fn write_cells<W: Write>(out: &mut W, cells: &[Vec<Cell>], color: bool) -> io::Result<()> {
    for row in cells {
        queue!(out, MoveToColumn(0))?;
        for cell in row {
            if color {
                queue!(
                    out,
                    SetForegroundColor(cell.fg.map_or(Color::Reset, Col::to_crossterm)),
                    SetBackgroundColor(cell.bg.map_or(Color::Reset, Col::to_crossterm)),
                )?;
            }
            queue!(out, Print(cell.ch))?;
        }
        if color {
            queue!(out, ResetColor)?;
        }
        queue!(out, Print("\r\n"))?;
    }
    Ok(())
}

/// Draw the startup banner head. On an interactive terminal it plays a short
/// "waking up" animation (eyes spring open over ~1s of breathing/bobbing) by
/// redrawing the head in place; otherwise it emits one static frame. Leaves the
/// cursor on the line just below the head.
#[allow(clippy::cast_possible_truncation)]
pub fn draw_banner(theme: &Theme, out: &mut impl Write) -> io::Result<()> {
    let animated = io::stdout().is_terminal();
    let frames = if animated { 14 } else { 1 };
    let dt = 0.06;
    let mut mascot = Mascot::booting();
    for frame in 0..frames {
        mascot.advance(dt);
        let cells = mascot.render_head_cells(theme);
        if animated {
            queue!(out, Print(SYNC_BEGIN))?;
        }
        if frame > 0 {
            // Rewind onto the previous frame's rows and overwrite them.
            queue!(out, MoveToPreviousLine(cells.len() as u16), MoveToColumn(0))?;
        }
        write_cells(out, &cells, animated)?;
        if animated {
            queue!(out, Print(SYNC_END))?;
        }
        out.flush()?;
        if animated && frame + 1 < frames {
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    Ok(())
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
    fn color_scale_and_mix_stay_in_gamut() {
        let base = Col::new(200, 100, 50);
        // Scaling up clamps at 255, scaling down floors at 0.
        assert_eq!(base.scale(2.0), Col::new(255, 200, 100));
        assert_eq!(base.scale(0.0), Col::new(0, 0, 0));
        // Mixing at the extremes reproduces the endpoints; the midpoint blends.
        assert_eq!(base.mix(Col::WHITE, 0.0), base);
        assert_eq!(base.mix(Col::WHITE, 1.0), Col::WHITE);
        let mid = base.mix(Col::WHITE, 0.5);
        assert!(
            mid.r > base.r && mid.g > base.g && mid.b > base.b,
            "mix lightens"
        );
    }

    #[test]
    fn half_block_encoder_maps_each_quadrant() {
        // A single 1x2 buffer exercises all four (top, bottom) combinations and
        // the fg/bg assignment, which no higher-level invariant would catch.
        let mut buf = HalfBuf::new(1, 2);
        let red = Col::new(255, 0, 0);
        let blue = Col::new(0, 0, 255);

        // both dark -> a plain space, no colour
        assert_eq!(
            buf.encode()[0][0],
            Cell {
                ch: ' ',
                fg: None,
                bg: None
            }
        );

        buf.set(0, 0, red);
        assert_eq!(
            buf.encode()[0][0],
            Cell {
                ch: '\u{2580}',
                fg: Some(red),
                bg: None
            },
            "only the top pixel is an upper block in the foreground"
        );

        buf.set(0, 1, blue);
        assert_eq!(
            buf.encode()[0][0],
            Cell {
                ch: '\u{2580}',
                fg: Some(red),
                bg: Some(blue)
            },
            "both pixels: upper block carries top as fg and bottom as bg"
        );

        let mut only_bottom = HalfBuf::new(1, 2);
        only_bottom.set(0, 1, blue);
        assert_eq!(
            only_bottom.encode()[0][0],
            Cell {
                ch: '\u{2584}',
                fg: Some(blue),
                bg: None
            },
            "only the bottom pixel is a lower block in the foreground"
        );
    }

    #[test]
    fn head_cells_have_stable_size_and_clear_corners() {
        let theme = Theme::default();
        let mut m = Mascot::new();
        m.set_mood(Mood::Thinking);
        let cells = m.render_head_cells(&theme);
        assert_eq!(cells.len(), HEAD_H / 2, "cell row count");
        for row in &cells {
            assert_eq!(row.len(), HEAD_W, "ragged row");
        }
        // A round head leaves the bounding-box corners empty.
        assert_eq!(cells[0][0].ch, ' ', "top-left corner");
        assert_eq!(cells[0][HEAD_W - 1].ch, ' ', "top-right corner");
        assert_eq!(cells[cells.len() - 1][0].ch, ' ', "bottom-left corner");
        // The head is not blank.
        assert!(
            cells.iter().any(|r| r.iter().any(|c| c.ch != ' ')),
            "head is blank"
        );
    }

    #[test]
    fn head_is_colored_not_just_shapes() {
        // The whole point of the half-block upgrade: the head carries real
        // foreground colour, not just glyphs.
        let theme = Theme::default();
        let m = Mascot::new();
        let cells = m.render_head_cells(&theme);
        assert!(
            cells.iter().any(|r| r.iter().any(|c| c.fg.is_some())),
            "head should paint coloured pixels"
        );
    }

    #[test]
    fn head_moods_differ() {
        let theme = Theme::default();
        let mut idle = Mascot::new();
        idle.set_mood(Mood::Idle);
        let mut err = Mascot::new();
        err.set_mood(Mood::Error);
        let mut done = Mascot::new();
        done.set_mood(Mood::Done);
        assert_ne!(
            idle.render_head_cells(&theme),
            err.render_head_cells(&theme)
        );
        assert_ne!(
            done.render_head_cells(&theme),
            err.render_head_cells(&theme)
        );
    }

    #[test]
    fn booting_eyes_spring_open() {
        let mut m = Mascot::booting();
        assert!(m.eyes_closed(), "boots with eyes shut");
        for _ in 0..40 {
            m.advance(0.06);
        }
        assert!(!m.eyes_closed(), "eyes open after the spring settles");
    }

    #[test]
    fn note_turn_sets_mood_and_resume_idle_returns() {
        let mut m = Mascot::new();
        assert_eq!(m.mood(), Mood::Idle, "starts idle");
        m.note_turn(true);
        assert_eq!(m.mood(), Mood::Done, "clean turn -> Done");
        m.resume_idle();
        assert_eq!(m.mood(), Mood::Idle, "typing -> back to Idle");
        m.note_turn(false);
        assert_eq!(m.mood(), Mood::Error, "failed turn -> Error");
        m.resume_idle();
        assert_eq!(m.mood(), Mood::Idle, "typing clears Error too");
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
    fn badge_is_two_braille_rows_and_blinks() {
        let mut m = Mascot::new();
        let open = m.badge();
        assert_eq!(open.len(), BADGE_H / CELL_ROWS, "badge row count");
        for row in &open {
            assert_eq!(row.chars().count(), BADGE_W / CELL_COLS, "badge width");
            for ch in row.chars() {
                assert!(
                    ch == ' ' || ('\u{2800}'..'\u{28FF}').contains(&ch),
                    "non-braille glyph {ch:?}"
                );
            }
        }
        // Force a blink frame and confirm the face actually changes.
        let mut closed_seen = None;
        for _ in 0..600 {
            m.advance(0.1);
            if m.eyes_closed() {
                closed_seen = Some(m.badge());
                break;
            }
        }
        assert_ne!(
            open,
            closed_seen.expect("idle badge should blink"),
            "blink changes badge"
        );
    }

    #[test]
    fn busy_face_alternates_for_scanning() {
        assert_ne!(Mascot::busy_face(0), Mascot::busy_face(1));
        assert_eq!(Mascot::busy_face(0), Mascot::busy_face(2));
    }

    #[test]
    fn braille_cell_encodes_dots_to_exact_glyph() {
        // A single 2x4 cell. The bit map is col0 rows0..3 = 0x01,0x02,0x04,0x40
        // and col1 rows0..3 = 0x08,0x10,0x20,0x80. A wrong dx<->dy or column swap
        // would still yield *a* valid braille glyph, so the blob-invariant tests
        // cannot catch it — this golden locks the encoding table itself.
        let mut cv = Braille::new(2, 4);
        assert_eq!(cv.render()[0], " ", "empty cell is a space");
        cv.set(0, 0, true);
        assert_eq!(cv.render()[0], "\u{2801}", "col0 row0 => dot 0x01");
        let mut pair = Braille::new(2, 4);
        pair.set(0, 0, true);
        pair.set(1, 3, true);
        assert_eq!(
            pair.render()[0],
            "\u{2881}",
            "col0 row0 + col1 row3 => 0x81"
        );
        let mut full = Braille::new(2, 4);
        for (x, y) in [
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 0),
            (1, 1),
            (1, 2),
            (1, 3),
        ] {
            full.set(x, y, true);
        }
        assert_eq!(
            full.render()[0],
            "\u{28FF}",
            "all eight dots => full braille"
        );
    }

    #[test]
    fn braille_float_set_rounds_and_clips() {
        let mut cv = Braille::new(2, 4);
        cv.set(1, 1, true); // integer set to (1,1) => dot 0x10
        assert_eq!(cv.render()[0], "\u{2810}", "col1 row1 rounds to dot 0x10");
    }

    #[test]
    fn braille_set_ignores_out_of_range() {
        let mut cv = Braille::new(2, 4);
        cv.set(9, 9, true); // past the buffer: a no-op, never a panic
        assert_eq!(
            cv.render(),
            vec![" ".to_string()],
            "out-of-range writes ignored"
        );
    }
}
