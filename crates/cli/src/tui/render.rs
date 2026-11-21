use super::*;

/// The input editor's border + title block. The hint lists the *bound* keys, so
/// it stays truthful after a `keymap.toml` remap instead of naming dead keys.
pub(super) fn input_block(keymap: &Keymap) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(format!(
        " input  ({} submit · shift+enter newline · {} fold · {}/{} scroll · {} keys · {} quit) ",
        keymap.hint_for(Action::Submit),
        keymap.hint_for(Action::ToggleFold),
        keymap.hint_for(Action::ScrollUp),
        keymap.hint_for(Action::ScrollDown),
        keymap.hint_for(Action::Help),
        keymap.hint_for(Action::Interrupt),
    ))
}

/// Apply the shared border/title + placeholder decoration to an input editor.
/// The placeholder advertises only affordances the full-screen shell actually
/// implements: the key reference (through its *bound* key, so it stays truthful
/// after a remap) and `@path` image attachment (expanded by `run_one_turn`). It
/// deliberately omits slash commands — the shell runs every submission as a
/// turn and dispatches none, so naming them here would be a dead hint.
pub(super) fn decorate(mut input: TextArea<'static>, keymap: &Keymap) -> TextArea<'static> {
    input.set_block(input_block(keymap));
    input.set_placeholder_text(format!(
        "Ask anything… · {} for keys · @path attaches an image",
        keymap.hint_for(Action::Help)
    ));
    input.set_placeholder_style(
        Style::default()
            .fg(Theme::current().muted().ratatui())
            .add_modifier(Modifier::DIM),
    );
    input
}

/// A fresh, empty input editor with the shared decoration. Recreating is the
/// cheapest reliable clear across tui-textarea versions.
pub(super) fn editor(keymap: &Keymap) -> TextArea<'static> {
    decorate(TextArea::default(), keymap)
}

/// An input editor preloaded with `text` (a recalled history entry), cursor
/// parked at the end. Newlines are split into rows so a multi-line entry loads
/// faithfully, matching the REPL's `textarea_from`.
pub(super) fn editor_with_text(keymap: &Keymap, text: &str) -> TextArea<'static> {
    let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut input = decorate(TextArea::from(lines), keymap);
    input.move_cursor(CursorMove::Bottom);
    input.move_cursor(CursorMove::End);
    input
}

/// Upper bound on open sections. Each section carries its own runtime, MCP
/// connections and session file, so the cap keeps a stray key from spawning
/// unbounded resources; nine single-digit tabs also keep the bar readable.
pub(super) const MAX_SECTIONS: usize = 9;

/// Process-wide header context. Every section shares one provider selection, so
/// the model, working directory and context budget are shell-level, not
/// per-section. `Default` keeps `Shell::new()` (and the reducer tests) hermetic;
/// `run_shell` fills these from the live selection.
#[derive(Clone, Debug, Default)]
pub(crate) struct Chrome {
    /// Active model name, shown in the header.
    pub(crate) model: String,
    /// Working directory (home-shortened), shown in the header.
    pub(crate) cwd: String,
    /// Context window in tokens; `0` disables the budget readout.
    pub(crate) context_window: usize,
}

/// The multi-section shell: a list of independent conversation sections plus
/// which one is active. Each section is a self-contained [`App`] with its own
/// transcript, input, follow-up queue and turn state, so sections never share
/// conversation data. The shell owns the header/tab bar and section navigation;
/// a section owns everything drawn inside its body.
pub(crate) struct Shell {
    pub(super) sections: Vec<App>,
    pub(super) active: usize,
    /// Header context (model · cwd · context budget), shared by all sections.
    pub(super) chrome: Chrome,
    /// Where the §11.4 rotation scan resumes. Sections are the delivery
    /// swimlanes (D2.1's verdict): when the loop goes idle and a fresh submit
    /// is not waiting, pending queue entries run one turn at a time,
    /// round-robin across the sections that have them, so work queued in a
    /// background section progresses even while another section is on screen.
    /// The cursor advances past each section the moment it is served, which
    /// is what makes the rotation fair rather than front-loaded.
    pub(super) delivery_cursor: usize,
}

