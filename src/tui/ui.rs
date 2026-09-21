use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table,
    },
    Frame,
};

use std::collections::VecDeque;

use super::app::{App, Row as AppRow};
use super::client::{Activity, JobLine, JobSummary};

/// A braille spinner: smoother than ASCII and reads as motion at 15 fps.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A running job silent for longer than this is flagged as possibly wedged.
/// Matches the xbar plugin's threshold so both readouts agree.
const IDLE_WARN_MS: u64 = 120_000;

/// One palette, so every pane agrees on what a colour means. Indexed 256-colour
/// values rather than named ones: the named set is remapped by terminal themes,
/// which is what made the old borders and dim text vary between terminals.
mod theme {
    use ratatui::style::Color;

    /// Chrome: borders and rules. Present but never competing with content.
    pub const BORDER: Color = Color::Indexed(238);
    /// Panel titles.
    pub const TITLE: Color = Color::Indexed(250);
    /// Secondary text — counts, hints, keys.
    pub const MUTED: Color = Color::Indexed(243);
    /// The selected row's background.
    pub const SELECTED_BG: Color = Color::Indexed(236);
    /// Live activity.
    pub const RUNNING: Color = Color::Indexed(80);
    /// Success.
    pub const OK: Color = Color::Indexed(114);
    /// Failure.
    pub const FAIL: Color = Color::Indexed(203);
    /// Needs attention but not a failure: idle, cancelled, timed out.
    pub const WARN: Color = Color::Indexed(215);
    /// Session identifiers.
    pub const SESSION: Color = Color::Indexed(140);
    /// Background of a search hit.
    pub const MATCH_BG: Color = Color::Indexed(222);
    /// Background of the hit `n`/`N` last landed on, so it is distinguishable from
    /// the other matches on screen.
    pub const MATCH_CURRENT_BG: Color = Color::Indexed(214);
}

/// A panel border and title in the shared style, so the two panes match.
fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(theme::TITLE).bold(),
        ))
}

/// A right-aligned mode badge for a panel's top edge.
fn badge(label: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(format!(" {label} "), Style::new().fg(color))).right_aligned()
}

pub fn render(frame: &mut Frame, app: &mut App, tick: u64) {
    let area = frame.area();

    // The search row only exists while searching, so the panes get the space back
    // as soon as the query is cleared.
    let searching = app.typing || !app.query.is_empty();
    let [main_area, search_area, status_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(if searching { 1 } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(area);
    let [list_area, log_area] =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Fill(1)]).areas(main_area);

    // The activity pane only exists once there is traffic to show, so a sidecar
    // that has only run jobs gives the whole column to the job list.
    if app.activities.is_empty() {
        render_job_list(frame, app, list_area, tick);
    } else {
        let height = activity_pane_height(app.activities.len(), list_area.height);
        let [jobs_area, activity_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).areas(list_area);
        render_job_list(frame, app, jobs_area, tick);
        render_activity(frame, app, activity_area, tick);
    }
    render_log_pane(frame, app, log_area);
    if searching {
        render_search_bar(frame, app, search_area);
    }
    render_status_bar(frame, app, status_area);

    if app.show_help {
        render_help(frame, app, area);
    } else if app.show_errors {
        render_errors(frame, app, area);
    } else if app.show_stats {
        render_stats(frame, app, area);
    }
}

/// Width of the inline bars in the stats overlay. Wide enough to show shape,
/// narrow enough that the panel fits an 80-column terminal with its labels.
const BAR_WIDTH: usize = 22;

/// A proportional bar for `value` against `max`, drawn with block characters.
///
/// Eighths rather than whole cells: a day with a twentieth of the busiest day's
/// traffic would otherwise round to nothing and read as "no activity". Any
/// non-zero value gets at least the narrowest visible sliver.
fn bar(value: u64, max: u64, width: usize) -> String {
    if value == 0 || max == 0 {
        return String::new();
    }
    const EIGHTHS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
    let eighths = (value as u128 * (width as u128 * 8) / max as u128).max(1);
    let full = (eighths / 8) as usize;
    let remainder = (eighths % 8) as usize;
    let mut out = "█".repeat(full);
    if remainder > 0 {
        out.push_str(EIGHTHS[remainder - 1]);
    }
    out
}

/// Recorded activity over time, from the sidecar's durable metrics.
///
/// Unlike every other pane this reads from disk rather than the event stream, so
/// it survives restarts — which is the whole point: the in-memory ring holds
/// minutes, and the question worth asking ("how much did I run this week") spans
/// days.
fn render_stats(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(8).clamp(32, 60);
    let mut lines: Vec<Line> = Vec::new();

    if let Some(error) = &app.stats_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("   {error}"),
            Style::new().fg(theme::FAIL),
        )));
        lines.push(Line::from(""));
    } else {
        match &app.stats {
            None => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "   loading…",
                    Style::new().fg(theme::MUTED).italic(),
                )));
                lines.push(Line::from(""));
            }
            Some(stats) if stats.days.is_empty() => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "   No calls recorded yet.",
                    Style::new().fg(theme::MUTED).italic(),
                )));
                lines.push(Line::from(""));
            }
            Some(stats) => {
                lines.push(Line::from(""));
                lines.push(section("CALLS / DAY"));
                let busiest = stats.days.iter().map(|d| d.calls).max().unwrap_or(1);
                for day in &stats.days {
                    let mut spans = vec![
                        Span::styled(
                            format!("   {:<4}", day.weekday()),
                            Style::new().fg(theme::MUTED),
                        ),
                        Span::styled(
                            format!(
                                "{:<width$}",
                                bar(day.calls, busiest, BAR_WIDTH),
                                width = BAR_WIDTH
                            ),
                            Style::new().fg(theme::RUNNING),
                        ),
                        Span::styled(format!(" {:>5}", day.calls), Style::new().fg(theme::TITLE)),
                    ];
                    // Failures ride alongside the count rather than getting their
                    // own chart: a day's failure count is only meaningful next to
                    // how much ran that day.
                    if day.failures > 0 {
                        spans.push(Span::styled(
                            format!("  ✗{}", day.failures),
                            Style::new().fg(theme::FAIL),
                        ));
                    }
                    lines.push(Line::from(spans));
                }

                if !stats.commands.is_empty() {
                    lines.push(Line::from(""));
                    lines.push(section("TOP COMMANDS"));
                    let most = stats.commands.iter().map(|c| c.calls).max().unwrap_or(1);
                    for stat in &stats.commands {
                        let p50 = match stat.p50_ms {
                            Some(ms) => format!("  p50 {}", format_millis(ms)),
                            None => String::new(),
                        };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("   {:<10}", truncate(&stat.cmd, 10)),
                                Style::new().fg(theme::TITLE),
                            ),
                            Span::styled(
                                format!(
                                    "{:<width$}",
                                    bar(stat.calls, most, BAR_WIDTH / 2),
                                    width = BAR_WIDTH / 2
                                ),
                                Style::new().fg(theme::SESSION),
                            ),
                            Span::styled(
                                format!(" {:>5}", stat.calls),
                                Style::new().fg(theme::MUTED),
                            ),
                            Span::styled(p50, Style::new().fg(theme::MUTED)),
                        ]));
                    }
                }

                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("   {} calls recorded", stats.total_calls),
                    Style::new().fg(theme::MUTED),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    let popup = centered_rect(width, lines.len() as u16 + 2, area);
    let visible = popup.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let offset = (app.stats_scroll as usize).min(max_scroll);
    let truncated = max_scroll > 0;

    let block = panel("Stats").title_top(badge(
        if truncated {
            "↑↓ scroll · any other key closes"
        } else {
            "any key closes"
        },
        theme::MUTED,
    ));

    if truncated {
        lines = lines.into_iter().skip(offset).collect();
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);

    if truncated {
        let mut state = ScrollbarState::new(max_scroll).position(offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_style(Style::new().fg(theme::BORDER)),
            popup,
            &mut state,
        );
    }
}

