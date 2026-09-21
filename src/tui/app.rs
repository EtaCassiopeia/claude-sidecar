use std::collections::VecDeque;

use ratatui::widgets::TableState;

use super::client::{Activity, HealthInfo, JobLine, JobSummary, ServerEvent, StatsSnapshot};

/// A compiled search query.
///
/// Literal by default: a build log is full of `.`, `(`, and `[`, so treating the
/// query as a regex unprompted would turn ordinary text into a surprising pattern.
/// An explicit `re:` prefix opts in.
#[derive(Debug)]
pub enum Pattern {
    /// Case-insensitive substring, pre-lowered so matching does not re-allocate.
    Literal(String),
    Regex(regex::Regex),
}

impl Pattern {
    /// Compile `query`. `Ok(None)` means "no search"; `Err` carries a message for
    /// the user.
    fn parse(query: &str) -> Result<Option<Self>, String> {
        let Some(rest) = query.strip_prefix("re:") else {
            return Ok((!query.is_empty()).then(|| Self::Literal(query.to_lowercase())));
        };
        if rest.is_empty() {
            return Ok(None); // `re:` alone is a prefix in progress, not an error
        }
        // Case-insensitive to match the literal path, so toggling `re:` does not
        // silently change which lines hit.
        regex::RegexBuilder::new(rest)
            .case_insensitive(true)
            .build()
            .map(|r| Some(Self::Regex(r)))
            .map_err(|e| {
                // The full message is multi-line with a caret diagram; the last
                // non-empty line is the actual reason.
                e.to_string()
                    .lines()
                    .rfind(|l| !l.trim().is_empty())
                    .unwrap_or("invalid pattern")
                    .trim()
                    .to_string()
            })
    }

    fn is_match(&self, text: &str) -> bool {
        match self {
            Self::Literal(needle) => text.to_lowercase().contains(needle),
            Self::Regex(re) => re.is_match(text),
        }
    }

    /// Byte ranges of every match, for highlighting.
    fn ranges(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            Self::Literal(needle) => {
                let hay = text.to_lowercase();
                let mut out = Vec::new();
                let mut at = 0;
                while let Some(found) = hay[at..].find(needle) {
                    let (start, end) = (at + found, at + found + needle.len());
                    // Lowercasing can change byte length for non-ASCII; skip a hit
                    // whose offsets no longer land on char boundaries in the
                    // original rather than slicing mid-character.
                    if text.is_char_boundary(start) && text.is_char_boundary(end) {
                        out.push((start, end));
                    }
                    at = end;
                }
                out
            }
            // Empty matches (`a*`) would loop forever if we advanced by width.
            Self::Regex(re) => re
                .find_iter(text)
                .filter(|m| m.start() < m.end())
                .map(|m| (m.start(), m.end()))
                .collect(),
        }
    }
}

/// Maximum log lines retained in the TUI scrollback per job.
pub const MAX_SCROLLBACK: usize = 5_000;

/// Maximum one-shot calls kept on display. Matches the server's own ring, so the
/// list holds everything a replay can deliver and no more.
pub const MAX_ACTIVITIES: usize = 200;

/// Maximum retained errors. Enough to see a pattern; the newest are kept.
pub const MAX_ERRORS: usize = 100;

/// How far a page-scroll moves.
const PAGE: u16 = 20;

/// Lines per wheel notch. One line per notch reads as sluggish on a long log;
/// three is what terminals and pagers conventionally do.
const WHEEL: isize = 3;

/// Every message that can flow through the action channel.
#[derive(Debug)]
pub enum Action {
    Quit,
    /// One event from the server-wide `/events` stream — the primary channel.
    Server(ServerEvent),
    /// The event stream dropped. Shown in the status bar, since a monitor that
    /// has silently stopped updating is worse than one that says so.
    StreamLost(String),
    /// Reconciliation after a lag or reconnect: the authoritative job list and
    /// health snapshot, fetched rather than streamed.
    RefreshJobs(Vec<JobSummary>, HealthInfo),
    /// A connection or fetch error to display in the status bar.
    FetchError(String),
    /// Append one log line for the currently selected job.
    AppendLogLine(JobLine),
    SelectNext,
    SelectPrev,
    SelectFirst,
    SelectLast,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollBottom,
    /// One notch of the mouse wheel, which moves further than an arrow key.
    WheelUp,
    WheelDown,
    ToggleFollow,
    ToggleHelp,
    /// Show the retained error log, so an error that flashed past is readable.
    ToggleErrors,
    /// Show the durable metrics overlay.
    ToggleStats,
    /// The fetched metrics summary, or why it could not be read.
    StatsLoaded(Box<StatsSnapshot>),
    StatsFailed(String),
    /// Scroll the stats overlay.
    StatsScroll(i16),
    /// Scroll the error log.
    ErrorScroll(i16),
    /// Switch between one flat list and jobs nested under session headers.
    ToggleGrouping,
    /// Switch between showing every job and only the running ones.
    ToggleRunningOnly,
    /// Scroll the help overlay, for terminals too short to show it all.
    HelpScroll(i16),
    /// Begin typing a search query.
    SearchStart,
    /// Append a character to the query.
    SearchInput(char),
    /// Delete the last character of the query.
    SearchBackspace,
    /// Keep the query and its highlighting, but stop capturing keystrokes.
    SearchAccept,
    /// Abandon the search and clear the highlighting.
    SearchCancel,
    /// Jump to the next/previous line matching the query.
    SearchNext,
    SearchPrev,
    /// Show only matching lines.
    ToggleFilterMatches,
}

/// One line of the job list. Grouped mode interleaves headers with jobs, so the
/// rendered rows and the job list are no longer the same thing — selection has to
/// move over jobs while headers are drawn but never selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A session heading: its short id (or `None` for jobs with no session), plus
    /// how many jobs and how many of those are running.
    Header {
        session: Option<String>,
        jobs: usize,
        running: usize,
    },
    /// Index into `App::jobs`.
    Job(usize),
}

impl Row {
    pub fn job_index(&self) -> Option<usize> {
        match self {
            Row::Job(i) => Some(*i),
            Row::Header { .. } => None,
        }
    }
}

pub struct App {
    pub jobs: Vec<JobSummary>,
    /// Display rows derived from `jobs` by the current grouping and filter.
    /// Rebuilt on every refresh and on every toggle, so it never drifts.
    pub rows: Vec<Row>,
    pub table_state: TableState,
    pub lines: VecDeque<JobLine>,
    /// The job ID whose SSE stream is currently loaded.
    pub loaded_job_id: Option<String>,
    /// Newest log line the pane should show, as the server's logical line index.
    /// `None` follows the tail.
    ///
    /// An index rather than a distance from the end: the tail moves as output
    /// arrives, so "8 lines back from newest" slid forward at exactly the rate
    /// lines came in — a paused view drifted instead of holding still.
    pub pinned: Option<usize>,
    /// Rows of log the pane last rendered. Recorded by the renderer because a
    /// page-scroll's size depends on the terminal, which the state cannot know.
    pub log_rows: usize,
    pub show_help: bool,
    /// First visible line of the help overlay, for terminals too short to show it
    /// all at once.
    pub help_scroll: u16,
    /// Group jobs under session headers rather than showing one flat list.
    pub grouped: bool,
    /// Hide jobs that have finished.
    pub running_only: bool,
    /// What the user typed. A `re:` prefix makes the rest a regex; otherwise it is
    /// a case-insensitive substring.
    pub query: String,
    /// The compiled form of `query`, rebuilt whenever it changes. `Err` holds the
    /// message to show — an invalid pattern must say so rather than silently
    /// matching nothing.
    pub pattern: Result<Option<Pattern>, String>,
    /// True while keystrokes go to the query rather than to commands.
    pub typing: bool,
    /// Show only lines matching `query`.
    pub filter_matches: bool,
    /// The match `n`/`N` last landed on, so repeated presses advance from the hit
    /// rather than from the window's bottom edge.
    pub cursor: Option<usize>,
    pub health: Option<HealthInfo>,
    pub status_message: Option<String>,
    /// Every error seen this session, newest last, with a wall-clock stamp.
    ///
    /// `status_message` shows one line and is cleared by the next success, so an
    /// error that flashed past was unreadable and unrecoverable — there is no
    /// server-side log to go back to. This retains them for the `e` overlay.
    pub errors: VecDeque<ErrorEntry>,
    /// Show the retained error log.
    pub show_errors: bool,
    pub error_scroll: u16,
    /// Show the durable metrics overlay.
    pub show_stats: bool,
    pub stats_scroll: u16,
    /// The last fetched metrics summary. `None` while the first fetch is in
    /// flight, which is what the overlay reports as "loading".
    pub stats: Option<StatsSnapshot>,
    /// Why the metrics could not be read. Shown in place of the charts, since a
    /// sidecar built without a writable metrics directory has none.
    pub stats_error: Option<String>,
    /// Server diagnostic ids already recorded, so a reconnect's replay does not
    /// duplicate every one of them.
    seen_diagnostics: std::collections::HashSet<u64>,
    /// Recent one-shot calls, oldest first. Kept separate from `jobs` because the
    /// two are genuinely different: an activity has no output stream, no pid, and
    /// nothing to select into the log pane.
    pub activities: VecDeque<Activity>,
    /// True once the opening replay has been applied, so "connecting…" clears at
    /// the right moment rather than on the first arbitrary event.
    pub ready: bool,
}