impl Shell {
    /// A shell with a single `main` section and empty header context.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            sections: vec![App::new()],
            active: 0,
            chrome: Chrome::default(),
            delivery_cursor: 0,
        }
    }

    /// The clamped active index. Invariant: `sections` is never empty and
    /// `active` stays in range, so the `min` only guards a stale index from
    /// panicking the render loop. The event loop reads it to pick the matching
    /// runtime/state from its parallel vectors.
    pub(super) fn active_index(&self) -> usize {
        self.active.min(self.sections.len().saturating_sub(1))
    }

    /// The active section.
    pub(super) fn section(&self) -> &App {
        let idx = self.active_index();
        &self.sections[idx]
    }

    /// The active section, mutably.
    pub(super) fn section_mut(&mut self) -> &mut App {
        let idx = self.active_index();
        &mut self.sections[idx]
    }

    /// The section at `idx`, clamped like [`Self::active_index`]. The turn
    /// settle phase routes to the section whose turn just finished — identical
    /// to the active one while nav is blocked mid-turn, but carried explicitly
    /// so the routing never depends on that invariant.
    pub(super) fn section_index_mut(&mut self, idx: usize) -> &mut App {
        let clamped = idx.min(self.sections.len().saturating_sub(1));
        &mut self.sections[clamped]
    }

    /// The next section the rotation should serve (§11.4): scan from just
    /// past the cursor, wrapping, and stop at the first idle section holding
    /// at least one deliverable (non-Steer) pending entry. `None` when no
    /// section has queued work. Delivery *within* a section stays arrival
    /// order — lanes remain an observation/editing dimension (D2.1/D2.2
    /// discipline); the rotation is across sections.
    pub(super) fn next_queue_section(&self) -> Option<usize> {
        let len = self.sections.len();
        if len == 0 {
            return None;
        }
        let cursor = self.delivery_cursor % len;
        for offset in 0..len {
            let index = (cursor + offset) % len;
            let section = &self.sections[index];
            if !section.model.is_running()
                && section
                    .model
                    .queue()
                    .items()
                    .iter()
                    .any(|entry| entry.mode != crate::core::DeliveryMode::Steer)
            {
                return Some(index);
            }
        }
        None
    }

    /// Advance to the next section, wrapping at the end. A no-op with a single
    /// section so the tab bar never flickers on a solo session.
    pub(super) fn next_section(&mut self) {
        let n = self.sections.len();
        if n > 1 {
            self.active = (self.active_index() + 1) % n;
        }
    }

    /// Step back to the previous section, wrapping at the start.
    pub(super) fn prev_section(&mut self) {
        let n = self.sections.len();
        if n > 1 {
            self.active = (self.active_index() + n - 1) % n;
        }
    }

    /// Append a freshly titled section and make it active. The caller pushes the
    /// matching runtime and state so the three vectors stay index-aligned.
    pub(super) fn push_section(&mut self, title: String) {
        self.sections.push(App::titled(title));
        self.active = self.sections.len().saturating_sub(1);
    }

    /// Pure projection: a context header across the top, then (only when more
    /// than one section is open) the tab bar, then the active section's body
    /// below. A solo session shows no lonely tab; the `*` marker externalizes
    /// which section has a live turn so a running turn stays visible from the
    /// bar.
    pub(super) fn view(&self, frame: &mut Frame) {
        let theme = Theme::current();
        let section = self.section();
        let show_tabs = self.sections.len() > 1;

        let mut constraints = vec![Constraint::Length(1)]; // header
        if show_tabs {
            constraints.push(Constraint::Length(1)); // tab bar
        }
        constraints.push(Constraint::Min(1)); // body
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());

        self.render_header(areas[0], section, frame, &theme);

        let body = if show_tabs {
            let titles: Vec<Line> = self
                .sections
                .iter()
                .map(|section| {
                    let marker = if section.turn_active { "*" } else { "" };
                    Line::from(Span::raw(format!("{marker}{}", section.title)))
                })
                .collect();
            let tabs = Tabs::new(titles)
                .select(self.active)
                .divider(Span::styled(
                    " │ ",
                    Style::default().fg(theme.muted().ratatui()),
                ))
                .highlight_style(
                    Style::default()
                        .fg(theme.accent().ratatui())
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_widget(tabs, areas[1]);
            areas[2]
        } else {
            areas[1]
        };
        view(section, body, frame, Some(self.chrome.context_window));
    }

    /// The top context bar: identity (`heartflow · model · cwd`) on the left,
    /// the context-budget readout right-aligned. Splitting the row into two
    /// cells lets the budget hug the right edge without padding math.
    pub(super) fn render_header(
        &self,
        area: Rect,
        section: &App,
        frame: &mut Frame,
        theme: &Theme,
    ) {
        let budget = self.context_budget(section, theme);
        let right_w = budget
            .as_ref()
            .map_or(0, |(text, _)| text.chars().count() as u16);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(right_w)])
            .split(area);

        let mut spans = vec![Span::styled(
            "heartflow",
            Style::default()
                .fg(theme.heading().ratatui())
                .add_modifier(Modifier::BOLD),
        )];
        let sep = Span::styled(" · ", Style::default().fg(theme.muted().ratatui()));
        if !self.chrome.model.is_empty() {
            spans.push(sep.clone());
            spans.push(Span::styled(
                self.chrome.model.clone(),
                Style::default().fg(theme.accent().ratatui()),
            ));
        }
        if !self.chrome.cwd.is_empty() {
            spans.push(sep);
            spans.push(Span::styled(
                self.chrome.cwd.clone(),
                Style::default().fg(theme.muted().ratatui()),
            ));
        }
        frame.render_widget(Line::from(spans), cols[0]);
        if let Some((text, style)) = budget {
            frame.render_widget(
                Line::from(Span::styled(text, style)).alignment(Alignment::Right),
                cols[1],
            );
        }
    }

    /// Context-budget readout `[ctx NN%]` from the active section's last usage
    /// sample against the configured window. The prompt footprint is the
    /// Anthropic total input (fresh + cache-read + cache-write). Colored by
    /// pressure: success under half, accent to 80%, error at/over the
    /// compaction threshold. `None` when no window is configured or no turn has
    /// reported usage yet.
    pub(super) fn context_budget(&self, section: &App, theme: &Theme) -> Option<(String, Style)> {
        if self.chrome.context_window == 0 {
            return None;
        }
        let usage = section.last_usage?;
        let used = usize::try_from(
            usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens),
        )
        .unwrap_or(usize::MAX);
        let pct = (used * 100 / self.chrome.context_window).min(999);
        let color = if pct >= 80 {
            theme.error()
        } else if pct >= 50 {
            theme.accent()
        } else {
            theme.success()
        };
        Some((
            format!("[ctx {pct}%]"),
            Style::default().fg(color.ratatui()),
        ))
    }
}