/// A section heading inside an overlay.
fn section(name: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {name}"),
        Style::new().fg(theme::SESSION).bold(),
    ))
}

/// Clip `text` to `max` display columns, marking the cut so a truncated command
/// name is not mistaken for a shorter one.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The retained error log.
///
/// `status_message` shows one line and the next success clears it, so an error
/// that flashed past was gone for good — and the sidecar keeps no server-side log
/// to consult instead. This is the place to actually read what happened.
fn render_errors(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(8).clamp(20, 100);
    let mut lines: Vec<Line> = Vec::with_capacity(app.errors.len() + 2);

    if app.errors.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "   No errors this session.",
            Style::new().fg(theme::OK),
        )));
        lines.push(Line::from(""));
    } else {
        // Newest first: a recurring failure is what you opened this for, and it
        // is the most recent entry.
        for entry in app.errors.iter().rev() {
            let mut spans = vec![
                Span::styled(format!(" {} ", entry.at), Style::new().fg(theme::MUTED)),
                Span::styled(entry.message.clone(), Style::new().fg(theme::FAIL)),
            ];
            // A repeat count distinguishes one stale failure from a live loop.
            if entry.count > 1 {
                spans.push(Span::styled(
                    format!(" ×{}", entry.count),
                    Style::new().fg(theme::WARN).bold(),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    let popup = centered_rect(width, lines.len() as u16 + 2, area);
    let visible = popup.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let offset = (app.error_scroll as usize).min(max_scroll);
    let truncated = max_scroll > 0;

    let title = if app.errors.is_empty() {
        "Errors".to_string()
    } else {
        format!("Errors ({})", app.errors.len())
    };
    let block = panel(&title).title_top(badge(
        if truncated {
            "↑↓ scroll · any other key closes"
        } else {
            "any key closes"
        },
        theme::MUTED,
    ));

    if truncated {
        lines = lines.into_iter().skip(offset).collect();
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);

    if truncated {
        let mut state = ScrollbarState::new(max_scroll).position(offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_style(Style::new().fg(theme::BORDER)),
            popup,
            &mut state,
        );
    }
}

fn render_job_list(frame: &mut Frame, app: &mut App, area: Rect, tick: u64) {
    let header = Row::new(vec![
        Cell::from("COMMAND"),
        Cell::from(Line::from("ELAPSED").right_aligned()),
        Cell::from(Line::from("LINES").right_aligned()),
        Cell::from("STATE"),
    ])
    .style(Style::new().fg(theme::MUTED))
    .bottom_margin(1);

    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|row| match row {
            AppRow::Header {
                session,
                jobs,
                running,
            } => session_header_row(session.as_deref(), *jobs, *running),
            AppRow::Job(i) => job_row(&app.jobs[*i], app.grouped, tick),
        })
        .collect();

    let widths = [
        Constraint::Fill(1),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(14),
    ];

    // Say which view is active: a filtered list that looks short is otherwise
    // indistinguishable from a sidecar with nothing to show.
    let shown = app.rows.iter().filter(|r| r.job_index().is_some()).count();
    let title = if app.running_only {
        format!("Running ({shown})")
    } else {
        format!("Jobs ({shown})")
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(panel(&title).title_top(badge(
            if app.grouped { "grouped" } else { "flat" },
            theme::MUTED,
        )))
        // A left bar marks the cursor, so selection survives a terminal whose
        // background colours are remapped by its own theme.
        .row_highlight_style(Style::new().bg(theme::SELECTED_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(theme::RUNNING)))
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

/// A session heading. The counts describe the group, so they sit in the state
/// column where per-job state would otherwise be.
fn session_header_row(session: Option<&str>, jobs: usize, running: usize) -> Row<'static> {
    let (label, style) = match session {
        Some(id) => (format!("● {id}"), Style::new().fg(theme::SESSION).bold()),
        None => ("○ no session".to_string(), Style::new().fg(theme::MUTED)),
    };
    let counts = if running > 0 {
        format!("{jobs} jobs, {running} live")
    } else {
        format!("{jobs} jobs")
    };
    Row::new(vec![
        Cell::from(label).style(style),
        Cell::from(""),
        Cell::from(""),
        Cell::from(counts).style(Style::new().fg(theme::MUTED)),
    ])
    .top_margin(1)
}

fn job_row(job: &JobSummary, grouped: bool, tick: u64) -> Row<'static> {
    let (state, style) = job_state(job, tick);
    // Indent under a header so the nesting is visible; in flat mode the session
    // is shown inline instead, since there is no header to carry it.
    let command = if grouped {
        Line::from(vec![Span::raw("  "), Span::raw(job.command())])
    } else if let Some(session) = job.session_short() {
        Line::from(vec![
            Span::styled(format!("{session} "), Style::new().fg(theme::SESSION)),
            Span::raw(job.command()),
        ])
    } else {
        Line::from(job.command())
    };
    Row::new(vec![
        Cell::from(command),
        Cell::from(
            Line::from(format_duration(job.elapsed_ms))
                .right_aligned()
                .fg(theme::MUTED),
        ),
        Cell::from(
            Line::from(compact_count(job.line_count))
                .right_aligned()
                .fg(theme::MUTED),
        ),
        Cell::from(state).style(style),
    ])
}

/// Thousands as `1.2k`, so a long build's line count stays inside its column.
fn compact_count(n: usize) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 100_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}k", n / 1_000)
    }
}

