//! The REPL companion: a lively terminal heart drawn from pure geometry, with
//! no image assets and no third-party animation framework.
//!
//! The figure is a filled heart carrying a heartbeat trace. Four flat tones only
//! — a bright contour, a darker body, the trace, and a highlight on the trace's
//! spike. Nothing is shaded: a vertical gradient across a filled shape quantises
//! into visible horizontal stripes at this size, which is what made the earlier
//! gradient-filled egg read as a bee.
//!
//! Two renderers share one [`Mascot`] state machine, both driven by the same
//! spring physics, and each uses the dot-matrix packing that suits its size:
//!   * [`Mascot::render_head_cells`] rasterises the banner as **half-block
//!     truecolor** cells (`▀`/`▄`/`█`, two full-colour pixels per cell —
//!     foreground = top pixel, background = bottom), coloured via [`draw_banner`].
//!   * [`Mascot::badge`] draws the same heart as a tiny **braille** figure (2×4
//!     sub-pixels per cell, the same sub-cell technique ratatui's `Canvas` uses)
//!     for the fixed inline input viewport, where ten dots across is the
//!     smallest size at which a heart still reads as one.
//!
//! The figure is *interactive*: the spring integrator ([`Spring`]) gives the
//! breathing squash, the bob, the wake-up, the heartbeat that dims and lights the
//! whole figure, and the little pop when a mood flips — all computed only when
//! the clock advances, so idle CPU stays near zero. Motion is a standard
//! critically-damped spring; every "pixel" is an original geometric glyph chosen
//! to read on any UTF-8 truecolor terminal, and the base hues come from [`Theme`]
//! so the companion stays in the project's one coordinated colour family.

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
    /// Waiting at the prompt: one beat, in place, on the breathing period.
    Idle,
    /// Streaming a response / reasoning: a single faint beat wanders slowly.
    Thinking,
    /// Running a tool: a fast beat sweeps back and forth.
    Busy,
    /// Turn finished cleanly: a full beat, held, in the success hue.
    Done,
    /// Turn failed: the trace drops and stalls, in the error hue.
    Error,
}