/// Pure projection of one section onto `area`: transcript, input editor, status
/// bar. The shell renders the tab bar above this and hands the active section
/// the body below it, so a section never draws its own chrome. Overlays are
/// modal and float over the whole frame.
pub(super) fn view(app: &App, area: Rect, frame: &mut Frame, context_window: Option<usize>) {
    let theme = Theme::current();
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // transcript
            Constraint::Length(3), // input editor (with its border)
            Constraint::Length(1), // status bar
        ])
        .split(area);

    // -- Center: scrollable transcript --------------------------------------
    // Finished history (memoized) plus a synthetic live tail (streaming text,
    // thinking, running-tool marker). The transcript projection is cached and
    // rebuilt only when the transcript or the palette changes, so a streaming
    // redraw — which grows just the tail, never the frozen transcript — reuses
    // it. `visible_window` then measures wrapped heights over the virtual
    // concatenation `cache ⧺ tail` from the bottom and the paragraph is handed
    // only the rows that can show, so a frame is O(visible), not O(transcript).
    let body_height = areas[0].height.saturating_sub(2) as usize; // minus block borders
    let key = (app.transcript_version, Theme::generation());
    if app.render_key.get() != key {
        *app.render_cache.borrow_mut() = render_items(&app.transcript);
        app.render_key.set(key);
    }
    let cache = app.render_cache.borrow();

    // The live tail is small and rebuilt every frame; it is never cached because
    // it changes on each streaming delta.
    let mut tail: Vec<Line<'static>> = Vec::new();
    if app.turn_active {
        if !app.thinking_buf.is_empty() {
            tail.push(Line::from(Span::styled(
                app.thinking_buf.clone(),
                Style::default()
                    .fg(theme.muted().ratatui())
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            )));
        }
        if !app.assistant_buf.is_empty() {
            tail.push(Line::from(Span::raw(app.assistant_buf.clone())));
        }
        if let Some(tool) = &app.running_tool {
            tail.push(Line::from(Span::styled(
                format!("· running `{tool}`"),
                Style::default().fg(theme.accent().ratatui()),
            )));
        }
    }

    // Visible-window virtualization over `cache ⧺ tail`: measure wrapped heights
    // from the bottom with ratatui's own composer, then materialize only the
    // rows that can show. Counting composed rows (not source lines) is what
    // keeps the newest wrapped output pinned to the viewport bottom.
    let inner_width = areas[0].width.saturating_sub(2); // minus block borders
    let cache_len = cache.len();
    let (start_src, sub_offset) = visible_window(
        cache_len + tail.len(),
        |i| {
            let line = if i < cache_len {
                &cache[i]
            } else {
                &tail[i - cache_len]
            };
            wrapped_height(line, inner_width)
        },
        body_height,
        usize::from(app.scroll_up),
    );
    let window: Vec<Line> = if start_src < cache_len {
        cache[start_src..]
            .iter()
            .map(reborrow_line)
            .chain(tail.iter().map(reborrow_line))
            .collect()
    } else {
        tail[start_src - cache_len..]
            .iter()
            .map(reborrow_line)
            .collect()
    };
    let transcript = Paragraph::new(Text::from(window))
        .wrap(Wrap { trim: false })
        .scroll((sub_offset as u16, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" transcript ")
                .border_style(Style::default().fg(theme.muted().ratatui())),
        );
    frame.render_widget(transcript, areas[0]);

    // -- Bottom: input editor + status bar ----------------------------------
    frame.render_widget(&app.input, areas[1]);
    // Park the real cursor at the editor's caret so the terminal draws IME
    // candidates in the right place (native composition, never self-drawn).
    // The column is display cells, not char count — tui-textarea renders wide
    // glyphs two cells wide, so the raw char column drifts (see
    // `editor::display_col`).
    let (row, col) = app.input.cursor();
    let line = app.input.lines().get(row).map(String::as_str).unwrap_or("");
    let caret_x = areas[1]
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(display_col(line, col)).unwrap_or(u16::MAX));
    let caret_y = areas[1].y.saturating_add(1).saturating_add(row as u16);
    frame.set_cursor_position((caret_x, caret_y));

    let mut status_spans = vec![Span::styled(
        if app.turn_active { " ● " } else { " ○ " },
        Style::default().fg(if app.turn_active {
            theme.accent().ratatui()
        } else {
            theme.success().ratatui()
        }),
    )];
    let status_note = app.rendered_status_note();
    status_spans.push(Span::styled(
        status_note.as_ref(),
        Style::default().fg(theme.muted().ratatui()),
    ));
    if let Some(usage) = app.last_usage {
        status_spans.push(Span::styled(
            format!(
                "  [in {} / out {} / cache {}]",
                usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
            ),
            Style::default()
                .fg(theme.muted().ratatui())
                .add_modifier(Modifier::DIM),
        ));
        // Context-pressure warning: when cumulative usage exceeds 70% of the
        // window, show a soft hint; past 90% make it loud. Helps users notice
        // they're approaching a compaction cliff before the model starts
        // truncating context silently.
        if let Some(window) = context_window {
            // Same accounting as the header's context budget above: all input
            // (fresh + cache read + cache creation), excluding output. Using
            // raw `input_tokens` alone misses cache hits, which dominate
            // multi-turn sessions and would make the warning never fire.
            let total = usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens);
            let pct = if window > 0 {
                f64::from(total) / window as f64
            } else {
                0.0
            };
            if pct >= 0.9 {
                status_spans.push(Span::styled(
                    format!("  [context: {:.0}% — near limit]", pct * 100.0),
                    Style::default()
                        .fg(theme.error().ratatui())
                        .add_modifier(Modifier::BOLD),
                ));
            } else if pct >= 0.7 {
                status_spans.push(Span::styled(
                    format!("  [context: {:.0}%]", pct * 100.0),
                    Style::default().fg(theme.accent().ratatui()),
                ));
            }
        }
    }
    let queued = app.model.queue().len();
    if queued > 0 {
        // Split the count by delivery mode (§11.1 P-C): a steer in flight is
        // a different promise than a follow-up waiting its turn.
        let steering = app
            .model
            .queue()
            .items()
            .iter()
            .filter(|entry| entry.mode == crate::core::DeliveryMode::Steer)
            .count();
        let label = if steering > 0 {
            format!("  [{} queued · {} steering]", queued - steering, steering)
        } else {
            format!("  [{queued} queued]")
        };
        status_spans.push(Span::styled(
            label,
            Style::default().fg(theme.accent().ratatui()),
        ));
    }
    frame.render_widget(Line::from(status_spans), areas[2]);

    // Overlays float above every region. Layering, bottom to top: the read-only
    // key reference, then the queue panel, then the guide draft, then the
    // permission popup, so an approval always wins the topmost layer.
    render_help_overlay(app, frame);
    render_queue_overlay(app, frame);
    render_guide_overlay(app, frame);
    render_permission_overlay(app, frame);
}