/// The single most useful thing to say about a job's state, and its colour.
fn job_state(job: &JobSummary, tick: u64) -> (String, Style) {
    if job.running {
        // A long silence is more informative than "running", and is the signature
        // of a process stuck on a prompt or a deadlock.
        if job.idle_ms > IDLE_WARN_MS {
            return (
                format!("idle {}", format_duration(job.idle_ms)),
                Style::new().fg(theme::WARN).bold(),
            );
        }
        let spin = SPINNER_FRAMES[(tick / 2 % SPINNER_FRAMES.len() as u64) as usize];
        return (format!("{spin} running"), Style::new().fg(theme::RUNNING));
    }

    // A killed or timed-out job needs its reason, not an exit code it never had.
    if job.ended_abnormally() {
        let outcome = job.outcome.as_ref().expect("checked by ended_abnormally");
        let killed = if outcome.escalated { " killed" } else { "" };
        let label = outcome.kind.replace('_', " ");
        return (format!("⊘ {label}{killed}"), Style::new().fg(theme::WARN));
    }

    match job.exit_code {
        Some(0) => ("✓ done".into(), Style::new().fg(theme::OK)),
        Some(code) => (
            format!("✗ exit {code}"),
            Style::new().fg(theme::FAIL).bold(),
        ),
        None => ("· ended".into(), Style::new().fg(theme::MUTED)),
    }
}

/// Rows to give the activity pane: enough for its content, but never so much
/// that the job list — the primary pane — is squeezed below a usable height.
///
/// Returns the total including borders, or 0 when there is no room at all.
fn activity_pane_height(count: usize, available: u16) -> u16 {
    /// Keep at least this many rows of job list (borders, header, a few jobs).
    const MIN_JOB_ROWS: u16 = 8;
    /// Past this the pane is a tail, not a list — older calls scroll away.
    const MAX_ROWS: u16 = 8;

    let wanted = (count as u16).min(MAX_ROWS) + 2; // + borders
    let spare = available.saturating_sub(MIN_JOB_ROWS);
    if spare < 3 {
        return 0; // no room for a border pair plus one row
    }
    wanted.min(spare)
}

/// Recent `/exec`, `/batch`, `/browser`, and `/gdocs` calls, newest last.
///
/// These have no output stream and nothing to select, so they are a readout
/// rather than a list — but they are the bulk of what the sidecar does, and
/// before this pane existed the monitor showed none of it.
fn render_activity(frame: &mut Frame, app: &App, area: Rect, tick: u64) {
    if area.height < 3 {
        return;
    }
    let running = app.activities_running();
    let title = if running > 0 {
        format!("Calls ({running} live)")
    } else {
        "Calls".to_string()
    };

    let visible = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = app
        .activities
        .iter()
        .skip(app.activities.len().saturating_sub(visible))
        .map(|a| activity_line(a, tick))
        .collect();

    frame.render_widget(Paragraph::new(Text::from(lines)).block(panel(&title)), area);
}

/// One call as `mark cmd args   duration`.
fn activity_line(activity: &Activity, tick: u64) -> Line<'static> {
    let (mark, style) = if activity.running() {
        let spin = SPINNER_FRAMES[(tick / 2 % SPINNER_FRAMES.len() as u64) as usize];
        (spin.to_string(), Style::new().fg(theme::RUNNING))
    } else if activity.failed() {
        ("✗".to_string(), Style::new().fg(theme::FAIL).bold())
    } else {
        ("✓".to_string(), Style::new().fg(theme::OK))
    };

    // A failure's reason is the useful thing; a duration on a denied call is not.
    let tail = match (&activity.error, activity.duration_ms) {
        (Some(err), _) => Span::styled(format!(" {err}"), Style::new().fg(theme::FAIL)),
        (None, Some(ms)) => Span::styled(
            format!(" {}", format_millis(ms)),
            Style::new().fg(theme::MUTED),
        ),
        (None, None) => Span::raw(""),
    };

    Line::from(vec![
        Span::styled(format!(" {mark} "), style),
        Span::raw(activity.command()),
        tail,
    ])
}

/// Sub-second durations matter here — most one-shot calls finish in milliseconds,
/// and `format_duration`'s floor would render every one of them as `0s`.
fn format_millis(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format_duration(ms)
    }
}

fn render_log_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    // The follow state is the one thing here that changes under the user's hands,
    // so it gets a right-aligned badge: appended to the command it would be the
    // first thing clipped on a long command line.
    let block = match app.selected_job() {
        Some(job) => {
            let (label, color) = if app.following() {
                ("following", theme::RUNNING)
            } else {
                ("paused", theme::WARN)
            };
            panel(&job.command()).title_top(badge(label, color))
        }
        None => panel("Output"),
    };

    if app.lines.is_empty() {
        let hint = if app.selected_job().is_some() {
            "waiting for output…"
        } else {
            "no job selected"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::new().fg(theme::MUTED).italic(),
            )))
            .block(block),
            area,
        );
        return;
    }

    // Slice the window ourselves rather than handing the whole log to
    // `Paragraph::scroll`. Lines stay in natural order — a log rendered
    // newest-first reads backwards, which inverts every multi-line block (stack
    // traces, box-drawn banners) and makes build output unreadable.
    //
    // Wrapping stays off so one log line is exactly one row, which is what lets
    // the window be computed by line index without measuring wrapped heights.
    // Overlong lines are clipped at the pane edge.
    let visible = area.height.saturating_sub(2) as usize;
    // The scroll actions need this and only the renderer knows the terminal size.
    app.log_rows = visible;

    // Filtering rebuilds the visible set, so it needs its own window: positions
    // in the filtered list have no relation to positions in the full buffer.
    let (window, total_rows, rows_below) = if app.filter_matches && !app.query.is_empty() {
        let hits: Vec<&JobLine> = app.lines.iter().filter(|l| app.matches(&l.text)).collect();
        let end = match app.pinned {
            Some(pin) => hits
                .iter()
                .position(|l| l.index >= pin)
                .map_or(hits.len(), |p| p + 1),
            None => hits.len(),
        };
        let start = end.saturating_sub(visible);
        (
            hits[start..end]
                .iter()
                .map(|l| {
                    Line::from(highlight_matches(
                        &l.text,
                        &app.match_ranges(&l.text),
                        Some(l.index) == app.cursor,
                    ))
                })
                .collect::<Vec<_>>(),
            hits.len(),
            hits.len().saturating_sub(end),
        )
    } else {
        let (start, end) = log_window(&app.lines, visible, app.pinned);
        (
            app.lines
                .iter()
                .skip(start)
                .take(end - start)
                .map(|l| {
                    Line::from(highlight_matches(
                        &l.text,
                        &app.match_ranges(&l.text),
                        Some(l.index) == app.cursor,
                    ))
                })
                .collect::<Vec<_>>(),
            app.lines.len(),
            app.lines.len().saturating_sub(end),
        )
    };

    frame.render_widget(Paragraph::new(Text::from(window)).block(block), area);

    // Track position by how far the window's end sits from the newest line, so
    // the thumb reaches the bottom exactly when the pane is tailing.
    let mut scrollbar_state = ScrollbarState::new(total_rows.saturating_sub(visible))
        .position(total_rows.saturating_sub(visible + rows_below));
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_style(Style::new().fg(theme::BORDER)),
        area,
        &mut scrollbar_state,
    );
}