/// One retained error, stamped so a recurring failure can be told from a single
/// old one and correlated with what the user was doing.
#[derive(Debug, Clone)]
pub struct ErrorEntry {
    /// Local `HH:MM:SS`.
    pub at: String,
    pub message: String,
    /// How many times this same message repeated consecutively. A stream
    /// reconnecting every 10s would otherwise bury everything else.
    pub count: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            jobs: Vec::new(),
            rows: Vec::new(),
            table_state: TableState::default(),
            lines: VecDeque::new(),
            loaded_job_id: None,
            pinned: None,
            log_rows: 0,
            show_help: false,
            help_scroll: 0,
            grouped: false,
            running_only: false,
            query: String::new(),
            // No query yet, which is a valid state rather than an error.
            pattern: Ok(None),
            typing: false,
            filter_matches: false,
            cursor: None,
            health: None,
            status_message: None,
            errors: VecDeque::new(),
            show_errors: false,
            error_scroll: 0,
            show_stats: false,
            stats_scroll: 0,
            stats: None,
            stats_error: None,
            seen_diagnostics: std::collections::HashSet::new(),
            activities: VecDeque::new(),
            ready: false,
        }
    }
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the log pane tracking the newest line?
    pub fn following(&self) -> bool {
        self.pinned.is_none()
    }

    /// Logical index of the oldest and newest lines held, if any.
    fn line_bounds(&self) -> Option<(usize, usize)> {
        Some((self.lines.front()?.index, self.lines.back()?.index))
    }

    /// Is the match filter actually narrowing the view?
    fn filtering(&self) -> bool {
        self.filter_matches && !self.query.is_empty()
    }

    /// Logical indices of the lines currently on display, oldest first.
    ///
    /// Scrolling has to step through *these*, not the whole buffer: with the match
    /// filter on, a pin taken from the full buffer names a line the pane never
    /// draws, so the view appeared stuck while the pin crawled through hidden
    /// lines one keypress at a time.
    fn visible_indices(&self) -> Vec<usize> {
        self.lines
            .iter()
            .filter(|l| !self.filtering() || self.matches(&l.text))
            .map(|l| l.index)
            .collect()
    }

    /// Stop following, pinning the view exactly where it currently sits.
    ///
    /// The pin names the *newest visible* line, and a tailing pane's newest
    /// visible line is the newest line — so pausing must not shift the view.
    fn pin(&mut self) -> Option<usize> {
        let (_, newest) = self.line_bounds()?;
        Some(*self.pinned.get_or_insert(newest))
    }

    /// Scroll by `delta` lines, negative for older. Reaching the newest line
    /// resumes following, so returning to the bottom does not leave the pane
    /// frozen one line short of live output.
    fn scroll(&mut self, delta: isize, visible: usize) {
        self.cursor = None;
        let rows = self.visible_indices();
        if rows.is_empty() {
            return;
        }
        let newest = *rows.last().expect("non-empty");
        let current = match self.pin() {
            Some(pin) => pin,
            None => return,
        };

        // Move by rows of the display, not by logical index: with the filter on
        // those differ by however many lines are hidden between two matches.
        let at = rows
            .iter()
            .position(|&i| i >= current)
            .unwrap_or(rows.len() - 1);
        // The pin names the newest visible row, so it can never sit closer to the
        // front than a full screen — that would leave blank rows above the text.
        let floor = visible.saturating_sub(1).min(rows.len() - 1);
        let target = at.saturating_add_signed(delta).clamp(floor, rows.len() - 1);

        self.pinned = if rows[target] >= newest {
            None
        } else {
            Some(rows[target])
        };
    }

    /// Does `text` match the query? False when there is no query, or the pattern
    /// failed to compile, so callers need not special-case either.
    pub fn matches(&self, text: &str) -> bool {
        match &self.pattern {
            Ok(Some(p)) => p.is_match(text),
            _ => false,
        }
    }

    /// Every match in `text` as byte ranges, for highlighting.
    pub fn match_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        match &self.pattern {
            Ok(Some(p)) => p.ranges(text),
            _ => Vec::new(),
        }
    }

    /// The error to display, if the query does not compile.
    pub fn pattern_error(&self) -> Option<&str> {
        self.pattern.as_ref().err().map(String::as_str)
    }

    /// Recompile after the query changed. Cheap enough per keystroke, and doing it
    /// here keeps every read of `pattern` consistent with `query`.
    fn recompile(&mut self) {
        self.pattern = Pattern::parse(&self.query);
        self.cursor = None;
    }

    /// Logical indices of every matching line, oldest first.
    pub fn match_indices(&self) -> Vec<usize> {
        self.lines
            .iter()
            .filter(|l| self.matches(&l.text))
            .map(|l| l.index)
            .collect()
    }

    /// Move the view to the next match after the cursor (or before it, going
    /// back), wrapping at the ends so repeated presses cycle.
    fn jump_match(&mut self, forward: bool) {
        let hits = self.match_indices();
        if hits.is_empty() {
            return;
        }
        // The cursor is the match itself, not the pin: the pin is the *bottom* of
        // the window, which sits below the match once it is centred, and searching
        // from there would re-find the same hit.
        let from = self
            .cursor
            .or(self.pinned)
            .or_else(|| self.line_bounds().map(|(_, n)| n));
        let Some(from) = from else { return };

        let target = if forward {
            hits.iter().find(|&&i| i > from).or_else(|| hits.first())
        } else {
            hits.iter()
                .rev()
                .find(|&&i| i < from)
                .or_else(|| hits.last())
        };
        let Some(&hit) = target else { return };
        self.cursor = Some(hit);
        self.reveal(hit);
    }

    /// Pin the view so `line` is visible with context around it.
    ///
    /// Centring rather than pinning the match itself: the pin names the newest
    /// visible line, so pinning the hit put it flush against the bottom edge with
    /// nothing after it — and for an early match the window was clamped to the
    /// front of the buffer, leaving most of the pane blank. That read as "the log
    /// was cleared".
    fn reveal(&mut self, line: usize) {
        let rows = self.visible_indices();
        if rows.is_empty() {
            return;
        }
        let visible = self.log_rows.max(1);
        let at = rows.iter().position(|&i| i >= line).unwrap_or(0);
        // Put the hit a bit above centre so following context is visible, which is
        // the direction you read.
        let below = visible / 3;
        let bottom = (at + below).min(rows.len() - 1);
        let floor = visible.saturating_sub(1).min(rows.len() - 1);
        let bottom = bottom.max(floor);

        let newest = *rows.last().expect("non-empty");
        self.pinned = if rows[bottom] >= newest {
            None
        } else {
            Some(rows[bottom])
        };
    }

    /// The currently selected job, if any. Headers are never selectable, so a
    /// selected row always resolves to a job.
    pub fn selected_job(&self) -> Option<&JobSummary> {
        // `get`, not indexing: between assigning `self.jobs` and rebuilding
        // `rows`, the rows still hold indices into the previous, possibly longer
        // list. Indexing there panicked when a refresh dropped a job.
        self.selected_job_index().and_then(|i| self.jobs.get(i))
    }

    fn selected_job_index(&self) -> Option<usize> {
        self.table_state
            .selected()
            .and_then(|r| self.rows.get(r))
            .and_then(Row::job_index)
    }

    pub fn update(&mut self, action: Action) {
        match action {
            Action::Quit => {}

            Action::Server(event) => self.apply(event),

            Action::StreamLost(msg) => {
                self.ready = false;
                let text = format!("Event stream lost: {msg} — reconnecting");
                self.record_error(text.clone());
                self.status_message = Some(text);
            }

            Action::RefreshJobs(jobs, health) => {
                // Capture the anchor before swapping the list: afterwards the
                // rows index into the old `jobs`, so the cursor would resolve to
                // whatever job now happens to sit at that position.
                let anchor = self.selected_job().map(|j| j.job_id.clone());
                self.jobs = jobs;
                self.health = Some(health);
                self.status_message = None;
                self.rebuild_rows(anchor);
            }

            Action::FetchError(msg) => {
                let text = format!("Error: {msg}");
                self.record_error(text.clone());
                self.status_message = Some(text);
            }

            Action::AppendLogLine(line) => {
                self.lines.push_back(line);
                while self.lines.len() > MAX_SCROLLBACK {
                    self.lines.pop_front();
                }
                // A pinned view can be scrolled off the front by eviction. Drag it
                // forward to the oldest line still held rather than letting the
                // window silently clamp, which would look like drift again.
                if self.pinned.is_some() {
                    self.clamp_pin();
                }
            }

            Action::SelectNext => self.step(1),
            Action::SelectPrev => self.step(-1),
            Action::SelectFirst => self.jump(true),
            Action::SelectLast => self.jump(false),

            Action::ToggleGrouping => {
                self.grouped = !self.grouped;
                let anchor = self.selected_job().map(|j| j.job_id.clone());
                self.rebuild_rows(anchor);
            }

            Action::ToggleRunningOnly => {
                self.running_only = !self.running_only;
                let anchor = self.selected_job().map(|j| j.job_id.clone());
                self.rebuild_rows(anchor);
            }

            Action::ScrollUp => self.scroll(-1, self.log_rows),
            Action::ScrollDown => self.scroll(1, self.log_rows),
            Action::WheelUp => self.scroll(-WHEEL, self.log_rows),
            Action::WheelDown => self.scroll(WHEEL, self.log_rows),
            Action::ScrollPageUp => self.scroll(-(PAGE as isize), self.log_rows),
            Action::ScrollPageDown => self.scroll(PAGE as isize, self.log_rows),
            Action::ScrollBottom => self.pinned = None,

            Action::ToggleFollow => {
                if self.following() {
                    self.pin();
                } else {
                    self.pinned = None;
                }
            }

            Action::SearchStart => {
                self.typing = true;
                self.show_help = false;
            }

            Action::SearchInput(c) => {
                self.query.push(c);
                // Editing the query invalidates whichever hit we were sitting on.
                self.recompile();
                // Typing pins the view: a live tail would scroll the match the
                // user is looking for straight off the screen.
                self.pin();
            }

            Action::SearchBackspace => {
                self.query.pop();
                self.recompile();
            }

            Action::SearchAccept => self.typing = false,

            Action::SearchCancel => {
                self.typing = false;
                self.query.clear();
                self.filter_matches = false;
                self.recompile();
            }

            Action::SearchNext => self.jump_match(true),
            Action::SearchPrev => self.jump_match(false),

            Action::ToggleFilterMatches => {
                self.filter_matches = !self.filter_matches;
                // The row set just changed size; a pin valid a moment ago may now
                // name a hidden line.
                if self.pinned.is_some() {
                    self.clamp_pin();
                }
            }

            Action::ToggleHelp => {
                self.show_help = !self.show_help;
                self.help_scroll = 0;
                // Two overlays at once would stack unreadably.
                if self.show_help {
                    self.show_errors = false;
                    self.show_stats = false;
                }
            }

            Action::ToggleErrors => {
                self.show_errors = !self.show_errors;
                self.error_scroll = 0;
                if self.show_errors {
                    self.show_help = false;
                    self.show_stats = false;
                }
            }

            Action::ToggleStats => {
                self.show_stats = !self.show_stats;
                self.stats_scroll = 0;
                if self.show_stats {
                    self.show_help = false;
                    self.show_errors = false;
                }
            }

            Action::StatsLoaded(snapshot) => {
                self.stats = Some(*snapshot);
                self.stats_error = None;
            }

            Action::StatsFailed(message) => {
                // Deliberately not `record_error`: a sidecar with metrics
                // disabled would otherwise add an entry every time the overlay
                // is opened, burying the failures that matter.
                self.stats_error = Some(message);
            }

            Action::StatsScroll(delta) => {
                self.stats_scroll = self.stats_scroll.saturating_add_signed(delta);
            }

            Action::ErrorScroll(delta) => {
                self.error_scroll = self.error_scroll.saturating_add_signed(delta);
            }

            Action::HelpScroll(delta) => {
                self.help_scroll = self.help_scroll.saturating_add_signed(delta);
            }
        }
    }

    /// Fold one server event into the view.
    ///
    /// Every case is idempotent and keyed by id, which is what lets the stream's
    /// opening replay overlap its live events harmlessly — a much easier property
    /// to hold than an exact handoff boundary.
    fn apply(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::JobCreated {
                job_id,
                cmd,
                args,
                session_id,
            } => {
                if self.job_position(&job_id).is_some() {
                    return; // replayed
                }
                self.jobs.push(JobSummary {
                    job_id,
                    cmd,
                    args,
                    session_id,
                    running: true,
                    exit_code: None,
                    line_count: 0,
                    elapsed_ms: 0,
                    idle_ms: 0,
                    outcome: None,
                });
                self.resort();
            }

            ServerEvent::JobProgress {
                job_id,
                line_count,
                elapsed_ms,
                idle_ms,
            } => {
                let Some(i) = self.job_position(&job_id) else {
                    return;
                };
                let job = &mut self.jobs[i];
                // A progress event that arrives after the terminal one (replay
                // overlap) must not resurrect a finished job.
                if job.outcome.is_some() {
                    return;
                }
                job.line_count = line_count;
                job.elapsed_ms = elapsed_ms;
                job.idle_ms = idle_ms;
            }

            ServerEvent::JobFinished {
                job_id,
                outcome,
                exit_code,
                elapsed_ms,
                line_count,
            } => {
                let Some(i) = self.job_position(&job_id) else {
                    return;
                };
                let job = &mut self.jobs[i];
                job.running = false;
                job.outcome = Some(outcome);
                job.exit_code = exit_code;
                job.elapsed_ms = elapsed_ms;
                job.line_count = line_count;
                // Ordering puts running jobs first, so a job ending moves it.
                self.resort();
            }

            ServerEvent::JobEvicted { job_id } => {
                let Some(i) = self.job_position(&job_id) else {
                    return;
                };
                self.jobs.remove(i);
                self.resort();
            }

            ServerEvent::ActivityStarted { activity }
            | ServerEvent::ActivityFinished { activity } => {
                match self.activities.iter_mut().find(|a| a.id == activity.id) {
                    // Replace rather than append: a call is one row that gains an
                    // outcome, not two rows.
                    Some(existing) => *existing = activity,
                    None => self.activities.push_back(activity),
                }
                while self.activities.len() > MAX_ACTIVITIES {
                    self.activities.pop_front();
                }
            }

            ServerEvent::Diagnostic { diagnostic } => {
                // Keyed by the server's id so the replay's copies are not
                // re-recorded on every reconnect.
                if self.seen_diagnostics.contains(&diagnostic.id) {
                    return;
                }
                self.seen_diagnostics.insert(diagnostic.id);
                self.record_error(format!(
                    "[sidecar {}] {}",
                    diagnostic.level.to_lowercase(),
                    diagnostic.message
                ));
            }

            ServerEvent::Ready => {
                self.ready = true;
                self.status_message = None;
            }

            // No history to replay from, so the honest response is to say so and
            // let the event loop reconcile against `/jobs`.
            ServerEvent::Lagged { dropped } => {
                let text = format!("Fell behind by {dropped} events — resyncing");
                self.record_error(text.clone());
                self.status_message = Some(text);
            }
        }
    }

    fn job_position(&self, job_id: &str) -> Option<usize> {
        self.jobs.iter().position(|j| j.job_id == job_id)
    }

    /// Retain an error so it survives the next success.
    ///
    /// A repeat of the newest entry bumps its count instead of appending: a
    /// stream reconnecting on a loop would otherwise fill the log with one
    /// message and push out everything that actually differs.
    fn record_error(&mut self, message: String) {
        if let Some(last) = self.errors.back_mut() {
            if last.message == message {
                last.count += 1;
                last.at = crate::logger::now_hms();
                return;
            }
        }
        self.errors.push_back(ErrorEntry {
            at: crate::logger::now_hms(),
            message,
            count: 1,
        });
        while self.errors.len() > MAX_ERRORS {
            self.errors.pop_front();
        }
    }

    /// Rebuild the display rows, holding the cursor on the same job.
    fn resort(&mut self) {
        let anchor = self.selected_job().map(|j| j.job_id.clone());
        self.rebuild_rows(anchor);
    }

    /// How many one-shot calls are still in flight.
    pub fn activities_running(&self) -> usize {
        self.activities.iter().filter(|a| a.running()).count()
    }

    /// Keep a pin on a line the pane can actually draw, a full screen clear of the
    /// oldest row. Called after eviction and after the filter changes, either of
    /// which can leave the pin pointing below the visible set.
    fn clamp_pin(&mut self) {
        let rows = self.visible_indices();
        let Some(pin) = self.pinned else { return };
        if rows.is_empty() {
            self.pinned = None;
            return;
        }
        let floor_pos = self.log_rows.saturating_sub(1).min(rows.len() - 1);
        let floor = rows[floor_pos];
        let newest = *rows.last().expect("non-empty");
        if pin >= newest {
            self.pinned = None;
        } else if pin < floor {
            self.pinned = Some(floor);
        }
    }

    /// Rebuild `rows` from `jobs` under the current grouping and filter, keeping
    /// the cursor on the job named by `anchor`.
    ///
    /// Selection is tracked by `job_id` rather than row number: rows shift as jobs
    /// start, finish, and get filtered out, so a remembered index would silently
    /// slide onto a different job — or onto a header.
    fn rebuild_rows(&mut self, anchor: Option<String>) {
        // Running first, then longest-running: a monitor should surface live work
        // without the user hunting for it.
        let mut order: Vec<usize> = (0..self.jobs.len())
            .filter(|&i| !self.running_only || self.jobs[i].running)
            .collect();
        order.sort_by_key(|&i| {
            let job = &self.jobs[i];
            (!job.running, std::cmp::Reverse(job.elapsed_ms))
        });

        self.rows = if self.grouped {
            group_rows(&self.jobs, &order)
        } else {
            order.into_iter().map(Row::Job).collect()
        };

        // Re-anchor: same job if it is still visible, else the first job in view.
        let restored = anchor.and_then(|id| {
            self.rows
                .iter()
                .position(|r| r.job_index().is_some_and(|i| self.jobs[i].job_id == id))
        });
        let target = restored.or_else(|| self.rows.iter().position(|r| r.job_index().is_some()));
        if target != self.table_state.selected() {
            self.table_state.select(target);
            // The visible job changed under the cursor, so the log pane must not
            // keep showing the previous job's output.
            self.reset_log();
        }
    }

    /// Move the cursor by `delta` selectable rows, skipping headers.
    fn step(&mut self, delta: isize) {
        let selectable: Vec<usize> = self.selectable_rows();
        if selectable.is_empty() {
            return;
        }
        let current = self.table_state.selected();
        let pos = current
            .and_then(|r| selectable.iter().position(|&s| s == r))
            .unwrap_or(0);
        let next = pos.saturating_add_signed(delta).min(selectable.len() - 1);
        self.select_row(selectable[next]);
    }

    fn jump(&mut self, first: bool) {
        let selectable = self.selectable_rows();
        let target = if first {
            selectable.first()
        } else {
            selectable.last()
        };
        if let Some(&row) = target {
            self.select_row(row);
        }
    }

    fn selectable_rows(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.job_index().is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// Point the cursor at `row`, resetting the log pane if the job changed.
    fn select_row(&mut self, row: usize) {
        if Some(row) == self.table_state.selected() {
            return; // already there — keep the log and scroll position
        }
        self.table_state.select(Some(row));
        self.reset_log();
    }

    fn reset_log(&mut self) {
        self.lines.clear();
        self.pinned = None;
        self.cursor = None;
        // Dropping the loaded id makes the event loop notice the mismatch and
        // reopen the stream against the new selection.
        self.loaded_job_id = None;
    }
}

