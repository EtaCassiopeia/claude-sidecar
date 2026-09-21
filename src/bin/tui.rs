use std::io::IsTerminal;
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};
use std::time::Duration;

use clap::Parser;
use color_eyre::Result;
use futures::StreamExt;
use ratatui::crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{interval, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use claude_sidecar::tui::{
    app::{Action, App},
    client::{JobLine, ServerEvent, SidecarClient, StreamEvent},
    ui,
};

/// How often to reconcile against `/jobs` and `/health`.
///
/// A safety net, not the primary channel: `/events` pushes everything as it
/// happens, and this only repairs state after a lag or a reconnect. It was 1s
/// when polling *was* the channel, which is what made the display trail what the
/// sidecar was actually doing.
const RECONCILE: Duration = Duration::from_secs(15);
/// Delay before reopening a dropped event stream, so a restarting sidecar is not
/// hammered.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
/// Frame budget — the spinner and elapsed times need redrawing between events.
const FRAME: Duration = Duration::from_millis(33);

#[derive(Debug, Parser)]
#[command(
    name = "sidecar-tui",
    about = "Read-only monitor for the claude-sidecar daemon"
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

    // Refuse to start without a terminal to read. crossterm leaves its event
    // source unset when it cannot open the tty and then panics inside
    // `EventStream::new` ("reader source not set"), which is what a backgrounded
    // launch produced. A monitor that cannot read a key cannot be quit either, so
    // failing here with an explanation beats starting and dying.
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "sidecar-tui needs an interactive terminal: stdin is not a tty.\n\
             If you backgrounded it (`... &`), bring it to the foreground with `fg`,\n\
             or run it in its own terminal window."
        );
        std::process::exit(1);
    }

    let client = SidecarClient::new(cli.port);

    let mut terminal = ratatui::init();
    // Mouse reporting is not on by default. Enabling it costs the terminal's own
    // selection/copy behaviour, so it is switched off again on the way out —
    // including on the error path, or the shell inherits a mouse-reporting tty.
    let mouse = enable_mouse();
    // `ratatui::init` installs a panic hook that leaves the alternate screen, but
    // it knows nothing about the mouse capture we enabled ourselves — so a panic
    // dropped the user at a prompt that printed `35;46;1M…` for every mouse move.
    if mouse {
        install_mouse_panic_hook();
    }
    let result = run(&mut terminal, client, mouse).await;
    if mouse {
        disable_mouse();
    }
    // Drain *before* restoring: once raw mode is off, an unread keystroke belongs
    // to the shell and shows up at the next prompt — which is how the `q` that quit
    // the app ended up echoed. Reading with a zero timeout takes only what is
    // already buffered.
    drain_pending_input();
    ratatui::restore();
    result
}

/// Chain a mouse-disable onto the existing panic hook.
///
/// Wraps rather than replaces, so ratatui's own restore and color_eyre's report
/// both still run — this only adds the escape sequence they cannot know about.
fn install_mouse_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_mouse();
        previous(info);
    }));
}

/// Give the terminal back before the process stops, and take it again on resume.
///
/// Three signals stop us, and none of them run any cleanup on the way out:
///
/// * `SIGTSTP` — Ctrl-Z.
/// * `SIGTTIN` / `SIGTTOU` — a *background* process touching the terminal. This
///   is the one that bit in practice (`[1] + suspended (tty input)`): launched
///   from a shell that put us in the background, the first keyboard read stops
///   the process instantly.
///
/// In every case the default disposition stops us mid-render with raw mode and
/// mouse reporting still on, so the shell prompt comes back with tracking live
/// and prints `35;46;1M…` for every mouse move. The signals are intercepted, the
/// terminal restored, and the stop re-raised as `SIGSTOP` — which is not
/// catchable, so this cannot loop.
#[cfg(unix)]
fn spawn_suspend_handler(mouse: bool, cancel: &CancellationToken) {
    let cancel = cancel.clone();
    tokio::spawn(async move {
        // Best-effort: a missing handler makes suspension ugly, not fatal.
        let mut stops = Vec::new();
        for raw in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
            match signal(SignalKind::from_raw(raw)) {
                Ok(s) => stops.push(s),
                Err(e) => tracing::warn!("could not handle signal {raw}: {e}"),
            }
        }
        let Ok(mut cont) = signal(SignalKind::from_raw(libc::SIGCONT)) else {
            return;
        };
        if stops.is_empty() {
            return;
        }

        loop {
            // `select_all` over the stop signals, so one arm covers all three.
            let stop = futures::future::select_all(stops.iter_mut().map(|s| Box::pin(s.recv())));
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = stop => {
                    if mouse {
                        disable_mouse();
                    }
                    ratatui::restore();
                    // SIGSTOP rather than re-raising the original: that would be
                    // caught by this same handler and never actually stop us.
                    // SAFETY: raising a signal on our own process; no invariants.
                    unsafe { libc::raise(libc::SIGSTOP) };
                }
                _ = cont.recv() => {
                    // `fg`: retake the screen. The next frame redraws everything,
                    // so nothing needs repainting here.
                    let _ = ratatui::try_init();
                    if mouse {
                        enable_mouse();
                    }
                }
            }
        }
    });
}

