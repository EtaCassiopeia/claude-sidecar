use std::collections::VecDeque;

use ratatui::widgets::TableState;

use super::client::{HealthInfo, JobLine, JobSummary};

/// Maximum log lines retained in the TUI scrollback per job.
pub const MAX_SCROLLBACK: usize = 5_000;

/// All distinct UI modes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    ConfirmKill,
    Help,
}

/// Every message that can flow through the action channel.
#[derive(Debug)]
pub enum Action {
    Quit,
    /// Periodic data refresh: new job list and health snapshot.
    RefreshJobs(Vec<JobSummary>, HealthInfo),
    /// A connection or fetch error to display in the status bar.
    FetchError(String),
    /// Append one log line for the currently selected job.
    AppendLogLine(JobLine),
    /// The SSE stream for the selected job has ended.
    LogStreamEnded,
    // Navigation
    SelectNext,
    SelectPrev,
    SelectFirst,
    SelectLast,
    // Log scrolling
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    ToggleFollow,
    // Kill flow
    RequestKill,
    ConfirmKill,
    CancelModal,
    // Help
    ToggleHelp,
    // Save log to file
    SaveLog,
}

#[derive(Default)]
pub struct App {
    pub mode: Mode,
    pub jobs: Vec<JobSummary>,
    pub table_state: TableState,
    pub lines: VecDeque<JobLine>,
    /// The job ID whose SSE stream is currently loaded.
    pub loaded_job_id: Option<String>,
    pub scroll_offset: u16,
    /// When true, new log lines snap the view to the bottom.
    pub follow: bool,
    pub health: Option<HealthInfo>,
    pub status_message: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            follow: true,
            ..Default::default()
        }
    }

    /// The currently selected job, if any.
    pub fn selected_job(&self) -> Option<&JobSummary> {
        self.table_state.selected().and_then(|i| self.jobs.get(i))
    }

    pub fn update(&mut self, action: Action) -> Option<Action> {
        match action {
            Action::Quit => {}

            Action::RefreshJobs(jobs, health) => {
                // Preserve selection by job_id across refreshes.
                let selected_id = self
                    .table_state
                    .selected()
                    .and_then(|i| self.jobs.get(i))
                    .map(|j| j.job_id.clone());

                self.jobs = jobs;
                self.health = Some(health);
                self.status_message = None;

                // Re-select by matching job_id.
                let new_idx = selected_id
                    .as_deref()
                    .and_then(|id| self.jobs.iter().position(|j| j.job_id == id));
                if let Some(idx) = new_idx {
                    self.table_state.select(Some(idx));
                } else if !self.jobs.is_empty() && self.table_state.selected().is_none() {
                    self.table_state.select(Some(0));
                } else if self.jobs.is_empty() {
                    self.table_state.select(None);
                }
            }

            Action::FetchError(msg) => {
                self.status_message = Some(format!("Error: {msg}"));
            }

            Action::AppendLogLine(line) => {
                self.lines.push_back(line);
                while self.lines.len() > MAX_SCROLLBACK {
                    self.lines.pop_front();
                }
                // follow: don't adjust offset here — rendering uses reversed
                // order, so offset 0 is always the newest line. follow = offset
                // stays at 0, which happens automatically if we don't scroll up.
            }

            Action::LogStreamEnded => {}

            Action::SelectNext => {
                if self.jobs.is_empty() {
                    return None;
                }
                let next = self
                    .table_state
                    .selected()
                    .map(|i| (i + 1).min(self.jobs.len() - 1))
                    .unwrap_or(0);
                self.table_state.select(Some(next));
                self.clear_log();
            }

            Action::SelectPrev => {
                if self.jobs.is_empty() {
                    return None;
                }
                let prev = self
                    .table_state
                    .selected()
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(0);
                self.table_state.select(Some(prev));
                self.clear_log();
            }

            Action::SelectFirst => {
                if !self.jobs.is_empty() {
                    self.table_state.select(Some(0));
                    self.clear_log();
                }
            }

            Action::SelectLast => {
                if !self.jobs.is_empty() {
                    self.table_state.select(Some(self.jobs.len() - 1));
                    self.clear_log();
                }
            }

            Action::ScrollUp => {
                self.follow = false;
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }

            Action::ScrollDown => {
                if self.scroll_offset > 0 {
                    self.scroll_offset -= 1;
                } else {
                    self.follow = true;
                }
            }

            Action::ScrollPageUp => {
                self.follow = false;
                self.scroll_offset = self.scroll_offset.saturating_add(20);
            }

            Action::ScrollPageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                if self.scroll_offset == 0 {
                    self.follow = true;
                }
            }

            Action::ScrollTop => {
                self.follow = false;
                self.scroll_offset = u16::MAX;
            }

            Action::ScrollBottom => {
                self.scroll_offset = 0;
                self.follow = true;
            }

            Action::ToggleFollow => {
                self.follow = !self.follow;
                if self.follow {
                    self.scroll_offset = 0;
                }
            }

            Action::RequestKill => {
                if let Some(job) = self.selected_job() {
                    if job.running {
                        self.mode = Mode::ConfirmKill;
                    }
                }
            }

            Action::ConfirmKill => {
                self.mode = Mode::Normal;
            }

            Action::CancelModal => {
                self.mode = Mode::Normal;
            }

            Action::ToggleHelp => {
                self.mode = if self.mode == Mode::Help {
                    Mode::Normal
                } else {
                    Mode::Help
                };
            }

            Action::SaveLog => {}
        }
        None
    }

    fn clear_log(&mut self) {
        self.lines.clear();
        self.scroll_offset = 0;
        self.follow = true;
        self.loaded_job_id = None;
    }
}