/// Style every match in `text`, so hits stand out the way a terminal search does.
///
/// Ranges come from the compiled pattern rather than being re-derived here, which
/// is what keeps literal and regex queries highlighting the same spans they match.
fn highlight_matches(text: &str, ranges: &[(usize, usize)], current: bool) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return highlight_log_line(text);
    }
    let hit_style = Style::new()
        .bg(if current {
            theme::MATCH_CURRENT_BG
        } else {
            theme::MATCH_BG
        })
        .fg(Color::Black)
        .bold();

    let mut spans = Vec::with_capacity(ranges.len() * 2 + 1);
    let mut cursor = 0;
    for &(start, end) in ranges {
        // Defensive: a stale range against re-fetched text must not panic.
        if start < cursor
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            continue;
        }
        if start > cursor {
            spans.push(Span::raw(text[cursor..start].to_string()));
        }
        spans.push(Span::styled(text[start..end].to_string(), hit_style));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::raw(text[cursor..].to_string()));
    }
    spans
}

/// Tint a build log's severity prefix so errors stand out while scrolling.
///
/// Deliberately only the leading `[level]`/`level:` token: colouring matched
/// words anywhere would repaint file paths and message bodies, which is noisier
/// than no colour at all.
fn highlight_log_line(text: &str) -> Vec<Span<'static>> {
    let trimmed = text.trim_start();
    let indent = &text[..text.len() - trimmed.len()];

    const LEVELS: &[(&str, Color)] = &[
        ("[error]", theme::FAIL),
        ("[warn]", theme::WARN),
        ("[info]", theme::RUNNING),
        ("[success]", theme::OK),
        ("[debug]", theme::MUTED),
        ("error:", theme::FAIL),
        ("warning:", theme::WARN),
        ("error[", theme::FAIL),
    ];

    for (token, color) in LEVELS {
        if let Some(rest) = trimmed.strip_prefix(token) {
            return vec![
                Span::raw(indent.to_string()),
                Span::styled(token.to_string(), Style::new().fg(*color).bold()),
                Span::raw(rest.to_string()),
            ];
        }
    }
    vec![Span::raw(text.to_string())]
}

/// The longest prefix of `hints` that fits beside `content_width`.
///
/// Dropping from the end rather than swapping in one abbreviated string: the old
/// all-or-nothing form needed 108 columns for the status bar, so at 80 it fell
/// straight back to `? keys · q quit` and the keys that teach the tool — `/`, `g`,
/// `r`, `f` — vanished at the width most terminals actually are. Callers order
/// hints most-important first.
fn fitting_hints<'a>(
    total: usize,
    content_width: usize,
    hints: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    // Each entry renders as "  key label"; +2 keeps a gap from the content.
    let width = |(k, l): &(&str, &str)| k.chars().count() + l.chars().count() + 3;
    let budget = total.saturating_sub(content_width + 2);

    let mut used = 0;
    hints
        .iter()
        .take_while(|h| {
            used += width(h);
            used <= budget
        })
        .copied()
        .collect()
}

/// The slice of `lines` a pane of `visible` rows shows.
///
/// `pinned` names the newest line to display by its server-assigned logical
/// index; `None` tails the buffer. Returns a half-open range of *buffer
/// positions*, which differ from logical indices once scrollback evicts lines.
///
/// Extracted so the windowing is testable without a terminal: an off-by-one here
/// silently hides either the newest line or the oldest.
#[cfg(test)]
pub(crate) fn log_window_for_test(
    lines: &VecDeque<JobLine>,
    visible: usize,
    pinned: Option<usize>,
) -> (usize, usize) {
    log_window(lines, visible, pinned)
}

fn log_window(lines: &VecDeque<JobLine>, visible: usize, pinned: Option<usize>) -> (usize, usize) {
    let end = match pinned {
        // Position of the pinned line, one past it. A pin older than anything
        // held (evicted between update and render) falls back to the front.
        Some(pin) => lines
            .iter()
            .position(|l| l.index >= pin)
            .map_or(lines.len(), |p| p + 1),
        None => lines.len(),
    };
    (end.saturating_sub(visible), end)
}

/// The search field, plus how many lines match and how to move between them.
fn render_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    // While the field has focus, plain letters are query text, so the jump keys are
    // the Ctrl- forms — the hint has to say which mode you are in. Ordered by what
    // is least obvious: the jump keys are the ones nobody guesses.
    let hints: &[(&str, &str)] = if app.typing {
        &[("^N/^P", "next·prev"), ("↵", "keep"), ("Esc", "clear")]
    } else {
        &[
            ("n/N", "next·prev"),
            ("m", "matches only"),
            ("/", "edit"),
            ("Esc", "clear"),
        ]
    };

    let mut spans = vec![
        Span::styled(" /", Style::new().fg(theme::MATCH_BG).bold()),
        Span::styled(app.query.clone(), Style::new().fg(theme::TITLE).bold()),
    ];
    // A block cursor only while the field has focus, so an accepted query does not
    // look like it is still being edited.
    if app.typing {
        spans.push(Span::styled("▏", Style::new().fg(theme::MATCH_BG)));
    }

    if app.query.is_empty() {
        spans.push(Span::styled(
            "  type to search  ·  re: for regex",
            Style::new().fg(theme::MUTED).italic(),
        ));
    } else if let Some(err) = app.pattern_error() {
        // A bad pattern must say so: silently matching nothing is indistinguishable
        // from a log that genuinely has no hits.
        spans.push(Span::styled(
            format!("  ⚠ {err}"),
            Style::new().fg(theme::FAIL),
        ));
    } else {
        let hits = app.match_indices().len();
        // Which hit of how many, so n/N has a sense of position in the log.
        let at = app
            .cursor
            .and_then(|c| app.match_indices().iter().position(|&i| i == c))
            .map(|i| format!("{}/", i + 1))
            .unwrap_or_default();
        spans.push(Span::styled(
            format!("  {at}{hits} match{}", if hits == 1 { "" } else { "es" }),
            Style::new().fg(if hits == 0 { theme::WARN } else { theme::MUTED }),
        ));
        if app.filter_matches {
            spans.push(Span::styled(
                "  · matches only",
                Style::new().fg(theme::MATCH_BG),
            ));
        }
    }

    split_bar(frame, area, spans, hints);
}