impl Mood {
    /// Whether this mood keeps the resting heartbeat rhythm (busy and thinking
    /// hold a steady pulse instead, so the figure reads as *working*).
    const fn beats(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// Half-block canvas size for the banner, in sub-pixels. Each cell is 1 column x
/// 2 rows of pixels, so the banner is `HEAD_W` cells wide and `HEAD_H / 2` cells
/// tall.
const HEAD_W: usize = 24;
const HEAD_H: usize = 18;
/// Braille canvas size for the inline badge, in sub-pixels (2 wide x 4 tall per
/// cell), so the badge is `BADGE_W / 2` cells wide and `BADGE_H / 4` cells tall.
const BADGE_W: usize = 10;
const BADGE_H: usize = 8;
/// Braille sub-pixels per cell, horizontally and vertically.
const CELL_COLS: usize = 2;
const CELL_ROWS: usize = 4;

/// The inline badge's heart, one dot row per string, `#` a lit dot. Ten dots
/// across is the narrowest a heart still reads at, and at that size a hand-drawn
/// mask beats sampling the banner's geometry: every lobe and the tip gets its
/// own dot row instead of whatever the curve happens to round to. [`BADGE_SQUEEZE`]
/// is the same heart with the systole cut off the tip.
const BADGE_HEART: [&str; BADGE_H] = [
    "..##..##..",
    ".########.",
    ".########.",
    ".########.",
    "..######..",
    "...####...",
    "....##....",
    "..........",
];
const BADGE_SQUEEZE: [&str; BADGE_H] = [
    "..##..##..",
    ".########.",
    ".########.",
    "..######..",
    "...####...",
    "....##....",
    "..........",
    "..........",
];

/// Heart geometry on the banner canvas, in half-block pixels: half-width, full
/// height, and how finely the outline is sampled ([`heart_outline`]).
///
/// The proportions are fixed by the screen, not the canvas. A half-block pixel is
/// 16 x 12 px on a normal terminal, so it is *wider* than it is tall and the
/// figure needs more rows than columns to match a heart glyph's ~1.11:1. These
/// numbers give 14 columns by 16 rows, i.e. 1.17:1 on screen.
const HEART_HW: f64 = 7.2;
const HEART_H: f64 = 17.6;
/// Outline samples per heart. At 14 px across, 180 points already land a sample
/// in every boundary pixel; more only costs time on the startup frames.
const HEART_SAMPLES: usize = 180;
/// Horizontal and vertical centre of the heart on the canvas. The bob and the
/// breathing scale pivot on the vertical one.
const HEART_CX: f64 = (HEAD_W - 1) as f64 / 2.0;
const HEART_CY: f64 = (HEAD_H - 1) as f64 / 2.0;
/// The trace's rest line: the heart's own centre line, where the silhouette is
/// still twelve pixels wide, so the trace clears the contour on both sides
/// instead of running into it.
const TRACE_DY: f64 = 0.0;
/// Half-width of the beat's segment, in columns, and how many of its outermost
/// columns fade out. Bounding it is what keeps the trace a pulse *inside* the
/// figure: run edge to edge with a lead either side, the same line reads as a
/// sash strapped across the heart.
const TRACE_SPAN: f64 = 4.0;
const TRACE_FADE: f64 = 1.5;
/// Below this fraction of the glow the fade is not painted at all. The tail of a
/// smooth ramp is a lone almost-black pixel, which reads as dirt on the figure
/// rather than as the end of a line.
const TRACE_MIN: f64 = 0.3;
/// How far the working moods' beat wanders off centre, in columns, and how fast.
const THINK_SWAY: f64 = 3.0;
const BUSY_SWAY: f64 = 3.5;
/// Seconds between beats at rest, matched to the breathing period.
const BEAT_PERIOD: f64 = 2.6;

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

/// The mood-resolved palette the renderers share, so the banner heart, its
/// badge, and the trace all stay harmonised. The design language is deliberately
/// flat: one contour, one body, one signal.
struct Palette {
    /// Outer contour: the mood hue washed toward white.
    shell: Col,
    /// Heart body: the mood hue darkened. A flat fill, not a gradient — a
    /// gradient would quantise into horizontal stripes across these 16 rows.
    shade: Col,
    /// The heartbeat trace and its spike highlight, from the theme's emphasis hue.
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

/// The heart's outline, sampled once into a closed polygon in unit space: `u`
/// runs -1..=1 across the lobes, `v` runs 0 at the clefts down to 1 at the tip.
///
/// This is the classic parametric heart, and it is the third construction tried
/// against this canvas. The implicit cubic `(u²+v²−1)³ − u²v³ = 0` rasterised as
/// a rounded box with a dent: over 18 x 16 pixels its sides vary 8% in half-width
/// and its cleft is under a pixel deep. Two overlapping discs with a power-tapered
/// cone needed a hand-cut V and still left six equal-width rows through the middle
/// and a blunt two-pixel stem, so the figure read as a crenellated box. Sampling
/// the curve instead gives a deep three-row cleft, a continuous taper to a
/// two-pixel tip, and no flat run longer than a third of the height.
#[allow(clippy::cast_precision_loss)] // sample index -> angle
fn heart_outline() -> Vec<[f64; 2]> {
    let samples: Vec<[f64; 2]> = (0..HEART_SAMPLES)
        .map(|i| {
            let t = std::f64::consts::TAU * i as f64 / HEART_SAMPLES as f64;
            let (sin, cos) = t.sin_cos();
            let v = 13.0 * cos - 5.0 * (2.0 * t).cos() - 2.0 * (3.0 * t).cos() - (4.0 * t).cos();
            [16.0 * sin * sin * sin, v]
        })
        .collect();
    // Normalise to the curve's own bounding box, so the canvas constants are the
    // only place the figure's size is stated.
    let top = samples
        .iter()
        .map(|p| p[1])
        .fold(f64::NEG_INFINITY, f64::max);
    let tip = samples.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
    samples
        .iter()
        .map(|[u, v]| [u / 16.0, (top - v) / (top - tip)])
        .collect()
}

/// Whether `(x, y)` falls inside the heart centred on `(cx, cy)`. `k` scales every
/// length at once, so the renderer animates the whole figure through one knob.
///
/// The test is an even-odd crossing count along the row: the outline is closed, so
/// a point is inside exactly when a ray from it crosses the boundary an odd number
/// of times. That resolves the cleft and the tip exactly, which sampling the curve
/// per pixel would not.
fn heart_inside(outline: &[[f64; 2]], x: f64, y: f64, cx: f64, cy: f64, k: f64) -> bool {
    let u = (x - cx) / (HEART_HW * k);
    let v = (y - (cy - 0.5 * HEART_H * k)) / (HEART_H * k);
    // The curve reaches |u| = 1 exactly once, at the widest row.
    if !(0.0..=1.0).contains(&v) || u.abs() > 1.0 {
        return false;
    }
    let mut inside = false;
    let mut previous = outline[outline.len() - 1];
    for &[px, py] in outline {
        if (py > v) != (previous[1] > v) {
            let cut = (previous[0] - px) * (v - py) / (previous[1] - py) + px;
            if u < cut {
                inside = !inside;
            }
        }
        previous = [px, py];
    }
    inside
}

/// One column of the QRS complex, as a vertical offset from the trace's rest
/// line with `+` pointing down the screen: a small dip, the R spike, the S dip,
/// then recovery. `d` is the distance from the beat's centre, in columns.
///
/// The windows are delimited at half-integers so that every integer column falls
/// in exactly one of them. That matters: the beat's centre is usually fractional
/// (the idle beat sits on the canvas centre, `HEART_CX` = 11.5), and on
/// quarter-integer windows no sampled column lands in the spike at all — the
/// R spike vanishes and leaves a lone floating pixel behind.
fn qrs(d: f64) -> f64 {
    if d < -1.5 || d >= 2.5 {
        0.0
    } else if d < -0.5 {
        0.9
    } else if d < 0.5 {
        -3.4
    } else if d < 1.5 {
        1.6
    } else {
        0.5
    }
}

/// Where the mood puts its beat, and how tall. `Idle` beats in place, `Thinking`
/// wanders a faint beat slowly, `Busy` sweeps a fast one, `Done` holds a full
/// beat, and `Error` has no complex at all — its trace drops and stalls instead.
/// Only the brightness of the beat moves with the clock; the waveform's shape is
/// fixed per mood, so the figure never wriggles.
///
/// The working moods oscillate rather than travel: a beat crossing the whole
/// canvas would spend most of its cycle off the silhouette, leaving the figure
/// blank, so `THINK_SWAY` and `BUSY_SWAY` keep it over the heart.
fn beat_at(mood: Mood, t: f64) -> (f64, f64) {
    match mood {
        Mood::Idle => (HEART_CX, 0.9),
        Mood::Thinking => (HEART_CX + THINK_SWAY * (t * 0.9).sin(), 0.8),
        Mood::Busy => (HEART_CX + BUSY_SWAY * (t * 2.6).sin(), 1.0),
        Mood::Done => (HEART_CX, 1.0),
        Mood::Error => (HEART_CX, 0.0),
    }
}

/// The trace height at column `x`: a function graph, so the line is connected by
/// construction and never needs a path-walking rasteriser.
fn pulse_dy(mood: Mood, x: f64, centre: f64, amp: f64) -> f64 {
    let mut dy = qrs(x - centre) * amp;
    // The failed trace steps down at the beat and stays down: a signal that
    // stopped, rather than one that merely lost its spike.
    if mood == Mood::Error && x >= centre {
        dy += 2.6;
    }
    dy
}

/// Which banner-canvas pixels fall inside the heart silhouette, row-major.
#[allow(clippy::cast_precision_loss)]
fn heart_mask(outline: &[[f64; 2]], cx: f64, cy: f64, k: f64) -> Vec<bool> {
    (0..HEAD_H)
        .flat_map(|y| {
            (0..HEAD_W).map(move |x| heart_inside(outline, x as f64, y as f64, cx, cy, k))
        })
        .collect()
}

/// The heartbeat: one sample per column, joined to the previous column by a
/// vertical run, so the QRS spike is a connected riser rather than a pixel
/// floating above the line. Only columns within [`TRACE_SPAN`] of the beat are
/// drawn, faded out at the ends and clipped to the silhouette, so what shows is a
/// pulse in the figure rather than a line across it.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn paint_trace(
    buf: &mut HalfBuf,
    pal: &Palette,
    mood: Mood,
    t: f64,
    cy: f64,
    alive: f64,
    is_in: &impl Fn(i64, i64) -> bool,
) {
    let (centre, amp) = beat_at(mood, t);
    let mood_gain = match mood {
        Mood::Error => 0.6,
        Mood::Thinking => 0.8,
        _ => 1.0,
    };
    let gain = mood_gain * (0.3 + 0.7 * alive);
    let rest = cy + TRACE_DY;
    let sample = |x: f64| -> i64 { (rest + pulse_dy(mood, x, centre, amp)).round() as i64 };
    let first = (centre - TRACE_SPAN).ceil() as i64;
    let last = (centre + TRACE_SPAN).floor() as i64;
    let mut previous = sample(first as f64);
    // The topmost sample of the trace, which is the top of the R wave.
    let mut peak = (previous, first);
    for x in first..=last {
        let y = sample(x as f64);
        // Fade the outermost columns, so the segment has no cut ends.
        let strength =
            gain * ((TRACE_SPAN - (x as f64 - centre).abs()) / TRACE_FADE).clamp(0.0, 1.0);
        if strength > TRACE_MIN {
            for row in y.min(previous)..=y.max(previous) {
                if is_in(x, row) {
                    buf.set_i(x, row, pal.glow.scale(strength));
                }
            }
        }
        if amp > 0.0 && y < peak.0 {
            peak = (y, x);
        }
        previous = y;
    }
    // The spike's highlight rides the top of the R wave, the one pixel that
    // carries the beat. The old design floated a spark in the negative space
    // above the figure, which read as a detached plus sign; an accent on the
    // trace belongs to the character instead of sitting beside it.
    if amp > 0.0 && gain > 0.2 && is_in(peak.1, peak.0) {
        buf.set_i(peak.1, peak.0, pal.glow.mix(Col::WHITE, 0.55).scale(gain));
    }
}

/// A dot buffer that paints sub-pixels by dot coordinate and encodes them as
/// braille cells. Backs the compact inline [`Mascot::badge`], where the 2x4
/// sub-cell resolution packs a legible heart into five cells wide.
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

/// Time-based state for the companion. Owns the pulse spring the renderers draw
/// every pixel from, the beat cadence that drives it, and the current mood; the
/// REPL drives it by calling [`Mascot::advance`] on each idle tick and
/// [`Mascot::set_mood`] at phase changes.
#[derive(Debug, Clone)]
pub struct Mascot {
    mood: Mood,
    elapsed: f64,
    /// Aliveness, 0..=1: `1` at rest, springing up while the companion wakes and
    /// dropping for one systole on each beat. Every tone in the figure and the
    /// trace's gain scale from it, so one spring animates the whole character.
    pulse: Spring,
    /// Wall-clock seconds when the current beat began, or `None` between beats.
    beat_at: Option<f64>,
    /// When the next beat is due.
    next_beat: f64,
}

impl Default for Mascot {
    fn default() -> Self {
        Self::new()
    }
}

impl Mascot {
    /// A freshly-born idle mascot at time zero, at rest.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mood: Mood::Idle,
            elapsed: 0.0,
            pulse: Spring::new(1.0),
            beat_at: None,
            next_beat: BEAT_PERIOD,
        }
    }

