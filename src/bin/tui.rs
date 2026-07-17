use std::{
    io::Write as _,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use color_eyre::Result;
use futures::StreamExt;
use ratatui::crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{interval, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use claude_sidecar::tui::{
    app::{Action, App},
    client::{SidecarClient, StreamEvent},
    ui,
};

#[derive(Debug, Parser)]
#[command(
    name = "sidecar-tui",
    about = "Dashboard for the claude-sidecar daemon"
)]
struct Cli {
    /// Port the sidecar server is listening on.
    #[arg(short, long, default_value_t = 8765, env = "SIDECAR_PORT")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let client = SidecarClient::new(cli.port);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, client).await;
    ratatui::restore();
    result
}

async fn run(terminal: &mut ratatui::DefaultTerminal, client: SidecarClient) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
    let cancel = CancellationToken::new();

    // Background task: refresh job list and health every second.
    let refresh_tx = tx.clone();
    let refresh_client = client.clone();
    let refresh_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = refresh_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let jobs_result = refresh_client.list_jobs().await;
                    let health_result = refresh_client.health().await;
                    match (jobs_result, health_result) {
                        (Ok(jobs), Ok(health)) => {
                            let _ = refresh_tx.send(Action::RefreshJobs(jobs, health));
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            let _ = refresh_tx.send(Action::FetchError(e.to_string()));
                        }
                    }
                }
            }
        }
    });

    // Input events.
    let input_tx = tx.clone();
    let input_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut events = EventStream::new();
        loop {
            tokio::select! {
                _ = input_cancel.cancelled() => break,
                Some(Ok(event)) = events.next() => {
                    if let Some(action) = map_event(event) {
                        let _ = input_tx.send(action);
                    }
                }
            }
        }
    });

    let mut app = App::new();
    let mut tick: u64 = 0;
    let mut render_ticker = interval(Duration::from_millis(33)); // ~30 FPS
    render_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut sse_handle: Option<JoinHandle<()>> = None;
    let mut sse_cancel = CancellationToken::new();

    loop {
        tokio::select! {
            _ = render_ticker.tick() => {
                tick = tick.wrapping_add(1);

                // If selection changed, spawn a new SSE follower.
                let current_id = app.selected_job().map(|j| j.job_id.clone());
                if current_id != app.loaded_job_id {
                    // Cancel previous stream task.
                    sse_cancel.cancel();
                    if let Some(h) = sse_handle.take() {
                        h.abort();
                    }
                    sse_cancel = CancellationToken::new();
                    app.loaded_job_id = current_id.clone();

                    if let Some(job_id) = current_id {
                        let stream_tx = tx.clone();
                        let stream_client = client.clone();
                        let stream_cancel = sse_cancel.clone();
                        sse_handle = Some(tokio::spawn(async move {
                            let mut stream = Box::pin(stream_client.stream(&job_id));
                            loop {
                                tokio::select! {
                                    _ = stream_cancel.cancelled() => break,
                                    event = stream.next() => {
                                        match event {
                                            Some(Ok(StreamEvent::Line(line))) => {
                                                let _ = stream_tx.send(Action::AppendLogLine(line));
                                            }
                                            Some(Ok(StreamEvent::Exit { .. })) | None => {
                                                let _ = stream_tx.send(Action::LogStreamEnded);
                                                break;
                                            }
                                            Some(Err(_)) => break,
                                        }
                                    }
                                }
                            }
                        }));
                    }
                }

                terminal.draw(|f| ui::render(f, &mut app, tick))?;
            }

            Some(action) = rx.recv() => {
                match &action {
                    Action::Quit => {
                        cancel.cancel();
                        sse_cancel.cancel();
                        break;
                    }
                    Action::ConfirmKill => {
                        // Fire the kill request before updating app state.
                        if let Some(job) = app.selected_job() {
                            if job.running {
                                let kill_client = client.clone();
                                let kill_id = job.job_id.clone();
                                tokio::spawn(async move {
                                    let _ = kill_client.cancel(&kill_id).await;
                                });
                            }
                        }
                        app.update(action);
                    }
                    Action::SaveLog => {
                        save_log(&app);
                        app.update(action);
                    }
                    _ => {
                        app.update(action);
                    }
                }
            }
        }
    }

    Ok(())
}

fn map_event(event: Event) -> Option<Action> {
    let Event::Key(key) = event else {
        return None;
    };
    // Only react to key-press events (ignore release/repeat on some platforms).
    use ratatui::crossterm::event::KeyEventKind;
    if key.kind != KeyEventKind::Press {
        return None;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),

        // Job list navigation (no modifier = list nav; with modifier = log scroll on same keys).
        KeyCode::Char('j') if key.modifiers.is_empty() => Some(Action::SelectNext),
        KeyCode::Char('k') if key.modifiers.is_empty() => Some(Action::SelectPrev),
        KeyCode::Down if key.modifiers.is_empty() => Some(Action::SelectNext),
        KeyCode::Up if key.modifiers.is_empty() => Some(Action::SelectPrev),

        // Log scrolling.
        KeyCode::Char('J') => Some(Action::ScrollDown),
        KeyCode::Char('K') => Some(Action::ScrollUp),
        KeyCode::PageUp => Some(Action::ScrollPageUp),
        KeyCode::PageDown => Some(Action::ScrollPageDown),
        KeyCode::Char('g') if key.modifiers.is_empty() => Some(Action::SelectFirst),
        KeyCode::Char('G') => Some(Action::ScrollBottom),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPageUp)
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPageDown)
        }
        KeyCode::Char('f') => Some(Action::ToggleFollow),

        // Actions.
        KeyCode::Char('x') | KeyCode::Char('X') => Some(Action::RequestKill),
        KeyCode::Char('y') | KeyCode::Enter => Some(Action::ConfirmKill),
        KeyCode::Char('n') | KeyCode::Esc => Some(Action::CancelModal),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(Action::SaveLog),
        KeyCode::Char('?') => Some(Action::ToggleHelp),

        _ => None,
    }
}

fn save_log(app: &App) {
    let job = match app.selected_job() {
        Some(j) => j,
        None => return,
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = format!(
        "sidecar-{}-{ts}.log",
        job.job_id.chars().take(8).collect::<String>()
    );
    if let Ok(mut f) = std::fs::File::create(&filename) {
        // Write in chronological order (oldest first).
        let lines: Vec<_> = app.lines.iter().collect();
        for line in &lines {
            let _ = writeln!(f, "{}", line.text);
        }
    }
}
