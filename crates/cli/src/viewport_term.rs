//! A minimal inline-viewport terminal with a *dynamic* height, so a slash
//! command menu can grow below the prompt without the whole-screen clear that
//! [`ratatui::Terminal`] forces when a `Viewport::Inline` size changes (its
//! `try_draw` only reacts to *screen* resizes, and its `set_viewport_area` /
//! back-buffer resize are private).
//!
//! The resize/reflow idea is shared by astrcodey and `codex-rs`: track the
//! viewport rect, and when a taller viewport would fall off the bottom, make
//! room by scrolling history up (`append_lines`) instead of clearing everything.
//! When the height is unchanged — every ordinary keystroke, and the whole
//! non-slash path — this behaves exactly like a fixed 2-row inline
//! viewport, so the day-to-day input experience is byte-for-byte what it was.
//!
//! Built only from ratatui 0.29's public [`Backend`] / [`Buffer`] surface; the
//! initial placement mirrors `Terminal::with_options` inline construction so the
//! first frame lands where the stock terminal would put it.

use std::io;

use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};

/// An inline viewport terminal whose visible height may change between frames.
pub struct CompanionTerminal<B: Backend> {
    backend: B,
    buffers: [Buffer; 2],
    current: usize,
    viewport_area: Rect,
    last_known_cursor_pos: Position,
    hidden_cursor: bool,
}

impl<B: Backend> CompanionTerminal<B> {
    /// Create an inline viewport of `height` rows anchored at the current cursor,
    /// reserving screen room exactly like `ratatui::Terminal` does at construction.
    pub fn with_inline(mut backend: B, height: u16) -> io::Result<Self> {
        let size = backend.size()?;
        let (viewport_area, cursor_pos) = compute_inline_size(&mut backend, height, size, 0)?;
        Ok(Self {
            backend,
            buffers: [Buffer::empty(viewport_area), Buffer::empty(viewport_area)],
            current: 0,
            viewport_area,
            last_known_cursor_pos: cursor_pos,
            hidden_cursor: false,
        })
    }

    /// The current viewport rect in screen coordinates.
    #[must_use]
    #[allow(dead_code)] // inspection accessor; exercised by tests, used by resize diagnostics
    pub const fn viewport_area(&self) -> Rect {
        self.viewport_area
    }

    /// Draw one frame at `height` rows. `render` fills the viewport buffer and
    /// returns the absolute terminal cursor position (the IME anchor), or `None`
    /// to hide the cursor. Only cells that changed since the last frame are
    /// written, so a static frame costs nothing.
    pub fn draw<F>(&mut self, height: u16, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Buffer, Rect) -> Option<Position>,
    {
        self.set_inline_height(height)?;
        let area = self.viewport_area;
        let cursor_position = render(&mut self.buffers[self.current], area);
        self.flush()?;
        match cursor_position {
            Some(pos) => {
                self.show_cursor()?;
                self.set_cursor_position(pos)?;
                self.last_known_cursor_pos = pos;
            }
            None => self.hide_cursor()?,
        }
        self.swap_buffers();
        self.backend.flush()
    }

    /// Clear the viewport and force a full repaint on the next frame. Used on
    /// teardown.
    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        self.backend
            .set_cursor_position(self.viewport_area.as_position())?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        self.buffers[1 - self.current].reset();
        Ok(())
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.backend.set_cursor_position(position)
    }

    /// Resize the inline viewport to `height` rows, top-anchored so a menu grows
    /// downward. When it grows past the bottom of the screen, `append_lines`
    /// scrolls history up to make room (the stable-API stand-in for the gated
    /// `scroll_region_up`). Returns whether the area changed (and so was repainted).
    fn set_inline_height(&mut self, height: u16) -> io::Result<bool> {
        let size = self.backend.size()?;
        let old = self.viewport_area;
        let new_h = height.min(size.height.max(1));
        if old.height == new_h && old.width == size.width {
            return Ok(false);
        }
        if new_h > old.height {
            let extra = new_h - old.height;
            let room = size.height.saturating_sub(old.bottom());
            if extra > room {
                self.backend.append_lines(extra - room)?;
            }
        }
        let mut area = old;
        area.width = size.width;
        area.height = new_h;
        if area.bottom() > size.height {
            area.y = size.height.saturating_sub(area.height);
        }
        let changed = area != old;
        if changed {
            // Newly exposed rows may hold stale pixels; clear from the viewport
            // down, then blank the back buffer so the next diff repaints all of it.
            self.backend.set_cursor_position(area.as_position())?;
            self.backend.clear_region(ClearType::AfterCursor)?;
            self.set_viewport_area(area);
            self.buffers[1 - self.current].reset();
        }
        Ok(changed)
    }

    fn set_viewport_area(&mut self, area: Rect) {
        self.buffers[0].resize(area);
        self.buffers[1].resize(area);
        self.viewport_area = area;
    }

    fn flush(&mut self) -> io::Result<()> {
        let updates = self.buffers[1 - self.current].diff(&self.buffers[self.current]);
        if let Some((col, row, _)) = updates.last() {
            self.last_known_cursor_pos = Position::new(*col, *row);
        }
        self.backend.draw(updates.into_iter())
    }

    fn swap_buffers(&mut self) {
        self.buffers[1 - self.current].reset();
        self.current = 1 - self.current;
    }
}

