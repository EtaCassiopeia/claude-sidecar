use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, Wrap,
    },
    Frame,
};

use super::app::{App, Mode};
use super::client::JobSummary;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(frame: &mut Frame, app: &mut App, tick: u64) {
    let area = frame.area();

    // Main split: left list | right log, plus bottom status bar.
    let [main_area, status_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let [list_area, log_area] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Fill(1)]).areas(main_area);

    render_job_list(frame, app, list_area, tick);
    render_log_pane(frame, app, log_area);
    render_status_bar(frame, app, status_area);

    // Overlays (rendered last so they appear on top).
    match app.mode {
        Mode::ConfirmKill => render_confirm_kill(frame, app, area),
        Mode::Help => render_help(frame, area),
        Mode::Normal => {}
    }
}

fn render_job_list(frame: &mut Frame, app: &mut App, area: Rect, tick: u64) {
    let header = Row::new(vec![
        Cell::from("Command").style(Style::new().bold()),
        Cell::from("State").style(Style::new().bold()),
        Cell::from("Exit").style(Style::new().bold()),
        Cell::from("Elapsed").style(Style::new().bold()),
        Cell::from("Lines").style(Style::new().bold()),
    ]);

    let rows: Vec<Row> = app.jobs.iter().map(|j| job_row(j, tick)).collect();

    let widths = [
        Constraint::Fill(1),
        Constraint::Length(9),
        Constraint::Length(4),
        Constraint::Length(8),
        Constraint::Length(6),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Jobs "),
        )
        .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn job_row(job: &JobSummary, tick: u64) -> Row<'static> {
    let cmd = if job.args.is_empty() {
        job.cmd.clone()
    } else {
        format!("{} {}", job.cmd, job.args.join(" "))
    };
    let cmd = if cmd.len() > 28 {
        format!("{}…", &cmd[..27])
    } else {
        cmd
    };

    let (state_str, state_style) = if job.running {
        let frame = SPINNER_FRAMES[(tick / 2 % SPINNER_FRAMES.len() as u64) as usize];
        (format!("{frame} running"), Style::new().fg(Color::Cyan))
    } else {
        match job.exit_code {
            Some(0) => ("✓ done".to_string(), Style::new().fg(Color::Green)),
            Some(_) => ("✗ failed".to_string(), Style::new().fg(Color::Red)),
            None => ("◌ done".to_string(), Style::new().fg(Color::Yellow)),
        }
    };

    let exit_str = job
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "-".to_string());

    let elapsed = format_elapsed(job.elapsed_ms);
    let lines = job.line_count.to_string();

    Row::new(vec![
        Cell::from(cmd),
        Cell::from(state_str).style(state_style),
        Cell::from(exit_str),
        Cell::from(elapsed),
        Cell::from(lines),
    ])
}

fn render_log_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let selected_cmd = app
        .selected_job()
        .map(|j| format!(" {} ", j.cmd))
        .unwrap_or_else(|| " Log ".to_string());

    let follow_indicator = if app.follow { " [follow]" } else { " [paused]" };
    let title = format!("{selected_cmd}{follow_indicator}");

    // Render log lines in reverse order (newest first) so offset 0 == tailing.
    // This avoids having to compute wrapped line heights to find the true bottom.
    let log_text: Vec<Line> = app
        .lines
        .iter()
        .rev()
        .map(|l| Line::from(l.text.as_str().to_owned()))
        .collect();

    let log_content = Text::from(log_text);
    let content_len = log_content.lines.len() as u16;

    let paragraph = Paragraph::new(log_content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(title),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));

    // Clamp scroll to content length so it doesn't scroll past EOF.
    let max_scroll = content_len.saturating_sub(area.height.saturating_sub(2));
    if app.scroll_offset > max_scroll {
        app.scroll_offset = max_scroll;
    }

    frame.render_widget(paragraph, area);

    // Vertical scrollbar on the right edge.
    let mut scrollbar_state =
        ScrollbarState::new(content_len as usize).position(app.scroll_offset as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        area,
        &mut scrollbar_state,
    );
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let health_part = app
        .health
        .as_ref()
        .map(|h| format!("sidecar v{}  {} job(s)", h.version, h.jobs));

    let msg = if let Some(err) = &app.status_message {
        Span::styled(err.as_str().to_owned(), Style::new().fg(Color::Red))
    } else if let Some(h) = health_part {
        Span::styled(h, Style::new().fg(Color::DarkGray))
    } else {
        Span::styled("connecting…", Style::new().fg(Color::DarkGray))
    };

    let help_hint = Span::styled(
        "  j/k select  ↑/↓ scroll  x kill  s save  f follow  ? help  q quit",
        Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
    );

    let line = Line::from(vec![msg, help_hint]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_confirm_kill(frame: &mut Frame, app: &App, area: Rect) {
    let job_label = app
        .selected_job()
        .map(|j| format!("`{}`", j.cmd))
        .unwrap_or_else(|| "this job".to_string());

    let text = Text::from(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Kill "),
            Span::styled(job_label, Style::new().bold().fg(Color::Red)),
            Span::raw("?"),
        ]),
        Line::from(""),
        Line::from("  y / Enter = confirm    n / Esc = cancel").dim(),
        Line::from(""),
    ]);

    let popup = centered_rect(44, 7, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Confirm Kill ")
                .title_style(Style::new().fg(Color::Red).bold()),
        ),
        popup,
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from("  Navigation").bold(),
        Line::from("  j / ↓    select next job"),
        Line::from("  k / ↑    select previous job"),
        Line::from("  g        first job"),
        Line::from("  G        last job"),
        Line::from(""),
        Line::from("  Log scrolling").bold(),
        Line::from("  ↑ / ↓    scroll log up / down"),
        Line::from("  PgUp/Dn  scroll page"),
        Line::from("  Ctrl-u/d half-page"),
        Line::from("  g / G    scroll to top / bottom"),
        Line::from("  f        toggle auto-follow"),
        Line::from(""),
        Line::from("  Actions").bold(),
        Line::from("  x        kill selected job"),
        Line::from("  s        save log to file"),
        Line::from("  q        quit"),
        Line::from("  ?        close help"),
        Line::from(""),
    ]);

    let popup = centered_rect(50, 22, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Help "),
        ),
        popup,
    );
}

/// Create a centered `Rect` of fixed size, clamped to the frame.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn format_elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}