/// Turn on mouse reporting, returning whether it took effect. Best-effort: a
/// terminal that refuses it should still get a working keyboard UI.
fn enable_mouse() -> bool {
    use ratatui::crossterm::{event::EnableMouseCapture, execute};
    execute!(std::io::stdout(), EnableMouseCapture).is_ok()
}

fn disable_mouse() {
    use ratatui::crossterm::{event::DisableMouseCapture, execute};
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
}

/// Discard anything still sitting in the terminal's input buffer.
fn drain_pending_input() {
    use ratatui::crossterm::event::{poll, read};
    // Bounded: a terminal being spammed with input must not keep us here.
    for _ in 0..256 {
        match poll(Duration::ZERO) {
            Ok(true) => {
                if read().is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    client: SidecarClient,
    mouse: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
    let cancel = CancellationToken::new();

    // Published for the input task: key meaning depends on whether the search
    // field has focus and which overlay is up, and that state lives in `App` on
    // this task.
    let typing = Arc::new(AtomicBool::new(false));
    let overlay = Arc::new(AtomicU8::new(Overlay::None as u8));

    spawn_refresh(&tx, &client, &cancel);
    spawn_events(&tx, &client, &cancel);
    spawn_input(&tx, &cancel, Arc::clone(&typing), Arc::clone(&overlay));
    #[cfg(unix)]
    spawn_suspend_handler(mouse, &cancel);
    #[cfg(not(unix))]
    let _ = mouse;
    let mut app = App::new();
    let mut tick: u64 = 0;
    let mut frames = interval(FRAME);
    frames.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // The log stream is torn down and reopened whenever the selection moves.
    let mut log_task: Option<JoinHandle<()>> = None;
    let mut log_cancel = CancellationToken::new();

    loop {
        tokio::select! {
            _ = frames.tick() => {
                tick = tick.wrapping_add(1);

                let selected = app.selected_job().map(|j| j.job_id.clone());
                if selected != app.loaded_job_id {
                    log_cancel.cancel();
                    if let Some(task) = log_task.take() {
                        task.abort();
                    }
                    log_cancel = CancellationToken::new();
                    app.loaded_job_id = selected.clone();
                    if let Some(job_id) = selected {
                        log_task = Some(spawn_log_stream(&tx, &client, &log_cancel, job_id));
                    }
                }

                typing.store(app.typing, Ordering::Relaxed);
                overlay.store(current_overlay(&app) as u8, Ordering::Relaxed);
                terminal.draw(|f| ui::render(f, &mut app, tick))?;
            }

            Some(action) = rx.recv() => {
                if matches!(action, Action::Quit) {
                    cancel.cancel();
                    log_cancel.cancel();
                    break;
                }
                let toggled_stats = matches!(action, Action::ToggleStats);
                app.update(action);
                // Load on open, and only on open. Nothing else consumes the
                // summary, so fetching it on the reconcile ticker would scan the
                // metrics archive every few seconds to feed a panel that is
                // closed almost all the time.
                if toggled_stats && app.show_stats {
                    spawn_stats_fetch(&tx, &client, &cancel);
                }
            }
        }
    }

    Ok(())
}

/// Reconcile against `/jobs` and `/health` on a slow interval.
///
/// `/events` is the live channel; this exists because that stream can lag or
/// drop, and a monitor must not stay wrong until the user restarts it. It also
/// supplies `/health`, which has no event of its own.
fn spawn_refresh(
    tx: &mpsc::UnboundedSender<Action>,
    client: &SidecarClient,
    cancel: &CancellationToken,
) {
    let (tx, client, cancel) = (tx.clone(), client.clone(), cancel.clone());
    tokio::spawn(async move {
        let mut ticker = interval(RECONCILE);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let action = match (client.list_jobs().await, client.health().await) {
                        (Ok(jobs), Ok(health)) => Action::RefreshJobs(jobs, health),
                        (Err(e), _) | (_, Err(e)) => Action::FetchError(e.to_string()),
                    };
                    if tx.send(action).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Fetch the metrics summary once, for a freshly opened stats overlay.
///
/// One shot rather than a ticker: see the call site. A failure becomes
/// `StatsFailed` so the panel says why it is empty — a metrics-disabled sidecar
/// is the common case, and silence there reads as a broken overlay.
fn spawn_stats_fetch(
    tx: &mpsc::UnboundedSender<Action>,
    client: &SidecarClient,
    cancel: &CancellationToken,
) {
    let (tx, client, cancel) = (tx.clone(), client.clone(), cancel.clone());
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => {}
            result = client.stats() => {
                let action = match result {
                    Ok(snapshot) => Action::StatsLoaded(Box::new(snapshot)),
                    Err(e) => Action::StatsFailed(e.to_string()),
                };
                let _ = tx.send(action);
            }
        }
    });
}

/// Follow the server-wide event stream, reopening it if it drops.
///
/// The reconnect loop is what makes this the primary channel rather than a
/// best-effort extra: a sidecar restart, or any dropped connection, otherwise
/// left the monitor frozen with no indication that it had stopped listening.
/// Each reopen replays current state, so a reconnect is self-healing.
fn spawn_events(
    tx: &mpsc::UnboundedSender<Action>,
    client: &SidecarClient,
    cancel: &CancellationToken,
) {
    let (tx, client, cancel) = (tx.clone(), client.clone(), cancel.clone());
    tokio::spawn(async move {
        loop {
            let mut stream = Box::pin(client.events());
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    event = stream.next() => match event {
                        Some(Ok(event)) => {
                            // A lag means events were lost; ask for the
                            // authoritative list rather than carrying on wrong.
                            let lagged = matches!(event, ServerEvent::Lagged { .. });
                            if tx.send(Action::Server(event)).is_err() {
                                return;
                            }
                            if lagged {
                                if let (Ok(jobs), Ok(health)) =
                                    (client.list_jobs().await, client.health().await)
                                {
                                    if tx.send(Action::RefreshJobs(jobs, health)).is_err() {
                                        return;
                                    }
                                }
                                // On a failed fetch the reconcile ticker retries.
                            }
                        }
                        Some(Err(e)) => {
                            if tx.send(Action::StreamLost(e.to_string())).is_err() {
                                return;
                            }
                            break;
                        }
                        None => break,
                    }
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            }
        }
    });
}

fn spawn_input(
    tx: &mpsc::UnboundedSender<Action>,
    cancel: &CancellationToken,
    typing: Arc<AtomicBool>,
    overlay: Arc<AtomicU8>,
) {
    let (tx, cancel) = (tx.clone(), cancel.clone());
    tokio::spawn(async move {
        let mut events = EventStream::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                Some(Ok(event)) = events.next() => {
                    let t = typing.load(Ordering::Relaxed);
                    let o = Overlay::from_u8(overlay.load(Ordering::Relaxed));
                    if let Some(action) = map_event(event, t, o) {
                        if tx.send(action).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// Follow one job's output until the stream ends or the selection moves on.
fn spawn_log_stream(
    tx: &mpsc::UnboundedSender<Action>,
    client: &SidecarClient,
    cancel: &CancellationToken,
    job_id: String,
) -> JoinHandle<()> {
    let (tx, client, cancel) = (tx.clone(), client.clone(), cancel.clone());
    tokio::spawn(async move {
        let mut stream = Box::pin(client.stream(&job_id));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = stream.next() => {
                    let line = match event {
                        Some(Ok(StreamEvent::Line(line))) => line,
                        // Show the break in the log itself: a silently short log
                        // reads as complete output when it is not.
                        Some(Ok(StreamEvent::Gap { dropped })) => JobLine {
                            index: 0,
                            text: format!("── {dropped} earlier lines dropped (buffer cap) ──"),
                            ts: 0,
                        },
                        // Exit, end of stream, or a decode error: nothing more is
                        // coming, and the job list already reports the outcome.
                        _ => break,
                    };
                    if tx.send(Action::AppendLogLine(line)).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Which full-screen overlay is up, if any. They share their key handling — any
/// key closes, arrows scroll — so the mode is one value rather than a flag each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Errors,
    Stats,
}

impl Overlay {
    /// Recover the mode from its published `u8`. An unknown value means no
    /// overlay, which is the mode that leaves every key working.
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Help,
            2 => Self::Errors,
            3 => Self::Stats,
            _ => Self::None,
        }
    }
}

/// The overlay `app` is currently showing. Ties break the same way the renderer
/// stacks them, so the keys always belong to the panel actually on screen.
fn current_overlay(app: &App) -> Overlay {
    if app.show_help {
        Overlay::Help
    } else if app.show_errors {
        Overlay::Errors
    } else if app.show_stats {
        Overlay::Stats
    } else {
        Overlay::None
    }
}

/// Translate a key press into an action.
///
/// Modes are checked outermost-first, because the same physical key means
/// different things in each: an overlay swallows everything so it can always be
/// dismissed, and `typing` routes printable keys into the query — otherwise
/// typing "quit" would quit.
fn map_event(event: Event, typing: bool, overlay: Overlay) -> Option<Action> {
    use ratatui::crossterm::event::{MouseEvent, MouseEventKind};

    // The wheel scrolls the log in every mode, including while typing a query:
    // reaching for the mouse to look around should not require leaving the field.
    if let Event::Mouse(MouseEvent { kind, .. }) = event {
        return match kind {
            MouseEventKind::ScrollUp => Some(Action::WheelUp),
            MouseEventKind::ScrollDown => Some(Action::WheelDown),
            _ => None,
        };
    }

    let Event::Key(key) = event else {
        return None;
    };
    // Ignore release/repeat, which some terminals also report.
    use ratatui::crossterm::event::KeyEventKind;
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // While an overlay is up, anything that is not an explicit quit closes it.
    // Leaving other keys live let `?` open a panel with no documented way out.
    if overlay != Overlay::None {
        let (scroll, close): (fn(i16) -> Action, Action) = match overlay {
            Overlay::Errors => (Action::ErrorScroll, Action::ToggleErrors),
            Overlay::Stats => (Action::StatsScroll, Action::ToggleStats),
            _ => (Action::HelpScroll, Action::ToggleHelp),
        };
        return match key.code {
            KeyCode::Char('c') if ctrl => Some(Action::Quit),
            KeyCode::Char('q' | 'Q') => Some(Action::Quit),
            // The panel is taller than a short terminal, so it has to be scrollable
            // without those keys also dismissing it.
            KeyCode::Up | KeyCode::Char('k') => Some(scroll(-1)),
            KeyCode::Down | KeyCode::Char('j') => Some(scroll(1)),
            KeyCode::PageUp => Some(scroll(-10)),
            KeyCode::PageDown => Some(scroll(10)),
            _ => Some(close),
        };
    }

    if typing {
        return match key.code {
            KeyCode::Esc => Some(Action::SearchCancel),
            KeyCode::Enter => Some(Action::SearchAccept),
            KeyCode::Backspace => Some(Action::SearchBackspace),
            KeyCode::Char('c') if ctrl => Some(Action::SearchCancel),
            // Navigation still works mid-query: refining a search means looking
            // around the hits, and the whole point of typing is to find them.
            KeyCode::PageUp => Some(Action::ScrollPageUp),
            KeyCode::PageDown => Some(Action::ScrollPageDown),
            KeyCode::Up => Some(Action::ScrollUp),
            KeyCode::Down => Some(Action::ScrollDown),
            KeyCode::Char('u') if ctrl => Some(Action::ScrollPageUp),
            KeyCode::Char('d') if ctrl => Some(Action::ScrollPageDown),
            KeyCode::Char('n') if ctrl => Some(Action::SearchNext),
            KeyCode::Char('p') if ctrl => Some(Action::SearchPrev),
            // Every printable key, so a query may contain q, g, r, f…
            KeyCode::Char(c) if plain => Some(Action::SearchInput(c)),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Char('q' | 'Q') => Some(Action::Quit),
        KeyCode::Char('c') if ctrl => Some(Action::Quit),

        KeyCode::Down | KeyCode::Char('j') if plain => Some(Action::SelectNext),
        KeyCode::Up | KeyCode::Char('k') if plain => Some(Action::SelectPrev),
        KeyCode::Home => Some(Action::SelectFirst),
        KeyCode::End => Some(Action::SelectLast),

        // View toggles.
        KeyCode::Char('g') if plain => Some(Action::ToggleGrouping),
        KeyCode::Char('r') if plain => Some(Action::ToggleRunningOnly),

        // Search.
        KeyCode::Char('/') => Some(Action::SearchStart),
        KeyCode::Char('n') if plain => Some(Action::SearchNext),
        KeyCode::Char('N') => Some(Action::SearchPrev),
        KeyCode::Char('m') if plain => Some(Action::ToggleFilterMatches),

        KeyCode::PageUp => Some(Action::ScrollPageUp),
        KeyCode::PageDown => Some(Action::ScrollPageDown),
        KeyCode::Char('u') if ctrl => Some(Action::ScrollPageUp),
        KeyCode::Char('d') if ctrl => Some(Action::ScrollPageDown),
        KeyCode::Char('G') => Some(Action::ScrollBottom),
        KeyCode::Char('f') if plain => Some(Action::ToggleFollow),

        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('e') if plain => Some(Action::ToggleErrors),
        KeyCode::Char('s') if plain => Some(Action::ToggleStats),
        KeyCode::Esc => Some(Action::SearchCancel),
        // `/` with a query already set reopens the field to edit it.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    /// Existing tests pass `help` as a bool; keep that reading while the real
    /// signature carries the richer overlay mode.
    fn map(event: Event, typing: bool, help: bool) -> Option<Action> {
        let overlay = if help { Overlay::Help } else { Overlay::None };
        map_event(event, typing, overlay)
    }

    /// `?` opened the overlay but nothing closed it: the plain-mode arm mapped Esc
    /// to SearchCancel and `?` to a toggle that the overlay never saw.
    #[test]
    fn any_key_closes_the_help_overlay() {
        for code in [
            KeyCode::Char('?'),
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Char('x'),
            KeyCode::Char(' '),
        ] {
            assert!(
                matches!(map(key(code), false, true), Some(Action::ToggleHelp)),
                "{code:?} must dismiss help"
            );
        }
    }

    /// The panel is taller than a short terminal, so it has to scroll — and those
    /// keys must not double as "close".
    #[test]
    fn arrows_scroll_the_help_overlay_instead_of_closing_it() {
        for (code, sign) in [
            (KeyCode::Down, 1),
            (KeyCode::Char('j'), 1),
            (KeyCode::PageDown, 1),
            (KeyCode::Up, -1),
            (KeyCode::Char('k'), -1),
            (KeyCode::PageUp, -1),
        ] {
            match map(key(code), false, true) {
                Some(Action::HelpScroll(d)) => assert_eq!(
                    d.signum(),
                    sign,
                    "{code:?} should scroll {}",
                    if sign > 0 { "down" } else { "up" }
                ),
                other => panic!("{code:?} should scroll help, got {other:?}"),
            }
        }
    }

    /// Quitting still has to work with the overlay up, or it becomes a trap.
    #[test]
    fn quit_still_works_from_the_help_overlay() {
        assert!(matches!(
            map(key(KeyCode::Char('q')), false, true),
            Some(Action::Quit)
        ));
        assert!(matches!(map(ctrl('c'), false, true), Some(Action::Quit)));
    }

    /// Printable keys must reach the query, or searching for "quit" would quit.
    #[test]
    fn printable_keys_go_into_the_query_while_typing() {
        for c in ['q', 'g', 'r', 'f', 'n', 'm', '/'] {
            assert!(
                matches!(
                    map(key(KeyCode::Char(c)), true, false),
                    Some(Action::SearchInput(_))
                ),
                "{c} must be query text"
            );
        }
    }

    /// The bug: `_ => None` swallowed navigation, so a paused search could not be
    /// scrolled to see earlier hits.
    #[test]
    fn navigation_still_works_while_typing() {
        for (code, want) in [
            (KeyCode::PageUp, "up"),
            (KeyCode::PageDown, "down"),
            (KeyCode::Up, "up"),
            (KeyCode::Down, "down"),
        ] {
            let got = map(key(code), true, false);
            let ok = match want {
                "up" => matches!(got, Some(Action::ScrollUp | Action::ScrollPageUp)),
                _ => matches!(got, Some(Action::ScrollDown | Action::ScrollPageDown)),
            };
            assert!(ok, "{code:?} must scroll while typing, got {got:?}");
        }
    }

    /// Plain n/N are query text while typing, so the jump keys are the Ctrl forms.
    #[test]
    fn ctrl_n_and_p_jump_between_matches_while_typing() {
        assert!(matches!(
            map(ctrl('n'), true, false),
            Some(Action::SearchNext)
        ));
        assert!(matches!(
            map(ctrl('p'), true, false),
            Some(Action::SearchPrev)
        ));
    }

    #[test]
    fn plain_n_jumps_between_matches_once_the_field_is_released() {
        assert!(matches!(
            map(key(KeyCode::Char('n')), false, false),
            Some(Action::SearchNext)
        ));
        assert!(matches!(
            map(
                Event::Key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)),
                false,
                false
            ),
            Some(Action::SearchPrev)
        ));
    }

    #[test]
    fn escape_leaves_the_search_field_without_quitting() {
        assert!(matches!(
            map(key(KeyCode::Esc), true, false),
            Some(Action::SearchCancel)
        ));
        assert!(matches!(
            map(key(KeyCode::Enter), true, false),
            Some(Action::SearchAccept)
        ));
    }

    fn wheel(kind: ratatui::crossterm::event::MouseEventKind) -> Event {
        use ratatui::crossterm::event::MouseEvent;
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Mouse reporting was never enabled, so the wheel did nothing at all.
    #[test]
    fn the_wheel_scrolls_the_log() {
        use ratatui::crossterm::event::MouseEventKind;
        assert!(matches!(
            map(wheel(MouseEventKind::ScrollUp), false, false),
            Some(Action::WheelUp)
        ));
        assert!(matches!(
            map(wheel(MouseEventKind::ScrollDown), false, false),
            Some(Action::WheelDown)
        ));
    }

    /// Reaching for the mouse mid-query should not require leaving the field.
    #[test]
    fn the_wheel_works_while_typing_a_query() {
        use ratatui::crossterm::event::MouseEventKind;
        assert!(matches!(
            map(wheel(MouseEventKind::ScrollUp), true, false),
            Some(Action::WheelUp)
        ));
    }

    /// A click is not a scroll; ignoring it avoids surprising jumps.
    #[test]
    fn other_mouse_events_are_ignored() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};
        assert!(map(wheel(MouseEventKind::Moved), false, false).is_none());
        assert!(map(wheel(MouseEventKind::Down(MouseButton::Left)), false, false).is_none());
    }

    /// Key releases are reported by some terminals and would double every press.
    #[test]
    fn key_releases_are_ignored() {
        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert!(map(release, false, false).is_none());
    }

    /// The errors overlay must be reachable, or a flashed-past message stays
    /// unreadable — the whole reason it is retained.
    #[test]
    fn e_opens_the_error_log() {
        assert!(matches!(
            map(key(KeyCode::Char('e')), false, false),
            Some(Action::ToggleErrors)
        ));
    }

    /// Both overlays share their key handling, so the errors panel must scroll
    /// and close the same way help does — and scroll *itself*, not help.
    #[test]
    fn the_errors_overlay_scrolls_and_closes_like_help() {
        let up = map_event(key(KeyCode::Up), false, Overlay::Errors);
        assert!(
            matches!(up, Some(Action::ErrorScroll(d)) if d < 0),
            "arrows must scroll the error log, got {up:?}"
        );
        assert!(matches!(
            map_event(key(KeyCode::Char('x')), false, Overlay::Errors),
            Some(Action::ToggleErrors)
        ));
        // Quitting must still work, or the panel is a trap.
        assert!(matches!(
            map_event(key(KeyCode::Char('q')), false, Overlay::Errors),
            Some(Action::Quit)
        ));
    }

    /// Regression: every layer of the stats overlay existed — the action, the
    /// app state, the renderer, the HTTP client — except a key that reached it,
    /// so the footer advertised `s` and `s` did nothing.
    #[test]
    fn s_opens_the_stats_overlay() {
        assert!(matches!(
            map(key(KeyCode::Char('s')), false, false),
            Some(Action::ToggleStats)
        ));
    }

    /// The stats panel must scroll *itself* and close *itself*. Before
    /// `Overlay::Stats` existed these fell through to the help arm, which
    /// scrolled the wrong offset and toggled the wrong flag — leaving the panel
    /// on screen and unscrollable.
    #[test]
    fn the_stats_overlay_scrolls_and_closes_itself() {
        let up = map_event(key(KeyCode::Up), false, Overlay::Stats);
        assert!(
            matches!(up, Some(Action::StatsScroll(d)) if d < 0),
            "arrows must scroll the stats panel, got {up:?}"
        );
        let down = map_event(key(KeyCode::Char('j')), false, Overlay::Stats);
        assert!(
            matches!(down, Some(Action::StatsScroll(d)) if d > 0),
            "j must scroll down, got {down:?}"
        );
        assert!(matches!(
            map_event(key(KeyCode::Char('x')), false, Overlay::Stats),
            Some(Action::ToggleStats)
        ));
        // Quitting must still work, or the panel is a trap.
        assert!(matches!(
            map_event(key(KeyCode::Char('q')), false, Overlay::Stats),
            Some(Action::Quit)
        ));
    }

    /// The input task learns which overlay is up through a published `u8`, so a
    /// variant that does not survive that round trip silently routes the panel's
    /// keys to the job list underneath it.
    #[test]
    fn stats_overlay_survives_the_u8_round_trip() {
        assert_eq!(Overlay::from_u8(Overlay::Stats as u8), Overlay::Stats);
        let mut app = App::new();
        app.update(Action::ToggleStats);
        assert_eq!(current_overlay(&app), Overlay::Stats);
        app.update(Action::ToggleStats);
        assert_eq!(current_overlay(&app), Overlay::None);
    }

    /// `s` is a command, not query text — searching for "sbt" must not open a
    /// panel mid-word.
    #[test]
    fn s_is_query_text_while_typing() {
        assert!(matches!(
            map(key(KeyCode::Char('s')), true, false),
            Some(Action::SearchInput('s'))
        ));
    }

    /// `e` is a command, not query text — searching for "error" must not open a
    /// panel mid-word.
    #[test]
    fn e_is_query_text_while_typing() {
        assert!(matches!(
            map(key(KeyCode::Char('e')), true, false),
            Some(Action::SearchInput('e'))
        ));
    }

    /// An unknown published value must leave every key working rather than
    /// trapping the user in a phantom overlay.
    #[test]
    fn an_unknown_overlay_value_means_no_overlay() {
        assert_eq!(Overlay::from_u8(99), Overlay::None);
        assert_eq!(Overlay::from_u8(Overlay::Help as u8), Overlay::Help);
        assert_eq!(Overlay::from_u8(Overlay::Errors as u8), Overlay::Errors);
    }
}