/// Draw a one-line bar as content on the left and a key hint on the right.
///
/// Splitting the row is what keeps them apart: rendering both into the same full
/// width let the right-aligned hint paint over the content, which truncated the
/// match count mid-word on a narrow terminal. The hint gets its width first and
/// falls back to a shorter form, then drops entirely, since the content it would
/// otherwise erase is the answer the user is looking at.
fn split_bar(frame: &mut Frame, area: Rect, content: Vec<Span<'static>>, hints: &[(&str, &str)]) {
    let content_width: usize = content.iter().map(|s| s.content.chars().count()).sum();
    let kept = fitting_hints(area.width as usize, content_width, hints);

    if kept.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(content)), area);
        return;
    }

    // Rendered as key/label pairs so the key can be brighter than its description —
    // the key is what you act on.
    let spans: Vec<Span<'static>> = kept
        .iter()
        .flat_map(|(key, label)| {
            [
                Span::styled(format!("  {key}"), Style::new().fg(theme::TITLE).bold()),
                Span::styled(format!(" {label}"), Style::new().fg(theme::MUTED)),
            ]
        })
        .chain(std::iter::once(Span::raw(" ")))
        .collect();

    let hint_width: u16 = spans.iter().map(|s| s.content.chars().count() as u16).sum();
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(hint_width)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(content)), left);
    frame.render_widget(Paragraph::new(Line::from(spans).right_aligned()), right);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut left = Vec::new();
    if let Some(err) = &app.status_message {
        left.push(Span::styled(
            format!(" {err}"),
            Style::new().fg(theme::FAIL).bold(),
        ));
        // A message that vanishes on the next success is unreadable if it flashed
        // past, so always say where the retained copy is.
        left.push(Span::styled("  (e for log)", Style::new().fg(theme::MUTED)));
    } else if let Some(h) = &app.health {
        let running = app.jobs.iter().filter(|j| j.running).count();
        let idle = app
            .jobs
            .iter()
            .filter(|j| j.running && j.idle_ms > IDLE_WARN_MS)
            .count();
        left.push(Span::styled(
            format!(" sidecar v{}", h.version),
            Style::new().fg(theme::MUTED),
        ));
        // Say the feed is live: a monitor that has silently stopped listening is
        // indistinguishable from a quiet sidecar without it.
        if !app.ready {
            left.push(Span::styled("  •  ", Style::new().fg(theme::BORDER)));
            left.push(Span::styled(
                "reconnecting…",
                Style::new().fg(theme::WARN).italic(),
            ));
        }
        left.push(Span::styled("  •  ", Style::new().fg(theme::BORDER)));
        left.push(Span::styled(
            format!("{running} running"),
            Style::new().fg(if running > 0 {
                theme::RUNNING
            } else {
                theme::MUTED
            }),
        ));
        left.push(Span::styled(
            format!(" / {} total", app.jobs.len()),
            Style::new().fg(theme::MUTED),
        ));
        // Surface a wedged job here too: the offending row may be scrolled out of
        // view, and a silent job is the failure mode worth interrupting for.
        if idle > 0 {
            left.push(Span::styled("  •  ", Style::new().fg(theme::BORDER)));
            left.push(Span::styled(
                format!("{idle} idle"),
                Style::new().fg(theme::WARN).bold(),
            ));
        }
        // Errors outlive the message that announced them, so keep a standing
        // count — otherwise a failure that resolved itself leaves no trace and
        // the log nobody knows about goes unread.
        if !app.errors.is_empty() {
            left.push(Span::styled("  •  ", Style::new().fg(theme::BORDER)));
            left.push(Span::styled(
                format!("{} error", app.errors.len()),
                Style::new().fg(theme::FAIL).bold(),
            ));
            if app.errors.len() != 1 {
                left.push(Span::styled("s", Style::new().fg(theme::FAIL).bold()));
            }
            left.push(Span::styled(" (e)", Style::new().fg(theme::MUTED)));
        }
    } else {
        left.push(Span::styled(
            " connecting…",
            Style::new().fg(theme::MUTED).italic(),
        ));
    }

    // Ordered by what the user most needs to discover, because a narrow terminal
    // drops from the end: `?` first since it reveals everything else, then search
    // and the view toggles, with the keys people already guess (↑↓, q) last.
    split_bar(
        frame,
        area,
        left,
        &[
            ("?", "keys"),
            ("/", "search"),
            ("e", "errors"),
            ("s", "stats"),
            ("g", "group"),
            ("r", "running"),
            ("f", "follow"),
            ("↑↓", "select"),
            ("q", "quit"),
        ],
    );
}

fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    /// One key and what it does.
    fn row(key: &str, what: &str) -> Line<'static> {
        Line::from(vec![
            Span::raw("   "),
            Span::styled(format!("{key:<12}"), Style::new().fg(theme::TITLE).bold()),
            Span::styled(what.to_string(), Style::new().fg(theme::MUTED)),
        ])
    }

    let mut lines = vec![
        Line::from(""),
        section("JOBS"),
        row("↑ / k", "select previous"),
        row("↓ / j", "select next"),
        row("Home / End", "first / last"),
        Line::from(""),
        section("VIEW"),
        row("g", "group by session / flat"),
        row("r", "only running jobs"),
        Line::from(""),
        section("SEARCH"),
        row("/", "search, or edit the query"),
        row("re:…", "treat the query as a regex"),
        row("n / N", "next / previous match"),
        row("Ctrl-n/-p", "next / prev while typing"),
        row("m", "show only matching lines"),
        row("Enter", "keep query, leave the field"),
        row("Esc", "clear the search"),
        Line::from(""),
        section("OUTPUT"),
        row("wheel", "scroll the log"),
        row("PgUp/PgDn", "scroll a page"),
        row("Ctrl-u / -d", "scroll a page"),
        row("G", "jump to newest, resume tailing"),
        row("f", "follow / pause"),
        Line::from(""),
        section("GENERAL"),
        row("e", "error log for this session"),
        row("s", "recorded activity over time"),
        row("any key", "close this help"),
        row("q", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "   Read-only — never changes sidecar state.",
            Style::new().fg(theme::BORDER).italic(),
        )),
        // Mouse reporting takes over the terminal's own selection, so say how to
        // get it back rather than leaving copy/paste looking broken.
        Line::from(Span::styled(
            "   Hold Shift to select text with the mouse.",
            Style::new().fg(theme::BORDER).italic(),
        )),
        Line::from(""),
    ];

    // The panel is taller than a short terminal, and `centered_rect` clamps to the
    // frame — which silently cut the list off mid-section. Scroll it instead, so
    // every key stays reachable at any height.
    let popup = centered_rect(56, lines.len() as u16 + 2, area);
    let visible = popup.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let offset = (app.help_scroll as usize).min(max_scroll);
    let truncated = max_scroll > 0;

    let block = panel("Keys").title_top(badge(
        if truncated {
            "↑↓ scroll · any other key closes"
        } else {
            "any key closes"
        },
        theme::MUTED,
    ));

    if truncated {
        lines = lines.into_iter().skip(offset).collect();
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);

    if truncated {
        let mut state = ScrollbarState::new(max_scroll).position(offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_style(Style::new().fg(theme::BORDER)),
            popup,
            &mut state,
        );
    }
}