/// Interleave session headers with their jobs, preserving `order` within each
/// group. Sessions are ordered by their first appearance in `order`, so the
/// session with the longest-running live job leads. Jobs with no session id are
/// collected under a single trailing group.
fn group_rows(jobs: &[JobSummary], order: &[usize]) -> Vec<Row> {
    let mut sessions: Vec<Option<String>> = Vec::new();
    for &i in order {
        let key = jobs[i].session_short().map(str::to_string);
        if !sessions.contains(&key) {
            sessions.push(key);
        }
    }
    // Unattributed jobs last: they are the least useful thing to lead with.
    sessions.sort_by_key(Option::is_none);

    let mut rows = Vec::with_capacity(order.len() + sessions.len());
    for session in sessions {
        let members: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| jobs[i].session_short().map(str::to_string) == session)
            .collect();
        rows.push(Row::Header {
            session: session.clone(),
            jobs: members.len(),
            running: members.iter().filter(|&&i| jobs[i].running).count(),
        });
        rows.extend(members.into_iter().map(Row::Job));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::client::Outcome;

    fn job(id: &str, running: bool) -> JobSummary {
        JobSummary {
            job_id: id.to_string(),
            cmd: "sbt".into(),
            args: vec!["validate".into()],
            session_id: None,
            running,
            exit_code: None,
            line_count: 0,
            elapsed_ms: 0,
            idle_ms: 0,
            outcome: None,
        }
    }

    /// A job belonging to `session`, with an explicit runtime so ordering is
    /// deterministic.
    fn job_in(id: &str, running: bool, session: &str, elapsed_ms: u64) -> JobSummary {
        JobSummary {
            session_id: Some(session.to_string()),
            elapsed_ms,
            ..job(id, running)
        }
    }

    fn health() -> HealthInfo {
        HealthInfo {
            status: "ok".into(),
            version: "3".into(),
            jobs: 0,
        }
    }

    fn line(text: &str) -> JobLine {
        JobLine {
            index: 0,
            text: text.into(),
            ts: 0,
        }
    }

    /// Feed `n` lines with sequential logical indices, as the server assigns them.
    fn feed(app: &mut App, n: usize) {
        let next = app.lines.back().map_or(0, |l| l.index + 1);
        for i in next..next + n {
            app.update(Action::AppendLogLine(JobLine {
                index: i,
                text: format!("line {i}"),
                ts: 0,
            }));
        }
    }

    /// Type a query the way the user does, so the pattern is compiled.
    fn search(app: &mut App, query: &str) {
        app.update(Action::SearchStart);
        for c in query.chars() {
            app.update(Action::SearchInput(c));
        }
        app.update(Action::SearchAccept);
    }

    /// Logical index of the newest visible line, or None while tailing.
    fn newest_shown(app: &App, visible: usize) -> Option<usize> {
        let (_, end) = crate::tui::ui::log_window_for_test(&app.lines, visible, app.pinned);
        app.lines.get(end.checked_sub(1)?).map(|l| l.index)
    }

    /// The list re-sorts as jobs start and finish, so tracking the selection by
    /// index would silently move the cursor onto a different job.
    #[test]
    fn selection_follows_the_job_not_the_row() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![job("a", true), job("b", true)],
            health(),
        ));
        app.update(Action::SelectNext);
        assert_eq!(app.selected_job().unwrap().job_id, "b");

        // "b" is now first in the refreshed list.
        app.update(Action::RefreshJobs(
            vec![job("b", true), job("a", true)],
            health(),
        ));
        assert_eq!(app.selected_job().unwrap().job_id, "b");
    }

    #[test]
    fn a_vanished_selection_falls_back_to_the_top() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![job("a", true), job("b", true)],
            health(),
        ));
        app.update(Action::SelectNext);
        app.update(Action::RefreshJobs(vec![job("a", true)], health()));
        assert_eq!(app.selected_job().unwrap().job_id, "a");
    }

    #[test]
    fn an_empty_list_selects_nothing() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(vec![job("a", true)], health()));
        app.update(Action::RefreshJobs(vec![], health()));
        assert!(app.selected_job().is_none());
    }

    /// Switching jobs must not leave the previous job's output on screen.
    #[test]
    fn changing_selection_clears_the_log() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![job("a", true), job("b", true)],
            health(),
        ));
        app.update(Action::AppendLogLine(line("from a")));
        app.update(Action::ScrollUp);
        app.update(Action::SelectNext);
        assert!(app.lines.is_empty());
        assert!(app.following(), "a fresh job starts tailing");
    }

    /// Re-pressing `k` at the top would otherwise wipe the log and reset scroll.
    #[test]
    fn selecting_the_same_job_preserves_the_log() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(vec![job("a", true)], health()));
        app.update(Action::AppendLogLine(line("keep me")));
        app.update(Action::SelectPrev);
        assert_eq!(app.lines.len(), 1);
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut app = App::new();
        for i in 0..MAX_SCROLLBACK + 50 {
            app.update(Action::AppendLogLine(line(&i.to_string())));
        }
        assert_eq!(app.lines.len(), MAX_SCROLLBACK);
        assert_eq!(
            app.lines.back().unwrap().text,
            (MAX_SCROLLBACK + 49).to_string(),
            "the newest line must survive; the oldest is dropped"
        );
    }

    /// Scrolling up pauses the tail; returning to the bottom resumes it, so the
    /// user never has to know `f` exists.
    #[test]
    fn scrolling_away_pauses_follow_and_returning_resumes_it() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 100);

        app.update(Action::ScrollUp);
        assert!(!app.following());
        app.update(Action::ScrollDown);
        assert!(app.following(), "reaching the newest line resumes tailing");

        app.update(Action::ScrollPageUp);
        assert!(!app.following());
        app.update(Action::ScrollBottom);
        assert!(app.following());
    }

    /// The bug this replaced: the window was "N lines back from newest", so it
    /// slid forward as output arrived and a paused pane drifted at exactly the
    /// rate lines came in. A pause must hold the same lines.
    #[test]
    fn a_paused_view_does_not_drift_as_output_arrives() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 100);

        app.update(Action::ScrollPageUp);
        let before = newest_shown(&app, 10).expect("a visible line");
        feed(&mut app, 40);
        let after = newest_shown(&app, 10).expect("a visible line");
        assert_eq!(before, after, "paused view must stay on the same line");
    }

    /// Following must keep showing the newest line, not freeze where it started.
    #[test]
    fn following_tracks_new_output() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 20);
        assert!(app.following());
        feed(&mut app, 5);
        assert_eq!(newest_shown(&app, 10), Some(24), "tail follows the newest");
    }

    /// `f` was decorative: it flipped the label without moving the view.
    #[test]
    fn toggling_follow_actually_pins_and_releases() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 100);

        app.update(Action::ToggleFollow);
        assert!(!app.following(), "f pauses");
        let held = newest_shown(&app, 10);
        feed(&mut app, 30);
        assert_eq!(newest_shown(&app, 10), held, "pause holds position");

        app.update(Action::ToggleFollow);
        assert!(app.following(), "f resumes");
        assert_eq!(newest_shown(&app, 10), Some(129), "and snaps to newest");
    }

    /// Pausing then scrolling up must not jump: the first pin keeps the lines
    /// already on screen.
    #[test]
    fn pausing_keeps_the_lines_already_on_screen() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 50);
        app.update(Action::ToggleFollow);
        assert_eq!(
            newest_shown(&app, 10),
            Some(49),
            "pinning at the tail shows the same newest line"
        );
    }

    /// Scrollback eviction can drop the pinned line; the view must move forward
    /// to the oldest line still held rather than silently clamping.
    #[test]
    fn a_pin_evicted_from_scrollback_is_dragged_forward() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 50);
        app.update(Action::ScrollPageUp);
        let pinned = app.pinned.expect("paused");
        // Overflow the buffer well past the pinned line.
        feed(&mut app, MAX_SCROLLBACK + 100);
        let now = app.pinned.expect("still paused");
        assert!(now > pinned, "pin moved forward with the buffer");
        assert!(
            now >= app.lines.front().unwrap().index,
            "pin is within the retained buffer"
        );
    }

    // ─── search ──────────────────────────────────────────────────────────────

    fn feed_texts(app: &mut App, texts: &[&str]) {
        let next = app.lines.back().map_or(0, |l| l.index + 1);
        for (n, t) in texts.iter().enumerate() {
            app.update(Action::AppendLogLine(JobLine {
                index: next + n,
                text: (*t).to_string(),
                ts: 0,
            }));
        }
    }

    // ─── query syntax ────────────────────────────────────────────────────────

    /// Spaces are ordinary characters, not a token separator.
    #[test]
    fn a_literal_query_matches_spaces() {
        let mut app = App::new();
        search(&mut app, "failed to resolve");
        assert!(app.matches("[error] failed to resolve dependency"));
        assert!(!app.matches("[error] failed"), "not a word-set match");
    }

    /// Regex metacharacters are literal unless `re:` opts in — build logs are full
    /// of dots and brackets, so the reverse default would be surprising.
    #[test]
    fn metacharacters_are_literal_without_the_prefix() {
        let mut app = App::new();
        search(&mut app, "module_1.scala");
        assert!(app.matches("compiling module_1.scala"));
        assert!(
            !app.matches("compiling module_1Xscala"),
            "the dot is literal"
        );
    }

    #[test]
    fn the_re_prefix_enables_regex() {
        let mut app = App::new();
        search(&mut app, r"re:module_\d+\.scala");
        assert!(app.matches("compiling module_42.scala"));
        assert!(!app.matches("compiling module_x.scala"));
    }

    #[test]
    fn regex_alternation_and_anchors_work() {
        let mut app = App::new();
        search(&mut app, "re:^\\[(warn|error)\\]");
        assert!(app.matches("[error] boom"));
        assert!(app.matches("[warn] careful"));
        assert!(!app.matches("[info] fine"));
        assert!(
            !app.matches("prefixed [error] boom"),
            "anchored to the start"
        );
    }

    /// Both paths ignore case, so adding `re:` must not change which lines hit.
    #[test]
    fn regex_matching_is_case_insensitive_like_the_literal_path() {
        let mut app = App::new();
        search(&mut app, "re:ERROR");
        assert!(app.matches("[error] boom"));
    }

    /// A bad pattern must report itself; silently matching nothing looks the same
    /// as a log with no hits.
    #[test]
    fn an_invalid_regex_reports_an_error_and_matches_nothing() {
        let mut app = App::new();
        search(&mut app, "re:error(");
        assert!(app.pattern_error().is_some(), "an error is surfaced");
        assert!(!app.matches("[error] boom"));
        assert!(app.match_indices().is_empty());
    }

    /// `re:` alone is a prefix mid-typing, not a mistake.
    #[test]
    fn the_bare_prefix_is_not_an_error() {
        let mut app = App::new();
        search(&mut app, "re:");
        assert!(app.pattern_error().is_none());
        assert!(!app.matches("anything"), "and matches nothing yet");
    }

    /// An empty-width match (`x*`) would loop forever if ranges advanced by width.
    #[test]
    fn a_regex_that_can_match_nothing_terminates() {
        let mut app = App::new();
        search(&mut app, "re:x*");
        let ranges = app.match_ranges("axbxc");
        assert!(ranges.iter().all(|(s, e)| s < e), "no empty ranges");
    }

    #[test]
    fn ranges_cover_every_literal_occurrence() {
        let mut app = App::new();
        search(&mut app, "err");
        let text = "err a ERR b Err";
        assert_eq!(app.match_ranges(text).len(), 3, "case-insensitive");
    }

    /// Case-insensitive, like a terminal's find.
    #[test]
    fn matching_ignores_case() {
        let mut app = App::new();
        search(&mut app, "ERROR");
        assert!(app.matches("[error] boom"));
        assert!(!app.matches("[info] fine"));
    }

    /// An empty query must match nothing, or every line would highlight.
    #[test]
    fn an_empty_query_matches_nothing() {
        let app = App::new();
        assert!(!app.matches("anything"));
    }

    #[test]
    fn typing_builds_and_backspace_trims_the_query() {
        let mut app = App::new();
        app.update(Action::SearchStart);
        assert!(app.typing, "keys go to the query while typing");
        for c in "err".chars() {
            app.update(Action::SearchInput(c));
        }
        assert_eq!(app.query, "err");
        app.update(Action::SearchBackspace);
        assert_eq!(app.query, "er");
        app.update(Action::SearchAccept);
        assert!(!app.typing, "Enter releases the field but keeps the query");
        assert_eq!(app.query, "er");
    }

    #[test]
    fn cancelling_search_clears_the_query_and_filter() {
        let mut app = App::new();
        app.update(Action::SearchStart);
        app.update(Action::SearchInput('x'));
        app.update(Action::ToggleFilterMatches);
        app.update(Action::SearchCancel);
        assert!(app.query.is_empty());
        assert!(!app.typing);
        assert!(!app.filter_matches);
    }

    /// Searching while tailing would let new output scroll the match away, so
    /// typing pins the view.
    #[test]
    fn typing_a_query_pauses_the_tail() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 50);
        assert!(app.following());
        app.update(Action::SearchStart);
        app.update(Action::SearchInput('1'));
        assert!(!app.following(), "search pins the view");
    }

    #[test]
    fn n_and_shift_n_cycle_through_matches() {
        let mut app = App::new();
        app.log_rows = 4;
        feed_texts(
            &mut app,
            &["a", "hit one", "b", "hit two", "c", "hit three", "d"],
        );
        search(&mut app, "hit");
        assert_eq!(app.match_indices(), vec![1, 3, 5]);

        // Starts from the newest line, so the first next-match wraps to the top.
        // `cursor` is the hit; `pinned` is the window edge, which sits below it.
        app.update(Action::SearchNext);
        assert_eq!(app.cursor, Some(1));
        app.update(Action::SearchNext);
        assert_eq!(app.cursor, Some(3));
        app.update(Action::SearchPrev);
        assert_eq!(app.cursor, Some(1));
    }

    /// Repeated presses must cycle rather than stick at the last hit.
    #[test]
    fn searching_past_the_last_match_wraps() {
        let mut app = App::new();
        app.log_rows = 4;
        feed_texts(&mut app, &["hit", "x", "hit", "y"]);
        search(&mut app, "hit");
        app.cursor = Some(2); // on the final match
        app.update(Action::SearchNext);
        assert_eq!(app.cursor, Some(0), "wrapped to the first match");
    }

    /// The bug: jumping pinned the hit itself, and the pin is the *bottom* of the
    /// window — so an early match clamped the window to the front of the buffer
    /// and the pane showed a handful of lines instead of a full screen. It read as
    /// if the log had been cleared.
    #[test]
    fn jumping_to_an_early_match_still_fills_the_pane() {
        let mut app = App::new();
        app.log_rows = 20;
        // One hit near the very start of a long log.
        let mut texts: Vec<String> = (0..500).map(|i| format!("line {i}")).collect();
        texts[7] = "the hit".to_string();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        feed_texts(&mut app, &refs);

        search(&mut app, "the hit");
        app.update(Action::SearchNext);

        let (start, end) = crate::tui::ui::log_window_for_test(&app.lines, 20, app.pinned);
        assert_eq!(end - start, 20, "a full pane of rows, not a stub");
        // And the hit is inside it.
        let shown: Vec<usize> = app
            .lines
            .iter()
            .skip(start)
            .take(end - start)
            .map(|l| l.index)
            .collect();
        assert!(shown.contains(&7), "the match is visible: {shown:?}");
    }

    /// A match must land with lines after it, not flush against the bottom edge.
    #[test]
    fn a_jumped_match_has_context_below_it() {
        let mut app = App::new();
        app.log_rows = 12;
        let mut texts: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
        texts[100] = "the hit".to_string();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        feed_texts(&mut app, &refs);

        search(&mut app, "the hit");
        app.update(Action::SearchNext);

        let (start, end) = crate::tui::ui::log_window_for_test(&app.lines, 12, app.pinned);
        let shown: Vec<usize> = app
            .lines
            .iter()
            .skip(start)
            .take(end - start)
            .map(|l| l.index)
            .collect();
        assert!(shown.contains(&100), "hit visible");
        assert!(
            shown.last().copied().unwrap_or(0) > 100,
            "context follows the hit: {shown:?}"
        );
    }

    /// Repeated jumps must advance, not re-find the same hit — the window edge
    /// sits below the match, so searching from the pin would stall.
    #[test]
    fn repeated_jumps_keep_advancing() {
        let mut app = App::new();
        app.log_rows = 10;
        let mut texts: Vec<String> = (0..300).map(|i| format!("line {i}")).collect();
        for i in [50, 100, 150, 200] {
            texts[i] = format!("hit at {i}");
        }
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        feed_texts(&mut app, &refs);
        search(&mut app, "hit at");

        app.update(Action::SearchNext); // wraps to the first
        assert_eq!(app.cursor, Some(50));
        app.update(Action::SearchNext);
        assert_eq!(app.cursor, Some(100), "advanced past the first hit");
        app.update(Action::SearchNext);
        assert_eq!(app.cursor, Some(150));
    }

    /// The whole workflow: tail live output with matches highlighted, jump to a hit
    /// to read (which stops the scrolling), then return to tailing.
    #[test]
    fn jumping_pauses_the_tail_and_g_resumes_it() {
        let mut app = App::new();
        app.log_rows = 10;
        let mut texts: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
        texts[40] = "the hit".to_string();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        feed_texts(&mut app, &refs);
        // Typing pins on its own, so resume first to prove the jump is what pauses.
        search(&mut app, "the hit");
        app.update(Action::ScrollBottom);
        assert!(app.following(), "back to live before the jump");

        app.update(Action::SearchNext);
        assert!(!app.following(), "jumping to a hit stops the scrolling");

        // New output must not drag the view off the hit being read.
        let held = app.pinned;
        feed_texts(&mut app, &["more", "more", "more"]);
        assert_eq!(app.pinned, held, "stays put while reading");

        app.update(Action::ScrollBottom);
        assert!(app.following(), "G returns to auto-scroll");
    }

    /// Scrolling by hand means the user has taken over; the next jump should
    /// continue from where they are looking.
    #[test]
    fn scrolling_drops_the_match_cursor() {
        let mut app = App::new();
        app.log_rows = 10;
        feed_texts(&mut app, &["hit", "x", "hit", "x"]);
        search(&mut app, "hit");
        app.update(Action::SearchNext);
        assert!(app.cursor.is_some());
        app.update(Action::ScrollUp);
        assert_eq!(app.cursor, None, "manual scroll releases the cursor");
    }

    #[test]
    fn jumping_with_no_matches_leaves_the_view_alone() {
        let mut app = App::new();
        app.log_rows = 4;
        feed_texts(&mut app, &["a", "b"]);
        search(&mut app, "nothing-here");
        app.pinned = Some(1);
        app.update(Action::SearchNext);
        assert_eq!(app.pinned, Some(1), "unchanged");
        assert_eq!(app.cursor, None, "nothing to land on");
    }

    /// The bug: `scroll` clamped against the whole buffer, so with the filter on
    /// each keypress moved the pin one *logical* line — through lines the pane
    /// never draws — and the view looked frozen.
    #[test]
    fn scrolling_with_the_filter_on_steps_between_matches() {
        let mut app = App::new();
        app.log_rows = 2;
        // Matches at 0 and 6, with five hidden lines between them.
        feed_texts(
            &mut app,
            &["hit a", "x", "x", "x", "x", "x", "hit b", "y", "hit c"],
        );
        search(&mut app, "hit");
        app.filter_matches = true;

        app.update(Action::ScrollUp);
        assert_eq!(
            app.pinned,
            Some(6),
            "one press moves a whole row, not one logical line"
        );
        // Rows are matches [0, 6, 8]; with a 2-row pane the pin cannot go below
        // row 1 (index 6) or the pane would draw a blank line above the text.
        app.update(Action::ScrollUp);
        assert_eq!(app.pinned, Some(6), "clamped by the pane height");
    }

    /// With room to show them, scrolling reaches the oldest match.
    #[test]
    fn a_taller_pane_scrolls_to_the_oldest_match() {
        let mut app = App::new();
        app.log_rows = 1;
        feed_texts(&mut app, &["hit a", "x", "x", "hit b", "x", "hit c"]);
        search(&mut app, "hit");
        app.filter_matches = true;
        for _ in 0..5 {
            app.update(Action::ScrollUp);
        }
        assert_eq!(app.pinned, Some(0), "reached the oldest match");
    }

    /// Scrolling back down through a filtered view must resume tailing.
    #[test]
    fn scrolling_down_through_matches_resumes_following() {
        let mut app = App::new();
        app.log_rows = 2;
        feed_texts(&mut app, &["hit a", "x", "hit b", "x", "hit c"]);
        search(&mut app, "hit");
        app.filter_matches = true;
        app.pinned = Some(0);
        app.update(Action::ScrollDown);
        app.update(Action::ScrollDown);
        assert!(app.following(), "reaching the newest match tails again");
    }

    /// Turning the filter on can leave the pin naming a now-hidden line.
    #[test]
    fn enabling_the_filter_moves_a_pin_onto_a_visible_line() {
        let mut app = App::new();
        app.log_rows = 1;
        feed_texts(&mut app, &["hit", "x", "x", "x"]);
        search(&mut app, "hit");
        app.pinned = Some(2); // a non-matching line
        app.update(Action::ToggleFilterMatches);
        let pin = app.pinned;
        assert!(
            pin.is_none() || pin == Some(0),
            "pin must name a match or tail; got {pin:?}"
        );
    }

    /// With a query that matches nothing, filtering leaves no rows at all.
    #[test]
    fn filtering_to_zero_matches_falls_back_to_tailing() {
        let mut app = App::new();
        app.log_rows = 2;
        feed_texts(&mut app, &["a", "b"]);
        search(&mut app, "no-such-text");
        app.pinned = Some(0);
        app.update(Action::ToggleFilterMatches);
        assert!(app.following(), "nothing to pin to");
    }

    /// A wheel notch must cover more ground than an arrow key, or scrolling a long
    /// log by mouse is unusably slow.
    #[test]
    fn a_wheel_notch_moves_further_than_an_arrow_key() {
        let mut arrow = App::new();
        arrow.log_rows = 10;
        feed(&mut arrow, 200);
        arrow.update(Action::ScrollUp);

        let mut wheel = App::new();
        wheel.log_rows = 10;
        feed(&mut wheel, 200);
        wheel.update(Action::WheelUp);

        assert!(
            wheel.pinned < arrow.pinned,
            "wheel {:?} should reach further back than arrow {:?}",
            wheel.pinned,
            arrow.pinned
        );
    }

    /// Wheeling back down to the newest line resumes tailing, same as any scroll.
    #[test]
    fn wheeling_back_to_the_bottom_resumes_following() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 100);
        app.update(Action::WheelUp);
        assert!(!app.following());
        for _ in 0..10 {
            app.update(Action::WheelDown);
        }
        assert!(app.following(), "returning to the newest line tails again");
    }

    /// Scrolling older cannot run past the oldest retained line.
    #[test]
    fn scrolling_up_stops_at_the_oldest_line() {
        let mut app = App::new();
        app.log_rows = 10;
        feed(&mut app, 30);
        for _ in 0..20 {
            app.update(Action::ScrollPageUp);
        }
        let (start, _) = crate::tui::ui::log_window_for_test(&app.lines, 10, app.pinned);
        assert_eq!(start, 0, "clamped to the top of the buffer");
    }

    #[test]
    fn navigation_on_an_empty_list_does_not_panic() {
        let mut app = App::new();
        app.update(Action::SelectNext);
        app.update(Action::SelectPrev);
        app.update(Action::SelectLast);
        assert!(app.selected_job().is_none());
    }

    // ─── grouping and filtering ──────────────────────────────────────────────

    fn ids(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .map(|r| match r {
                Row::Header { session, .. } => {
                    format!("[{}]", session.as_deref().unwrap_or("none"))
                }
                Row::Job(i) => app.jobs[*i].job_id.clone(),
            })
            .collect()
    }

    /// The point of the monitor: live work is visible without hunting for it.
    #[test]
    fn running_jobs_sort_above_finished_ones() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![
                job("done", false),
                job_in("live", true, "s1", 10),
                job("also-done", false),
            ],
            health(),
        ));
        assert_eq!(ids(&app)[0], "live");
    }

    #[test]
    fn the_running_filter_hides_finished_jobs() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![job("done", false), job_in("live", true, "s1", 10)],
            health(),
        ));
        app.update(Action::ToggleRunningOnly);
        assert_eq!(ids(&app), vec!["live"]);
        app.update(Action::ToggleRunningOnly);
        assert_eq!(ids(&app).len(), 2, "toggling back restores the full list");
    }

    /// Filtering away the selected job must move the cursor to something visible,
    /// not leave it pointing at a hidden row.
    #[test]
    fn filtering_out_the_selection_reanchors_it() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![job_in("live", true, "s1", 10), job("done", false)],
            health(),
        ));
        app.update(Action::SelectLast);
        assert_eq!(app.selected_job().unwrap().job_id, "done");
        app.update(Action::ToggleRunningOnly);
        assert_eq!(app.selected_job().unwrap().job_id, "live");
    }

    #[test]
    fn grouping_nests_jobs_under_their_session() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![
                job_in("a1", true, "sess-aaa", 30),
                job_in("b1", true, "sess-bbb", 20),
                job_in("a2", false, "sess-aaa", 10),
            ],
            health(),
        ));
        app.update(Action::ToggleGrouping);
        assert_eq!(
            ids(&app),
            vec!["[sess-aaa]", "a1", "a2", "[sess-bbb]", "b1"],
            "each session's jobs follow its header"
        );
    }

    /// Jobs created without a session id still have to be reachable.
    #[test]
    fn unattributed_jobs_get_their_own_trailing_group() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![job("orphan", true), job_in("owned", true, "sess-aaa", 10)],
            health(),
        ));
        app.update(Action::ToggleGrouping);
        let rows = ids(&app);
        assert_eq!(rows.last().unwrap(), "orphan");
        assert_eq!(rows[rows.len() - 2], "[none]");
    }

    #[test]
    fn a_header_counts_its_jobs_and_how_many_run() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![
                job_in("a1", true, "sess-aaa", 30),
                job_in("a2", false, "sess-aaa", 10),
            ],
            health(),
        ));
        app.update(Action::ToggleGrouping);
        match &app.rows[0] {
            Row::Header { jobs, running, .. } => {
                assert_eq!((*jobs, *running), (2, 1));
            }
            other => panic!("expected a header, got {other:?}"),
        }
    }

    /// Headers are labels, not targets — landing on one would show no log and
    /// make `j`/`k` feel like it stuttered.
    #[test]
    fn navigation_skips_session_headers() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![
                job_in("a1", true, "sess-aaa", 30),
                job_in("b1", true, "sess-bbb", 20),
            ],
            health(),
        ));
        app.update(Action::ToggleGrouping);
        // Rows: [aaa] a1 [bbb] b1 — stepping once must reach b1, not the header.
        assert_eq!(app.selected_job().unwrap().job_id, "a1");
        app.update(Action::SelectNext);
        assert_eq!(app.selected_job().unwrap().job_id, "b1");
        app.update(Action::SelectPrev);
        assert_eq!(app.selected_job().unwrap().job_id, "a1");
    }

    #[test]
    fn the_first_selection_is_never_a_header() {
        let mut app = App::new();
        app.update(Action::ToggleGrouping);
        app.update(Action::RefreshJobs(
            vec![job_in("a1", true, "sess-aaa", 10)],
            health(),
        ));
        assert!(app.selected_job().is_some());
        assert_eq!(app.selected_job().unwrap().job_id, "a1");
    }

    /// Toggling the view must not silently move the cursor to another job.
    #[test]
    fn toggling_grouping_keeps_the_selected_job() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![
                job_in("a1", true, "sess-aaa", 30),
                job_in("b1", true, "sess-bbb", 20),
            ],
            health(),
        ));
        app.update(Action::SelectNext);
        let before = app.selected_job().unwrap().job_id.clone();
        app.update(Action::ToggleGrouping);
        assert_eq!(app.selected_job().unwrap().job_id, before);
    }

    #[test]
    fn a_refresh_that_drops_the_selected_job_does_not_panic() {
        let mut app = App::new();
        app.update(Action::RefreshJobs(
            vec![job("a", true), job("b", true)],
            health(),
        ));
        app.update(Action::SelectLast);
        // Evicted from the registry while selected.
        app.update(Action::RefreshJobs(vec![job("a", true)], health()));
        assert_eq!(app.selected_job().unwrap().job_id, "a");
    }

    // ─── Event stream ─────────────────────────────────────────────────────────

    fn created(id: &str) -> ServerEvent {
        ServerEvent::JobCreated {
            job_id: id.into(),
            cmd: "sbt".into(),
            args: vec!["test".into()],
            session_id: None,
        }
    }

    fn progress(id: &str, line_count: usize) -> ServerEvent {
        ServerEvent::JobProgress {
            job_id: id.into(),
            line_count,
            elapsed_ms: 1_000,
            idle_ms: 0,
        }
    }

    fn finished(id: &str, exit_code: i32) -> ServerEvent {
        ServerEvent::JobFinished {
            job_id: id.into(),
            outcome: Outcome {
                kind: "completed".into(),
                escalated: false,
            },
            exit_code: Some(exit_code),
            elapsed_ms: 2_000,
            line_count: 42,
        }
    }

    fn activity(id: u64, done: bool) -> Activity {
        Activity {
            id,
            kind: "exec".into(),
            cmd: "git".into(),
            args: vec!["status".into()],
            cwd: None,
            started_ms: 0,
            duration_ms: done.then_some(12),
            exit_code: done.then_some(0),
            error: None,
            session_id: None,
        }
    }

    fn send(app: &mut App, event: ServerEvent) {
        app.update(Action::Server(event));
    }

    #[test]
    fn a_created_job_appears_and_is_selected() {
        let mut app = App::new();
        send(&mut app, created("a"));
        assert_eq!(app.jobs.len(), 1);
        assert!(app.jobs[0].running);
        assert_eq!(
            app.selected_job().map(|j| j.job_id.as_str()),
            Some("a"),
            "the first job must land under the cursor"
        );
    }

    #[test]
    fn progress_updates_the_counters_in_place() {
        let mut app = App::new();
        send(&mut app, created("a"));
        send(&mut app, progress("a", 128));
        assert_eq!(app.jobs.len(), 1, "progress must not append a second row");
        assert_eq!(app.jobs[0].line_count, 128);
        assert_eq!(app.jobs[0].elapsed_ms, 1_000);
    }

    #[test]
    fn finishing_records_the_outcome_and_final_count() {
        let mut app = App::new();
        send(&mut app, created("a"));
        send(&mut app, finished("a", 0));
        assert!(!app.jobs[0].running);
        assert_eq!(app.jobs[0].exit_code, Some(0));
        assert_eq!(app.jobs[0].line_count, 42, "the closing count must win");
    }

    /// The property the whole design rests on: the stream's opening replay
    /// overlaps its live events, so applying anything twice must be a no-op.
    #[test]
    fn replaying_the_same_events_changes_nothing() {
        let mut app = App::new();
        let script = || [created("a"), progress("a", 10), finished("a", 0)];
        for event in script() {
            send(&mut app, event);
        }
        let snapshot = (app.jobs.len(), app.jobs[0].line_count, app.jobs[0].running);
        for event in script() {
            send(&mut app, event);
        }
        assert_eq!(
            (app.jobs.len(), app.jobs[0].line_count, app.jobs[0].running),
            snapshot
        );
    }

    /// Replay order is not guaranteed against the live tail, so a stale progress
    /// event can arrive after the terminal one — and must not undo it.
    #[test]
    fn a_late_progress_event_cannot_resurrect_a_finished_job() {
        let mut app = App::new();
        send(&mut app, created("a"));
        send(&mut app, finished("a", 2));
        send(&mut app, progress("a", 5));
        assert!(!app.jobs[0].running, "a finished job must stay finished");
        assert_eq!(app.jobs[0].exit_code, Some(2));
        assert_eq!(app.jobs[0].line_count, 42, "the final count must hold");
    }

    #[test]
    fn an_evicted_job_leaves_the_list() {
        let mut app = App::new();
        send(&mut app, created("a"));
        send(&mut app, created("b"));
        send(&mut app, ServerEvent::JobEvicted { job_id: "a".into() });
        assert_eq!(app.jobs.len(), 1);
        assert_eq!(app.jobs[0].job_id, "b");
    }

    #[test]
    fn an_event_for_an_unknown_job_is_ignored() {
        let mut app = App::new();
        send(&mut app, progress("ghost", 1));
        send(&mut app, finished("ghost", 0));
        send(
            &mut app,
            ServerEvent::JobEvicted {
                job_id: "ghost".into(),
            },
        );
        assert!(app.jobs.is_empty());
    }

    /// A call is one row that gains an outcome, not a start row plus an end row.
    #[test]
    fn a_finished_call_replaces_its_own_start_record() {
        let mut app = App::new();
        send(
            &mut app,
            ServerEvent::ActivityStarted {
                activity: activity(1, false),
            },
        );
        assert_eq!(app.activities_running(), 1);
        send(
            &mut app,
            ServerEvent::ActivityFinished {
                activity: activity(1, true),
            },
        );
        assert_eq!(app.activities.len(), 1, "the call must not be duplicated");
        assert_eq!(app.activities_running(), 0);
        assert_eq!(app.activities[0].exit_code, Some(0));
    }

    #[test]
    fn the_activity_list_is_bounded() {
        let mut app = App::new();
        for id in 0..MAX_ACTIVITIES as u64 + 25 {
            send(
                &mut app,
                ServerEvent::ActivityStarted {
                    activity: activity(id, true),
                },
            );
        }
        assert_eq!(app.activities.len(), MAX_ACTIVITIES);
        // The oldest are the ones dropped, so the newest call is still on screen.
        assert_eq!(
            app.activities.back().map(|a| a.id),
            Some(MAX_ACTIVITIES as u64 + 24)
        );
    }

    /// The user must be able to tell a live feed from one that quietly died.
    #[test]
    fn losing_the_stream_clears_ready_and_says_so() {
        let mut app = App::new();
        send(&mut app, ServerEvent::Ready);
        assert!(app.ready);
        app.update(Action::StreamLost("connection reset".into()));
        assert!(!app.ready);
        assert!(app.status_message.is_some());
    }

    #[test]
    fn a_lag_is_reported_rather_than_hidden() {
        let mut app = App::new();
        send(&mut app, ServerEvent::Lagged { dropped: 7 });
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|m| m.contains('7')),
            "the user must be told how much was missed: {:?}",
            app.status_message
        );
    }

    /// A reconcile fetch is authoritative, so it must not lose the selection or
    /// double up with what the stream already delivered.
    #[test]
    fn a_reconcile_replaces_streamed_state_without_moving_the_cursor() {
        let mut app = App::new();
        send(&mut app, created("a"));
        send(&mut app, created("b"));
        app.update(Action::SelectLast);
        let before = app.selected_job().unwrap().job_id.clone();

        app.update(Action::RefreshJobs(
            vec![job("a", true), job("b", true)],
            health(),
        ));
        assert_eq!(app.jobs.len(), 2, "a reconcile replaces, never appends");
        assert_eq!(app.selected_job().unwrap().job_id, before);
    }

    /// A job finishing reorders the list (running first), and the cursor must
    /// follow the job rather than the row it happened to occupy.
    #[test]
    fn the_cursor_follows_its_job_when_finishing_reorders_the_list() {
        let mut app = App::new();
        send(&mut app, created("a"));
        send(&mut app, created("b"));
        app.update(Action::SelectFirst);
        let watched = app.selected_job().unwrap().job_id.clone();
        // Whichever job the cursor is not on finishes, which sorts it below.
        let other = if watched == "a" { "b" } else { "a" };
        send(&mut app, finished(other, 0));
        assert_eq!(app.selected_job().unwrap().job_id, watched);
    }

    // ─── Error log ────────────────────────────────────────────────────────────

    /// The bug that motivated this: an error flashed past and the next success
    /// wiped it, with no server-side log to fall back on.
    #[test]
    fn an_error_survives_the_next_success() {
        let mut app = App::new();
        app.update(Action::FetchError("connection refused".into()));
        assert_eq!(app.errors.len(), 1);

        // A successful reconcile clears the status line…
        app.update(Action::RefreshJobs(vec![], health()));
        assert!(app.status_message.is_none());
        // …but the error must still be readable.
        assert_eq!(app.errors.len(), 1);
        assert!(app.errors[0].message.contains("connection refused"));
        assert!(!app.errors[0].at.is_empty(), "must carry a timestamp");
    }

    /// A stream reconnecting on a loop would otherwise fill the log with one
    /// message and push out everything that actually differs.
    #[test]
    fn a_repeated_error_bumps_a_count_instead_of_piling_up() {
        let mut app = App::new();
        for _ in 0..5 {
            app.update(Action::StreamLost("reset".into()));
        }
        assert_eq!(app.errors.len(), 1, "one entry, not five");
        assert_eq!(app.errors[0].count, 5);

        // A different message is its own entry.
        app.update(Action::FetchError("timeout".into()));
        assert_eq!(app.errors.len(), 2);
    }

    #[test]
    fn the_error_log_is_bounded() {
        let mut app = App::new();
        for i in 0..MAX_ERRORS + 30 {
            app.update(Action::FetchError(format!("failure {i}")));
        }
        assert_eq!(app.errors.len(), MAX_ERRORS);
        // Newest kept: a current failure matters more than an old one.
        assert!(app
            .errors
            .back()
            .is_some_and(|e| e.message.contains(&format!("failure {}", MAX_ERRORS + 29))));
    }

    fn diagnostic(id: u64, message: &str) -> ServerEvent {
        ServerEvent::Diagnostic {
            diagnostic: crate::tui::client::Diagnostic {
                id,
                at: "12:00:00".into(),
                level: "ERROR".into(),
                target: "claude_sidecar::job".into(),
                message: message.into(),
            },
        }
    }

    /// Server-side failures must reach the same log, since that is where a user
    /// will look — and they are otherwise only on stderr.
    #[test]
    fn server_diagnostics_land_in_the_error_log() {
        let mut app = App::new();
        send(&mut app, diagnostic(1, "spill write failed"));
        assert_eq!(app.errors.len(), 1);
        assert!(app.errors[0].message.contains("spill write failed"));
        assert!(
            app.errors[0].message.contains("sidecar"),
            "must be attributed to the server: {}",
            app.errors[0].message
        );
    }

    /// Each reconnect replays every diagnostic, so without id dedup the log would
    /// grow by the full set every 500ms of flapping.
    #[test]
    fn replayed_diagnostics_are_not_recorded_twice() {
        let mut app = App::new();
        for _ in 0..3 {
            send(&mut app, diagnostic(1, "spill write failed"));
            send(&mut app, diagnostic(2, "runner vanished"));
        }
        assert_eq!(app.errors.len(), 2);
        assert_eq!(app.errors[0].count, 1, "dedup by id, not a repeat count");
    }

    /// Both overlays at once would stack unreadably.
    #[test]
    fn opening_one_overlay_closes_the_other() {
        let mut app = App::new();
        app.update(Action::ToggleHelp);
        assert!(app.show_help);
        app.update(Action::ToggleErrors);
        assert!(app.show_errors);
        assert!(!app.show_help, "help must yield to the error log");
        app.update(Action::ToggleHelp);
        assert!(app.show_help);
        assert!(!app.show_errors);
    }

    /// Three overlays now share the screen, so every pair must be exclusive —
    /// not just the original two.
    #[test]
    fn the_stats_overlay_is_exclusive_with_the_others() {
        let mut app = App::new();
        app.update(Action::ToggleStats);
        assert!(app.show_stats);

        app.update(Action::ToggleHelp);
        assert!(app.show_help);
        assert!(!app.show_stats, "stats must yield to help");

        app.update(Action::ToggleStats);
        assert!(app.show_stats);
        assert!(!app.show_help, "help must yield to stats");

        app.update(Action::ToggleErrors);
        assert!(app.show_errors);
        assert!(!app.show_stats, "stats must yield to the error log");
    }

    /// A sidecar with metrics disabled reports that every time the panel is
    /// opened. Recording those would bury the failures the log exists for.
    #[test]
    fn a_stats_failure_does_not_pollute_the_error_log() {
        let mut app = App::new();
        app.update(Action::StatsFailed("metrics are disabled".into()));
        assert_eq!(app.stats_error.as_deref(), Some("metrics are disabled"));
        assert!(app.errors.is_empty(), "must not enter the retained log");
        assert!(app.status_message.is_none(), "nor the status bar");
    }

    /// A successful fetch must clear a previous failure, or a transient error
    /// would show forever.
    #[test]
    fn loading_stats_clears_a_previous_failure() {
        let mut app = App::new();
        app.update(Action::StatsFailed("boom".into()));
        app.update(Action::StatsLoaded(Box::new(StatsSnapshot {
            total_calls: 3,
            ..Default::default()
        })));
        assert!(app.stats_error.is_none());
        assert_eq!(app.stats.as_ref().map(|s| s.total_calls), Some(3));
    }
}