    /// A mascot whose pulse starts dark and springs up — used by [`draw_banner`]
    /// so the startup animation reads as the companion coming to life.
    #[must_use]
    pub fn booting() -> Self {
        let mut mascot = Self::new();
        mascot.pulse = Spring::new(0.0);
        mascot.pulse.target = 1.0; // spring alight over the first frames
        mascot
    }

    /// The mood currently shown.
    #[must_use]
    #[allow(dead_code)] // forward API for the status-bar host (P4-c.3)
    pub fn mood(&self) -> Mood {
        self.mood
    }

    /// Transition to a new mood. A flip to [`Mood::Done`] kicks the pulse off
    /// rest so the under-damped spring rings it back up — a visible surge on the
    /// frame the turn lands, instead of a silent recolour.
    pub fn set_mood(&mut self, mood: Mood) {
        if mood == self.mood {
            return;
        }
        self.mood = mood;
        if mood == Mood::Done {
            self.pulse = Spring::bouncy(0.45);
            self.pulse.target = 1.0;
        } else if mood != Mood::Idle {
            // Working moods hold a steady pulse rather than the resting rhythm.
            self.beat_at = None;
            self.pulse.target = 1.0;
        }
    }

    /// Advance elapsed time by `dt` seconds, driving the breathing, the beat
    /// cadence, and the pulse spring. Cheap; call once per idle tick.
    pub fn advance(&mut self, dt: f64) {
        self.elapsed += dt;
        // Beat scheduling (idle only: a working mascot holds a steady pulse).
        if self.mood.beats() {
            if self.beat_at.is_none() && self.elapsed >= self.next_beat {
                self.beat_at = Some(self.elapsed);
                self.pulse.target = 0.0;
            }
            if let Some(started) = self.beat_at {
                // The spring drops the pulse, holds the systole, then releases
                // it; the beat is over once it is back up.
                let since = self.elapsed - started;
                if self.pulse.value() < 0.15 && since > 0.08 {
                    self.pulse.target = 1.0;
                }
                if self.pulse.value() > 0.9 && since > 0.12 {
                    self.beat_at = None;
                    self.next_beat = self.elapsed + BEAT_PERIOD;
                }
            }
        }
        self.pulse.step(dt);
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
    /// idle [`Mood::Idle`] baseline. Idempotent (a no-op once already idle).
    pub fn resume_idle(&mut self) {
        self.set_mood(Mood::Idle);
    }

    /// Whether the figure is mid-systole: the resting rhythm squeezing the pulse
    /// spring down. A beat is the only thing that changes the badge, so this is
    /// the badge's single animation gate.
    #[must_use]
    fn contracted(&self) -> bool {
        self.mood.beats() && self.pulse.value() < 0.5
    }

    /// The mood-resolved palette: the mood hue washed toward white for the
    /// contour, darkened for the body, and the theme's emphasis hue carrying the
    /// signal.
    fn palette(&self, theme: &Theme) -> Palette {
        let base = col_from(match self.mood {
            Mood::Done => theme.success(),
            Mood::Error => theme.error(),
            _ => theme.accent(),
        });
        Palette {
            shell: base.mix(Col::WHITE, 0.42),
            shade: base.scale(0.62).mix(Col::WHITE, 0.12),
            glow: col_from(theme.emphasis()).mix(Col::WHITE, 0.28),
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

    /// Render the banner head as half-block truecolor cells: a filled heart with
    /// a heartbeat trace across it.
    ///
    /// The painting order is body, then trace, then contour. The contour is the
    /// mask's boundary layer, so drawing it last keeps the outline unbroken
    /// wherever the trace crosses it, and the whole figure stays readable as one
    /// silhouette. Every tone is a single flat colour scaled by the pulse; a
    /// vertical gradient across the fill would quantise into horizontal stripes
    /// over these ten cell rows, which is what made the old egg read as a bee.
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
        let alive = self.pulse.value().clamp(0.0, 1.0);
        let cx = HEART_CX;
        let cy = HEART_CY + bob * 0.5;
        // Waking up grows the heart into place; breathing swells it. The swell is
        // vertical-only, since a horizontal wobble reads as jitter.
        let scale = (0.9 + 0.1 * alive) / (1.0 + 0.02 * breath);
        let inside = heart_mask(&heart_outline(), cx, cy, scale);
        let is_in = |x: i64, y: i64| -> bool {
            x >= 0
                && y >= 0
                && (x as usize) < HEAD_W
                && (y as usize) < HEAD_H
                && inside[y as usize * HEAD_W + x as usize]
        };
        // The contour is the boundary layer of the mask: inside, with at least
        // one four-neighbour outside. Deriving it from the mask keeps the stroke
        // closed everywhere, which sampling the curve would not guarantee.
        let rim = |x: i64, y: i64| -> bool {
            is_in(x, y)
                && !(is_in(x - 1, y) && is_in(x + 1, y) && is_in(x, y - 1) && is_in(x, y + 1))
        };
        let mut buf = HalfBuf::new(HEAD_W, HEAD_H);
        let lit = 0.3 + 0.7 * alive;
        for y in 0..HEAD_H as i64 {
            for x in 0..HEAD_W as i64 {
                if is_in(x, y) && !rim(x, y) {
                    buf.set_i(x, y, pal.shade.scale(lit));
                }
            }
        }
        paint_trace(&mut buf, &pal, self.mood, self.elapsed, cy, alive, &is_in);
        for y in 0..HEAD_H as i64 {
            for x in 0..HEAD_W as i64 {
                if rim(x, y) {
                    buf.set_i(x, y, pal.shell.scale(lit));
                }
            }
        }
        buf.encode()
    }

    /// Render the inline badge as two braille rows: the same heart as the banner,
    /// filled, squeezed for the length of each systole. Braille (not half-block)
    /// is used here because the 2x4 sub-cell is the only packing that keeps a
    /// silhouette legible at five cells wide.
    #[must_use]
    pub fn badge(&self) -> Vec<String> {
        let art = if self.contracted() {
            BADGE_SQUEEZE
        } else {
            BADGE_HEART
        };
        let mut cv = Braille::new(BADGE_W, BADGE_H);
        for (y, row) in art.iter().enumerate() {
            for (x, dot) in row.chars().enumerate() {
                cv.set(x, y, dot == '#');
            }
        }
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
/// "waking up" animation (the pulse springs up over ~1s of breathing/bobbing) by
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
    fn booting_starts_contracted_and_springs_open() {
        let mut m = Mascot::booting();
        assert!(m.contracted(), "boots with the pulse down");
        for _ in 0..40 {
            m.advance(0.06);
        }
        assert!(!m.contracted(), "pulse up after the spring settles");
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
    fn idle_beats_over_time_and_recovers() {
        let mut m = Mascot::new(); // Idle
        let mut saw_contracted = false;
        let mut saw_recovered = false;
        for _ in 0..750 {
            m.advance(0.08);
            if m.contracted() {
                saw_contracted = true;
            } else if saw_contracted {
                saw_recovered = true;
            }
        }
        assert!(saw_contracted, "idle mascot should beat within 60s");
        assert!(saw_recovered, "pulse should come back up after a beat");
    }

    #[test]
    fn busy_mood_holds_a_steady_pulse() {
        let mut m = Mascot::new();
        m.set_mood(Mood::Busy);
        for _ in 0..750 {
            m.advance(0.08);
            assert!(!m.contracted(), "busy pulse must not contract");
        }
    }

    #[test]
    fn badge_is_two_braille_rows_and_beats() {
        let mut m = Mascot::new();
        let open = m.badge();
        assert_eq!(open.len(), BADGE_H / CELL_ROWS, "badge row count");
        for row in &open {
            assert_eq!(row.chars().count(), BADGE_W / CELL_COLS, "badge width");
            for ch in row.chars() {
                assert!(
                    ch == ' ' || ('\u{2800}'..='\u{28FF}').contains(&ch),
                    "non-braille glyph {ch:?}"
                );
            }
        }
        // Force a systole frame and confirm the figure actually changes.
        let mut squeezed = None;
        for _ in 0..750 {
            m.advance(0.08);
            if m.contracted() {
                squeezed = Some(m.badge());
                break;
            }
        }
        assert_ne!(open, squeezed.expect("idle badge should beat"), "beat");
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