/// A centered `Rect` of fixed size, clamped to the frame.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::client::Outcome;

    /// Highlight `text` for `query`, compiling the query exactly as the app does.
    fn hl(text: &str, query: &str) -> Vec<Span<'static>> {
        let mut app = App::new();
        for c in query.chars() {
            app.update(crate::tui::app::Action::SearchInput(c));
        }
        highlight_matches(text, &app.match_ranges(text), false)
    }

    fn job(running: bool, exit_code: Option<i32>, outcome: Option<Outcome>) -> JobSummary {
        JobSummary {
            job_id: "j".into(),
            cmd: "sbt".into(),
            args: vec!["validate".into()],
            session_id: None,
            running,
            exit_code,
            line_count: 0,
            elapsed_ms: 0,
            idle_ms: 0,
            outcome,
        }
    }

    fn outcome(kind: &str, escalated: bool) -> Option<Outcome> {
        Some(Outcome {
            kind: kind.into(),
            escalated,
        })
    }

    /// A buffer of `n` lines whose logical indices start at `base`, mirroring a
    /// job whose earliest output has already been evicted.
    fn buf(base: usize, n: usize) -> VecDeque<JobLine> {
        (base..base + n)
            .map(|i| JobLine {
                index: i,
                text: format!("line {i}"),
                ts: 0,
            })
            .collect()
    }

    /// Tailing must end on the newest line. The first version of this pane
    /// rendered the buffer reversed, so multi-line output read bottom-to-top.
    #[test]
    fn tailing_shows_the_newest_lines_last() {
        let lines = buf(0, 100);
        assert_eq!(log_window(&lines, 8, None), (92, 100));
    }

    /// A pin names a line by logical index, and that line must be the last one
    /// shown — this is what stops a paused pane from drifting.
    #[test]
    fn a_pin_puts_its_line_at_the_bottom_of_the_window() {
        let lines = buf(0, 100);
        let (start, end) = log_window(&lines, 8, Some(50));
        assert_eq!((start, end), (43, 51));
        assert_eq!(lines[end - 1].index, 50, "pinned line is the newest shown");
    }

    /// Logical indices and buffer positions diverge once scrollback evicts lines;
    /// the window must be computed from the index, not assume they are equal.
    #[test]
    fn a_pin_is_resolved_against_logical_indices_not_positions() {
        // Lines 500..600 retained; earlier output evicted.
        let lines = buf(500, 100);
        let (start, end) = log_window(&lines, 8, Some(550));
        assert_eq!(lines[end - 1].index, 550);
        assert_eq!(end - start, 8, "a full pane of rows");
    }

    /// Scrolling past the oldest retained line would otherwise blank the pane.
    #[test]
    fn the_window_cannot_scroll_past_the_oldest_line() {
        let lines = buf(0, 100);
        assert_eq!(log_window(&lines, 8, Some(0)), (0, 1));
    }

    /// A pin older than anything still held (evicted between update and render)
    /// falls back to the front rather than showing an empty pane.
    #[test]
    fn a_pin_below_the_buffer_falls_back_to_the_oldest_line() {
        let lines = buf(500, 100);
        let (start, end) = log_window(&lines, 8, Some(10));
        assert_eq!((start, end), (0, 1));
    }

    /// A log shorter than the pane must render in full, not float or clip.
    #[test]
    fn a_short_log_shows_every_line() {
        let lines = buf(0, 3);
        assert_eq!(log_window(&lines, 8, None), (0, 3));
    }

    /// A match must be split out into its own span so it can be styled.
    #[test]
    fn a_match_is_isolated_into_its_own_span() {
        let spans = hl("before HIT after", "hit");
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["before ", "HIT", " after"]);
        assert!(
            spans[1].style.bg.is_some(),
            "the match carries the highlight"
        );
    }

    #[test]
    fn every_occurrence_is_highlighted() {
        let spans = hl("err a err b err", "err");
        let hits = spans.iter().filter(|s| s.style.bg.is_some()).count();
        assert_eq!(hits, 3);
    }

    /// A non-matching line still gets its severity prefix tinted.
    #[test]
    fn a_line_with_no_match_falls_back_to_severity_tinting() {
        let spans = hl("[error] boom", "zzz");
        // [indent, token, rest] — the indent span is empty here.
        assert_eq!(spans[1].content.as_ref(), "[error]");
        assert!(spans[1].style.fg.is_some());
    }

    /// Offsets come from a lowercased copy, so multi-byte text must not panic or
    /// slice mid-character.
    #[test]
    fn multibyte_lines_do_not_panic() {
        for (text, q) in [
            ("→ compiling café", "café"),
            ("ΑΒΓ match ΔΕΖ", "match"),
            ("日本語 error 文字", "error"),
            ("İstanbul", "i"),
        ] {
            let spans = hl(text, q);
            let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(joined, text, "reassembly must be lossless for {text:?}");
        }
    }

    #[test]
    fn severity_prefixes_are_tinted_only_at_the_start() {
        let spans = highlight_log_line("  [warn] deprecated");
        assert_eq!(spans.len(), 3, "indent, token, rest");
        assert_eq!(spans[1].content.as_ref(), "[warn]");
        // A level word mid-line is left alone: colouring it would repaint paths.
        let plain = highlight_log_line("path/to/error.rs compiled");
        assert_eq!(plain.len(), 1);
    }

    #[test]
    fn line_counts_are_abbreviated_past_a_thousand() {
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_500), "1.5k");
        assert_eq!(compact_count(64_275), "64.3k");
        assert_eq!(compact_count(250_000), "250k");
    }

    const BAR_HINTS: &[(&str, &str)] = &[
        ("?", "keys"),
        ("/", "search"),
        ("g", "group"),
        ("q", "quit"),
    ];

    /// The bug: both halves of the bar were drawn into the same full-width area,
    /// so the right-aligned hint painted over the match count — it truncated to
    /// "43 matche" on a narrow terminal.
    #[test]
    fn hints_never_overlap_the_content() {
        // Wide: everything fits.
        assert_eq!(fitting_hints(200, 20, BAR_HINTS).len(), BAR_HINTS.len());
        // Nothing fits: the content keeps the row to itself.
        assert!(fitting_hints(22, 20, BAR_HINTS).is_empty());
    }

    /// The old all-or-nothing form needed 108 columns for the status bar, so at 80
    /// it fell back to two hints and `/ search` disappeared at the width most
    /// terminals are. Hints must degrade one at a time.
    #[test]
    fn hints_drop_one_at_a_time_from_the_end() {
        let mut seen = Vec::new();
        for width in (24..200).step_by(4) {
            seen.push(fitting_hints(width, 20, BAR_HINTS).len());
        }
        seen.dedup();
        // Monotonic, and passes through intermediate counts rather than jumping
        // from all to one.
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
        assert!(seen.contains(&2), "an intermediate count exists: {seen:?}");
    }

    /// Callers order hints most-important first, so a truncated bar keeps the ones
    /// that teach the tool.
    #[test]
    fn the_most_important_hint_survives_longest() {
        for width in 24..200 {
            let kept = fitting_hints(width, 20, BAR_HINTS);
            if let Some(first) = kept.first() {
                assert_eq!(first.0, "?", "at {width} cols the first hint is dropped");
            }
        }
    }

    /// Exactly-fits must not round up into an overlap.
    #[test]
    fn a_hint_that_exactly_fits_is_kept() {
        let one: &[(&str, &str)] = &[("ab", "cd")];
        // "  ab cd" is 7 columns; +2 gap over 10 of content = 19.
        assert_eq!(fitting_hints(19, 10, one).len(), 1);
        assert_eq!(fitting_hints(18, 10, one).len(), 0);
    }

    #[test]
    fn duration_switches_units() {
        assert_eq!(format_duration(5_000), "5s");
        assert_eq!(format_duration(65_000), "1m5s");
        assert_eq!(format_duration(3_700_000), "1h1m");
    }

    /// Most one-shot calls finish in milliseconds, and `format_duration`'s floor
    /// renders every one of them as `0s` — which reads as "no data".
    #[test]
    fn sub_second_calls_keep_their_milliseconds() {
        assert_eq!(format_millis(8), "8ms");
        assert_eq!(format_millis(999), "999ms");
        assert_eq!(format_millis(1_500), "1s");
    }

    /// The job list is the primary pane, so the calls pane must never grow into
    /// it — a busy sidecar would otherwise squeeze the jobs off screen entirely.
    #[test]
    fn the_calls_pane_never_starves_the_job_list() {
        for available in 0..60u16 {
            let height = activity_pane_height(200, available);
            assert!(
                height == 0 || available.saturating_sub(height) >= 8,
                "at {available} rows the calls pane took {height}, leaving too little"
            );
        }
    }

    /// A short terminal gives the calls pane nothing rather than a border pair
    /// with no room for content.
    #[test]
    fn a_short_terminal_drops_the_calls_pane_entirely() {
        assert_eq!(activity_pane_height(5, 10), 0);
        assert_eq!(activity_pane_height(5, 8), 0);
        assert!(activity_pane_height(5, 20) > 0);
    }

    /// Two calls should not reserve eight rows of empty space.
    #[test]
    fn the_calls_pane_is_sized_to_its_content() {
        assert_eq!(activity_pane_height(1, 40), 3, "one row plus borders");
        assert_eq!(activity_pane_height(3, 40), 5);
        // Past the cap it is a tail, not an ever-growing list.
        assert_eq!(activity_pane_height(500, 40), 10);
    }

    fn call(id: u64, exit_code: Option<i32>, error: Option<&str>) -> Activity {
        Activity {
            id,
            kind: "exec".into(),
            cmd: "git".into(),
            args: vec!["status".into()],
            cwd: None,
            started_ms: 0,
            duration_ms: (exit_code.is_some() || error.is_some()).then_some(42),
            exit_code,
            error: error.map(str::to_string),
            session_id: None,
        }
    }

    /// A denied call has no exit code, so a duration is not the useful thing to
    /// show — the reason is.
    #[test]
    fn a_failed_call_shows_its_reason_instead_of_a_duration() {
        let line = activity_line(&call(1, None, Some("command not allowed: sudo")), 0);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("command not allowed"), "{text}");
        assert!(!text.contains("42ms"), "{text}");
        assert!(text.contains('✗'), "{text}");
    }

    #[test]
    fn a_successful_call_shows_its_duration() {
        let line = activity_line(&call(1, Some(0), None), 0);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("42ms"), "{text}");
        assert!(text.contains('✓'), "{text}");
    }

    /// A non-zero exit is a failure even with no error message, or a failing
    /// command would read as successful.
    #[test]
    fn a_nonzero_exit_reads_as_a_failure() {
        let line = activity_line(&call(1, Some(1), None), 0);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('✗'), "{text}");
    }

    /// An in-flight call spins, so a slow one is visibly working rather than
    /// looking like it silently succeeded.
    #[test]
    fn an_in_flight_call_spins() {
        let line = activity_line(&call(1, None, None), 0);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            SPINNER_FRAMES.iter().any(|f| text.contains(f)),
            "expected a spinner frame in {text:?}"
        );
    }

    /// The whole point of the readout: a silent running job must not look healthy.
    #[test]
    fn a_long_silence_outranks_running() {
        let mut j = job(true, None, None);
        j.idle_ms = IDLE_WARN_MS + 1;
        let (state, _) = job_state(&j, 0);
        assert!(state.contains("idle"), "{state}");

        j.idle_ms = 1_000;
        let (state, _) = job_state(&j, 0);
        assert!(state.contains("running"), "{state}");
    }

    /// A signalled process has no exit code, so reporting one would be a lie —
    /// the outcome is the only truthful thing to show.
    #[test]
    fn an_abnormal_outcome_is_named_instead_of_an_exit_code() {
        let j = job(false, None, outcome("idle_timed_out", false));
        let (state, _) = job_state(&j, 0);
        assert!(state.contains("idle timed out"), "{state}");
        assert!(!state.contains("exit"), "{state}");
    }

    #[test]
    fn an_escalated_kill_says_so() {
        let j = job(false, None, outcome("canceled", true));
        let (state, _) = job_state(&j, 0);
        assert!(state.contains("killed"), "{state}");
    }

    #[test]
    fn completed_jobs_report_their_exit_code() {
        let (ok, _) = job_state(&job(false, Some(0), outcome("completed", false)), 0);
        assert!(ok.contains("done"), "{ok}");
        let (bad, _) = job_state(&job(false, Some(2), outcome("completed", false)), 0);
        assert!(bad.contains("exit 2"), "{bad}");
    }

    /// A non-zero value must always be visible: a quiet day next to a busy one
    /// would otherwise round to an empty bar and read as no activity at all.
    #[test]
    fn a_small_nonzero_value_still_draws_something() {
        assert_eq!(bar(0, 500, 22), "", "zero draws nothing");
        assert!(
            !bar(1, 500, 22).is_empty(),
            "1 of 500 must still be visible"
        );
        assert!(!bar(1, u64::MAX, 22).is_empty(), "no underflow to empty");
    }

    #[test]
    fn a_full_value_fills_the_width() {
        assert_eq!(bar(10, 10, 8).chars().count(), 8);
        // Proportional in between.
        assert_eq!(bar(5, 10, 8).chars().count(), 4);
    }

    /// Widths feed a `{:<width$}` format, so an overlong bar would push the count
    /// column out of alignment.
    #[test]
    fn a_bar_never_exceeds_its_width() {
        for value in [0u64, 1, 7, 99, 1_000, u64::MAX] {
            for max in [1u64, 10, 1_000, u64::MAX] {
                let drawn = bar(value.min(max), max, 22);
                assert!(
                    drawn.chars().count() <= 22,
                    "bar({value}, {max}) drew {} cells",
                    drawn.chars().count()
                );
            }
        }
    }

    /// Weekday labels are derived rather than pulled from a date library, so the
    /// arithmetic needs pinning against known dates.
    #[test]
    fn weekdays_are_derived_correctly() {
        for (date, want) in [
            ("2026-09-18", "Fri"),
            ("2026-09-17", "Thu"),
            ("2026-01-01", "Thu"),
            ("2024-02-29", "Thu"),
            ("2000-03-01", "Wed"),
        ] {
            let bucket = crate::tui::client::DayBucket {
                date: date.into(),
                calls: 1,
                failures: 0,
            };
            assert_eq!(bucket.weekday(), want, "{date}");
        }
    }

    /// A malformed date must label its bar with something rather than panicking
    /// or indexing out of bounds.
    #[test]
    fn a_malformed_date_falls_back_to_itself() {
        for date in ["", "nonsense", "2026-13-01", "2026"] {
            let bucket = crate::tui::client::DayBucket {
                date: date.into(),
                calls: 1,
                failures: 0,
            };
            assert_eq!(bucket.weekday(), date, "{date} should fall back");
        }
    }

    /// A long command name must not push the bars out of their column.
    #[test]
    fn a_long_command_name_is_clipped() {
        assert_eq!(truncate("git", 10), "git");
        let clipped = truncate("some-very-long-binary", 10);
        assert_eq!(clipped.chars().count(), 10);
        assert!(clipped.ends_with('…'));
    }

    /// Render `app` into an off-screen terminal and return it as text, so the
    /// overlay can be asserted on without a tty.
    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal
            .draw(|frame| render(frame, app, 0))
            .expect("render must not fail");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn snapshot() -> crate::tui::client::StatsSnapshot {
        use crate::tui::client::{CommandStat, DayBucket, StatsSnapshot};
        StatsSnapshot {
            days: vec![
                DayBucket {
                    date: "2026-09-16".into(),
                    calls: 4,
                    failures: 0,
                },
                DayBucket {
                    date: "2026-09-17".into(),
                    calls: 120,
                    failures: 3,
                },
                DayBucket {
                    date: "2026-09-18".into(),
                    calls: 60,
                    failures: 0,
                },
            ],
            total_calls: 184,
            commands: vec![
                CommandStat {
                    cmd: "git".into(),
                    calls: 150,
                    p50_ms: Some(17),
                },
                CommandStat {
                    cmd: "sbt".into(),
                    calls: 34,
                    p50_ms: Some(42_100),
                },
            ],
        }
    }

    /// The overlay must actually draw its chart: labels, bars, counts, and the
    /// failure marker.
    #[test]
    fn the_stats_overlay_draws_its_chart() {
        let mut app = App::new();
        app.update(crate::tui::app::Action::ToggleStats);
        app.update(crate::tui::app::Action::StatsLoaded(Box::new(snapshot())));
        let screen = rendered(&mut app, 100, 30);
        // Printed so `cargo test -- --nocapture` shows the real layout: a chart
        // that passes its assertions can still look wrong.
        println!("\n{screen}\n");

        assert!(screen.contains("Stats"), "panel title missing:\n{screen}");
        assert!(screen.contains("CALLS / DAY"), "{screen}");
        assert!(screen.contains("Wed"), "weekday labels missing:\n{screen}");
        assert!(screen.contains('█'), "no bars drawn:\n{screen}");
        assert!(screen.contains("184 calls recorded"), "{screen}");
        assert!(screen.contains("TOP COMMANDS"), "{screen}");
        assert!(screen.contains("✗3"), "failure count missing:\n{screen}");
        // The busiest day's bar must be the longest one on screen.
        let busiest = screen.lines().find(|l| l.contains("Thu")).expect("Thu row");
        let quietest = screen.lines().find(|l| l.contains("Wed")).expect("Wed row");
        assert!(
            busiest.matches('█').count() > quietest.matches('█').count(),
            "120 calls should outdraw 4:\nbusiest: {busiest}\nquietest: {quietest}"
        );
    }

    /// The panel exists to be readable on a normal terminal, so it must fit 80
    /// columns without its numbers being clipped off the edge.
    #[test]
    fn the_stats_overlay_fits_an_eighty_column_terminal() {
        let mut app = App::new();
        app.update(crate::tui::app::Action::ToggleStats);
        app.update(crate::tui::app::Action::StatsLoaded(Box::new(snapshot())));
        let screen = rendered(&mut app, 80, 30);
        assert!(screen.contains("CALLS / DAY"), "{screen}");
        assert!(screen.contains("184 calls recorded"), "{screen}");
        // Counts must survive: a clipped bar would swallow them.
        assert!(screen.contains("120"), "{screen}");
    }

    /// Metrics can be unavailable (no writable directory). Saying so beats an
    /// empty panel that looks like "no activity".
    #[test]
    fn the_stats_overlay_reports_why_it_is_empty() {
        let mut app = App::new();
        app.update(crate::tui::app::Action::ToggleStats);
        app.update(crate::tui::app::Action::StatsFailed(
            "metrics are disabled".into(),
        ));
        let screen = rendered(&mut app, 100, 30);
        assert!(screen.contains("metrics are disabled"), "{screen}");
    }

    /// Before the fetch returns, the panel must say it is working rather than
    /// showing a blank box.
    #[test]
    fn the_stats_overlay_shows_a_loading_state() {
        let mut app = App::new();
        app.update(crate::tui::app::Action::ToggleStats);
        let screen = rendered(&mut app, 100, 30);
        assert!(screen.contains("loading"), "{screen}");
    }

    /// A first-run sidecar has a working store with nothing in it, which is not
    /// the same as a failure.
    #[test]
    fn the_stats_overlay_distinguishes_empty_from_broken() {
        let mut app = App::new();
        app.update(crate::tui::app::Action::ToggleStats);
        app.update(crate::tui::app::Action::StatsLoaded(Box::default()));
        let screen = rendered(&mut app, 100, 30);
        assert!(screen.contains("No calls recorded yet"), "{screen}");
    }

    /// The job list and log pane are what the user actually needs; the overlay is
    /// cosmetic and must not replace them permanently.
    #[test]
    fn closing_the_overlay_restores_the_job_panes() {
        let mut app = App::new();
        app.update(crate::tui::app::Action::ToggleStats);
        app.update(crate::tui::app::Action::StatsLoaded(Box::new(snapshot())));
        assert!(rendered(&mut app, 100, 30).contains("CALLS / DAY"));

        app.update(crate::tui::app::Action::ToggleStats);
        let screen = rendered(&mut app, 100, 30);
        assert!(
            !screen.contains("CALLS / DAY"),
            "overlay still up:\n{screen}"
        );
        // The pane is narrow enough that "COMMAND" is clipped, so match the panel
        // title rather than the column header.
        assert!(screen.contains("Jobs ("), "job pane missing:\n{screen}");
        assert!(screen.contains("Output"), "log pane missing:\n{screen}");
    }

    /// A terminal too short for the whole panel must scroll rather than silently
    /// cutting the chart off.
    #[test]
    fn a_short_terminal_scrolls_the_stats_panel() {
        let mut app = App::new();
        app.update(crate::tui::app::Action::ToggleStats);
        app.update(crate::tui::app::Action::StatsLoaded(Box::new(snapshot())));
        let short = rendered(&mut app, 100, 12);
        assert!(short.contains("scroll"), "no scroll hint:\n{short}");

        // Scrolling down must reveal content that was below the fold.
        app.update(crate::tui::app::Action::StatsScroll(6));
        let scrolled = rendered(&mut app, 100, 12);
        assert_ne!(short, scrolled, "scrolling changed nothing");
    }
}