impl<B: Backend> Drop for CompanionTerminal<B> {
    fn drop(&mut self) {
        if self.hidden_cursor {
            // Best-effort: the terminal is going away, so a failure is ignored.
            let _ = self.show_cursor();
        }
    }
}

/// Replicates ratatui 0.29's inline construction: place the viewport top at the
/// current cursor row, appending blank lines for the rows that hang below, and
/// pull the top up if those rows would run past the screen.
fn compute_inline_size<B: Backend>(
    backend: &mut B,
    height: u16,
    size: Size,
    offset_in_previous_viewport: u16,
) -> io::Result<(Rect, Position)> {
    let pos = backend.get_cursor_position()?;
    let mut row = pos.y;

    let max_height = size.height.min(height);
    let lines_after_cursor = height
        .saturating_sub(offset_in_previous_viewport)
        .saturating_sub(1);
    backend.append_lines(lines_after_cursor)?;

    let available_lines = size.height.saturating_sub(row).saturating_sub(1);
    let missing_lines = lines_after_cursor.saturating_sub(available_lines);
    if missing_lines > 0 {
        row = row.saturating_sub(missing_lines);
    }
    row = row.saturating_sub(offset_in_previous_viewport);

    Ok((Rect::new(0, row, size.width, max_height), pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn blank(_: &mut Buffer, _: Rect) -> Option<Position> {
        None
    }

    #[test]
    fn inline_viewport_starts_at_requested_height() {
        let backend = TestBackend::new(40, 12);
        let mut term = CompanionTerminal::with_inline(backend, 2).unwrap();
        assert_eq!(term.viewport_area().height, 2);
        assert_eq!(term.viewport_area().width, 40);
        // A draw at the same height must not move the viewport.
        let before = term.viewport_area();
        term.draw(2, blank).unwrap();
        assert_eq!(term.viewport_area(), before);
    }

    #[test]
    fn growing_then_shrinking_restores_height() {
        let backend = TestBackend::new(40, 20);
        let mut term = CompanionTerminal::with_inline(backend, 2).unwrap();
        term.draw(2, blank).unwrap();
        term.draw(6, blank).unwrap();
        assert_eq!(term.viewport_area().height, 6);
        term.draw(2, blank).unwrap();
        assert_eq!(term.viewport_area().height, 2);
        // Top-anchored: the input row stays put across a grow/shrink cycle.
        assert_eq!(term.viewport_area().y, 0);
    }

    #[test]
    fn height_is_clamped_to_screen() {
        let backend = TestBackend::new(40, 5);
        let mut term = CompanionTerminal::with_inline(backend, 2).unwrap();
        term.draw(100, blank).unwrap();
        assert!(term.viewport_area().height <= 5);
    }

    #[test]
    fn render_receives_full_viewport_area() {
        let backend = TestBackend::new(30, 10);
        let mut term = CompanionTerminal::with_inline(backend, 4).unwrap();
        let mut seen = Rect::ZERO;
        term.draw(4, |_, area| {
            seen = area;
            None
        })
        .unwrap();
        assert_eq!(seen, term.viewport_area());
        assert_eq!(seen.width, 30);
    }

    #[test]
    fn placement_near_bottom_keeps_viewport_on_screen() {
        // Cursor on the last row: a 4-row inline viewport cannot hang below the
        // screen, so construction must lift its top so the whole rect fits. This
        // exercises compute_inline_size's `missing_lines` pull-up path.
        let mut backend = TestBackend::new(40, 12);
        backend.set_cursor_position(Position::new(0, 11)).unwrap();
        let term = CompanionTerminal::with_inline(backend, 4).unwrap();
        let area = term.viewport_area();
        assert_eq!(area.height, 4);
        assert!(
            area.bottom() <= 12,
            "initial viewport overflows screen: {area:?}"
        );
    }

    #[test]
    fn growing_past_the_bottom_stays_on_screen() {
        // Start near the bottom, then grow: the extra rows must be absorbed
        // (append_lines scrolls history) and the top pulled up so the taller
        // viewport still fits — the set_inline_height grow+clamp path.
        let mut backend = TestBackend::new(40, 10);
        backend.set_cursor_position(Position::new(0, 8)).unwrap();
        let mut term = CompanionTerminal::with_inline(backend, 2).unwrap();
        term.draw(6, blank).unwrap();
        let area = term.viewport_area();
        assert_eq!(area.height, 6);
        assert!(
            area.bottom() <= 10,
            "grown viewport overflows screen: {area:?}"
        );
    }
}