/// Draw the centered approval popup when a tool is waiting on a decision. The
/// highlighted option mirrors `pending.selected`; Enter confirms it.
pub(super) fn render_permission_overlay(app: &App, frame: &mut Frame) {
    let Some(pending) = &app.permission else {
        return;
    };
    let theme = Theme::current();
    let area = frame.area();
    // The policy's reason for stopping is worth a row of its own: it turns the
    // popup from "approve this?" into "approve this because it leaves the
    // workspace".
    let reason = pending.request.reason.as_deref();
    let width = (area.width * 3 / 5).clamp(40, 80).min(area.width);
    let height = (if reason.is_some() { 11u16 } else { 9u16 }).min(area.height);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let preview: String = pending.request.input.chars().take(160).collect();
    let mut lines = vec![
        Line::from(Span::styled(
            format!("Allow `{}`?", pending.request.tool_name),
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            preview,
            Style::default().fg(theme.muted().ratatui()),
        )),
    ];
    if let Some(reason) = reason {
        lines.push(Line::from(Span::styled(
            format!("why: {reason}"),
            Style::default().fg(theme.error().ratatui()),
        )));
    }
    lines.push(Line::from(""));
    for (index, option) in pending.options.iter().enumerate() {
        let selected = index == pending.selected;
        let prefix = if selected { "❯ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!("{prefix}{option}"), style)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" permission ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: true })
            .block(block),
        popup,
    );
}

/// Draw the centered guide confirm while the operator shapes a next-task
/// draft. T6 two-column view: the left column keeps the task and the assembly
/// note; the right column is the **editable** draft editor — what it holds at
/// confirm time is exactly what [`App::guide_key`] sends.
pub(super) fn render_guide_overlay(app: &App, frame: &mut Frame) {
    let Some(pending) = &app.guide else {
        return;
    };
    let theme = Theme::current();
    let area = frame.area();
    let width = (area.width * 3 / 4).clamp(50, 110).min(area.width);
    let height = 16u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(Rect {
            x: popup.x + 1,
            y: popup.y + 1,
            width: popup.width.saturating_sub(2),
            height: popup.height.saturating_sub(2),
        });
    let mut left = vec![
        Line::from(Span::styled(
            "guide · next task",
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            pending.task.clone(),
            Style::default().fg(theme.muted().ratatui()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "the draft was assembled from the work log, the current state and the task; edit the right column freely — Enter sends it as-is.",
            Style::default().fg(theme.muted().ratatui()).add_modifier(Modifier::DIM),
        )),
    ];
    left.push(Line::from(""));
    left.push(Line::from(Span::styled(
        "Enter sends · Esc keeps the task · queued behind a running turn",
        Style::default().fg(theme.accent().ratatui()),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(left)).wrap(Wrap { trim: true }),
        columns[0],
    );
    frame.render_widget(&pending.editor, columns[1]);
    // A border around the whole popup gives the two columns one shared frame.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" guide ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(block, popup);
}

/// Draw the centered queue panel (§11.1 P-C): the live pending queue with the
/// selected row highlighted, plus the editing keys. Rows are read at render
/// time, so a settle-time delivery cannot leave the panel showing ghosts.
pub(super) fn render_queue_overlay(app: &App, frame: &mut Frame) {
    let Some(panel) = &app.queue_panel else {
        return;
    };
    let theme = Theme::current();
    let area = frame.area();
    let entries = app.model.queue().items();
    let rows = entries.len().max(1);
    // Title + one line per entry + a blank + the footer + two borders.
    let height = (rows as u16 + 5).min(area.height);
    let width = (area.width * 3 / 4).clamp(50, 110).min(area.width);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let mut lines = vec![Line::from(Span::styled(
        format!(
            "pending queue · {} (oldest first) · recent ledger {}",
            entries.len(),
            app.model.queue().recent().len()
        ),
        Style::default()
            .fg(theme.accent().ratatui())
            .add_modifier(Modifier::BOLD),
    ))];
    for (index, entry) in entries.iter().enumerate() {
        let selected = index == panel.selected.min(entries.len().saturating_sub(1));
        let mode = if entry.mode == crate::core::DeliveryMode::Steer {
            " ·steer"
        } else {
            ""
        };
        let lane = if entry.lane == crate::core::MAIN_LANE {
            String::new()
        } else {
            format!(" @{}", entry.lane)
        };
        let text: String = entry.text.chars().take(90).collect();
        let row = format!(
            "{}#{} [{}{lane}{mode}] {text}",
            if selected { "❯ " } else { "  " },
            entry.id,
            entry.source.label()
        );
        let style = if selected {
            Style::default()
                .fg(theme.accent().ratatui())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted().ratatui())
        };
        lines.push(Line::from(Span::styled(row, style)));
    }
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "(empty)",
            Style::default().fg(theme.muted().ratatui()),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ select · e edit in input · x withdraw · +/- reorder · l re-lane · Esc close",
        Style::default().fg(theme.accent().ratatui()),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" queue ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block),
        popup,
    );
}

/// Draw the centered key-reference overlay while the operator consults the
/// bindings. Each row is one action's *live* key (via `hint_for`) beside its
/// description, so the overlay stays truthful after a `keymap.toml` remap and
/// surfaces bindings the input bar has no room to list (guide, sections). Rows
/// are not wrapped: the table height is then exactly one line per action, and a
/// description too long for a narrow popup is clipped rather than pushing the
/// footer out of the box.
pub(super) fn render_help_overlay(app: &App, frame: &mut Frame) {
    if !app.help_open {
        return;
    }
    let theme = Theme::current();
    let area = frame.area();
    let rows = app.keymap.help_rows();
    // Title + one line per action + a blank + a footer + two borders.
    let height = (rows.len() as u16 + 5).min(area.height);
    let width = (area.width * 3 / 5).clamp(46, 84).min(area.width);
    let x = area.x + (area.width.saturating_sub(width) / 2);
    let y = area.y + (area.height.saturating_sub(height) / 2);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    // Pad the key column to the widest binding so the descriptions align.
    let key_w = rows
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled(
        "key reference",
        Style::default()
            .fg(theme.accent().ratatui())
            .add_modifier(Modifier::BOLD),
    ))];
    for (key, desc) in rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{key:<key_w$}"),
                Style::default()
                    .fg(theme.heading().ratatui())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(desc, Style::default().fg(theme.muted().ratatui())),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "{} closes · rebind any key in keymap.toml",
            app.keymap.hint_for(Action::Escape)
        ),
        Style::default().fg(theme.accent().ratatui()),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" keys ")
        .border_style(Style::default().fg(theme.accent().ratatui()));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);
}

/// Enter the alternate screen + raw mode, installing a panic hook that always
/// restores the terminal so a crash never leaves the user's shell wedged in
/// raw/alt-screen state.
pub(super) fn enter() -> io::Result<Terminal<Backend>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        previous(info);
    }));
    Ok(terminal)
}

/// Restore the ordinary terminal. Best-effort: each step is ignored on error so
/// a failure to leave the alt screen never masks the real result.
pub(super) fn exit(terminal: &mut Terminal<Backend>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()
}

/// Draw one frame bracketed by DEC 2026 synchronized-update sequences, so a
/// supporting terminal composites the whole frame atomically — no tearing or
/// flicker between the clear and the redraw. Terminals without DEC 2026 ignore
/// the bracketing escapes, so this is a pure quality wrapper with no downside.
/// ratatui 0.29's `CrosstermBackend` does not emit them itself, so the shell
/// adds them around every draw. Generic over the writer so the emitted byte
/// sequence is unit-testable against an in-memory `Vec<u8>`.
pub(super) fn draw_synced<W, F>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: F,
) -> io::Result<()>
where
    W: io::Write,
    F: FnOnce(&mut Frame),
{
    queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    terminal.draw(render)?;
    queue!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    terminal.flush()
}
