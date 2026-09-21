//! Server-wide event fan-out, so a watcher learns what the sidecar is doing
//! without polling for it.
//!
//! Two things are published here that no other channel carries:
//!
//! * **Job lifecycle and counters.** `/jobs/{id}/stream` covers one job's output;
//!   nothing told a watcher that a *different* job started, finished, or grew.
//! * **One-shot calls.** `/exec`, `/batch`, `/browser/*`, and `/gdocs/*` never
//!   touch the [`JobRegistry`], so they were invisible to a monitor even though
//!   they are the bulk of the traffic. They are recorded here as [`Activity`].
//!
//! Job events come from diffing the registry on a short interval rather than
//! from emit points inside `job.rs`. That keeps the bus structurally unable to
//! drift from the registry — there is no emit site to forget — at the cost of up
//! to [`WATCH_INTERVAL`] of latency. Activities are published from their route
//! directly, since nothing else holds them.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::job::{now_ms, recover, JobRegistry, Outcome};

/// Recent one-shot calls retained for replay to a newly-attached watcher, so a
/// monitor opened mid-session still shows the last few minutes of traffic.
const MAX_ACTIVITIES: usize = 200;

/// Broadcast depth. A watcher that falls this far behind is told it lagged and
/// must reconcile from `/jobs` — the same contract the per-job stream has.
const BROADCAST_CAPACITY: usize = 512;

/// Baseline heartbeat for a running job that produced no new output, so elapsed
/// and idle times stay live in a watcher that never polls.
const PROGRESS_HEARTBEAT: Duration = Duration::from_secs(1);

/// How often the registry is diffed. This is the worst-case latency between a
/// job starting or finishing and a watcher hearing about it.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum retained diagnostics. Enough to cover a session's worth of failures
/// without unbounded growth in a long-lived daemon.
const MAX_DIAGNOSTICS: usize = 500;

/// One retained warning or error from the sidecar's own `tracing` output.
///
/// Those calls — a spill write failing, a runner vanishing, an SSE subscriber
/// lagging — went to stderr only, which is unreachable once the terminal that
/// launched the daemon is gone. Without this there is no way to answer "why did
/// that fail" after the fact.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub id: u64,
    /// Local `HH:MM:SS`, matching the request log's stamps so the two correlate.
    pub at: String,
    pub level: String,
    /// Emitting module, e.g. `claude_sidecar::job`.
    pub target: String,
    pub message: String,
}

// ─── Activity ─────────────────────────────────────────────────────────────────

/// Which endpoint produced a one-shot call.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Exec,
    /// One step of a `/batch` request. Steps are recorded individually because
    /// that is the granularity at which they run and fail.
    Batch,
    Browser,
    Gdocs,
}

impl ActivityKind {
    /// The wire/storage name, matching the `snake_case` serialization so an
    /// event and a persisted row agree on what to call the same kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Batch => "batch",
            Self::Browser => "browser",
            Self::Gdocs => "gdocs",
        }
    }
}

/// A single non-job operation, from request to response.
#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    /// Process-unique, monotonic. Lets a watcher replace a started record with
    /// its finished form instead of accumulating both.
    pub id: u64,
    pub kind: ActivityKind,
    pub cmd: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub started_ms: u64,
    /// Absent while the call is in flight.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Absent while in flight, and for a call that failed before producing one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Set when the call failed to run at all (denied, timed out, spawn error)
    /// rather than exiting with a code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Which Claude Code session asked for this call, when the caller said. One
    /// sidecar serves every session on the machine, so without it the whole log
    /// is one undifferentiated stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

// ─── ServerEvent ──────────────────────────────────────────────────────────────

/// Everything a watcher is told. Serialize-only: the TUI keeps its own
/// `Deserialize` mirror, so adding a variant here cannot break an older client.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    JobCreated {
        job_id: String,
        cmd: String,
        args: Vec<String>,
        session_id: Option<String>,
    },
    JobProgress {
        job_id: String,
        line_count: usize,
        elapsed_ms: u64,
        idle_ms: u64,
    },
    JobFinished {
        job_id: String,
        outcome: Outcome,
        exit_code: Option<i32>,
        elapsed_ms: u64,
        /// Carried so the final count is right even when the last `JobProgress`
        /// predates the job's closing output.
        line_count: usize,
    },
    JobEvicted {
        job_id: String,
    },
    ActivityStarted {
        activity: Activity,
    },
    ActivityFinished {
        activity: Activity,
    },
    /// A warning or error the sidecar logged about itself, pushed live so a
    /// watcher sees a failure as it happens rather than only on request.
    Diagnostic {
        diagnostic: Diagnostic,
    },
}

// ─── DiagnosticLog ────────────────────────────────────────────────────────────

/// Process-wide ring of the sidecar's own warnings and errors.
///
/// A `static` rather than a field on [`EventBus`] because the `tracing` layer that
/// feeds it is installed once at startup, before any state exists, and
/// `tracing`'s dispatcher owns it for the life of the process.
static DIAGNOSTICS: RwLock<VecDeque<Diagnostic>> = RwLock::new(VecDeque::new());
static NEXT_DIAGNOSTIC_ID: AtomicU64 = AtomicU64::new(1);

/// Record a diagnostic and return it, so the caller can also broadcast it.
pub fn record_diagnostic(level: &str, target: &str, message: String) -> Diagnostic {
    let diagnostic = Diagnostic {
        id: NEXT_DIAGNOSTIC_ID.fetch_add(1, Ordering::Relaxed),
        at: crate::logger::now_hms(),
        level: level.to_string(),
        target: target.to_string(),
        message,
    };
    let mut log = recover(DIAGNOSTICS.write());
    log.push_back(diagnostic.clone());
    while log.len() > MAX_DIAGNOSTICS {
        log.pop_front();
    }
    diagnostic
}

/// Every retained diagnostic, oldest first.
pub fn diagnostics() -> Vec<Diagnostic> {
    recover(DIAGNOSTICS.read()).iter().cloned().collect()
}

#[cfg(test)]
fn clear_diagnostics() {
    recover(DIAGNOSTICS.write()).clear();
}

// ─── EventBus ─────────────────────────────────────────────────────────────────

/// What the last diff pass saw for one job, so the next pass can tell what
/// changed.
struct Watched {
    line_count: usize,
    running: bool,
    last_progress: Instant,
}

pub struct EventBus {
    tx: broadcast::Sender<ServerEvent>,
    activities: RwLock<VecDeque<Activity>>,
    next_id: AtomicU64,
    /// Durable metrics, when a writable directory was found. Recording happens
    /// here rather than at each route because [`Self::finish`] is the one place
    /// every call already converges — a per-route hook is a hook to forget.
    metrics: RwLock<Option<Arc<crate::metrics::MetricsRecorder>>>,
}

impl EventBus {
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            tx,
            activities: RwLock::new(VecDeque::with_capacity(MAX_ACTIVITIES)),
            next_id: AtomicU64::new(1),
            metrics: RwLock::new(None),
        })
    }

    /// Attach a metrics recorder. Separate from [`Self::new`] because the
    /// recorder touches the filesystem and may legitimately be absent.
    pub fn set_metrics(&self, recorder: Arc<crate::metrics::MetricsRecorder>) {
        *recover(self.metrics.write()) = Some(recorder);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.tx.subscribe()
    }

    /// Publish an event. No subscribers is the normal case, not an error.
    pub fn publish(&self, event: ServerEvent) {
        let _ = self.tx.send(event);
    }

    /// Record a one-shot call as in flight and announce it. The returned id is
    /// passed back to [`Self::finish`].
    ///
    /// Endpoints that carry a session id use [`Self::start_for_session`]; this is
    /// the plain form for the ones that do not (`/browser`, `/gdocs`).
    pub fn start(&self, kind: ActivityKind, cmd: &str, args: &[String], cwd: Option<&str>) -> u64 {
        self.start_for_session(kind, cmd, args, cwd, None)
    }

    /// As [`Self::start`], attributing the call to a Claude Code session.
    pub fn start_for_session(
        &self,
        kind: ActivityKind,
        cmd: &str,
        args: &[String],
        cwd: Option<&str>,
        session_id: Option<&str>,
    ) -> u64 {
        let activity = Activity {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            kind,
            cmd: cmd.to_string(),
            args: args.to_vec(),
            cwd: cwd.map(str::to_string),
            started_ms: now_ms(),
            duration_ms: None,
            exit_code: None,
            error: None,
            session_id: session_id.map(str::to_string),
        };
        let id = activity.id;
        {
            let mut log = recover(self.activities.write());
            log.push_back(activity.clone());
            while log.len() > MAX_ACTIVITIES {
                log.pop_front();
            }
        }
        self.publish(ServerEvent::ActivityStarted { activity });
        id
    }

    /// Close out a call started with [`Self::start`]. A call whose record has
    /// already aged out of the ring is still announced, so a watcher that saw the
    /// start also sees the end.
    pub fn finish(&self, id: u64, exit_code: Option<i32>, error: Option<String>) {
        let mut log = recover(self.activities.write());
        let Some(activity) = log.iter_mut().find(|a| a.id == id) else {
            return;
        };
        activity.duration_ms = Some(now_ms().saturating_sub(activity.started_ms));
        activity.exit_code = exit_code;
        activity.error = error;
        let finished = activity.clone();
        drop(log);
        // Persist before announcing, so a watcher that reacts to the event by
        // querying `/stats` cannot observe a gap.
        self.persist(&finished);
        self.publish(ServerEvent::ActivityFinished { activity: finished });
    }

    /// Append one completed call to the durable store.
    ///
    /// Only the binary, an allowlisted subcommand, and an argument *count* are
    /// kept — see [`crate::metrics`] for why argv values are not.
    fn persist(&self, activity: &Activity) {
        let Some(recorder) = recover(self.metrics.read()).clone() else {
            return;
        };
        let ts = activity.started_ms as i64;
        recorder.record(&crate::metrics::CallRecord {
            date: crate::metrics::local_date(ts),
            ts,
            kind: activity.kind.as_str().to_string(),
            cmd: activity.cmd.clone(),
            sub: activity
                .args
                .first()
                .and_then(|a| crate::metrics::safe_subcommand(a)),
            nargs: activity.args.len() as u32,
            ms: activity.duration_ms.map(|ms| ms as i64),
            exit: activity.exit_code,
            error: activity.error.clone(),
            session: activity.session_id.clone(),
        });
    }

    /// The events a newly-attached watcher needs to reach current state without
    /// a separate fetch: every tracked job, then the recent one-shot calls.
    ///
    /// Deliberately overlapping with the live stream rather than boundary-exact.
    /// A watcher applies these idempotently by id, so a duplicate is harmless —
    /// and that is much easier to get right than a handoff.
    pub fn replay(&self, registry: &JobRegistry) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        for job in registry.snapshot() {
            out.push(ServerEvent::JobCreated {
                job_id: job.id.clone(),
                cmd: job.cmd.clone(),
                args: job.args.clone(),
                session_id: job.session_id.clone(),
            });
            let state = job.state();
            match state.outcome() {
                Some(outcome) => out.push(ServerEvent::JobFinished {
                    job_id: job.id.clone(),
                    outcome,
                    exit_code: outcome.exit_code(),
                    elapsed_ms: job.elapsed_ms(),
                    line_count: job.line_count(),
                }),
                None => out.push(ServerEvent::JobProgress {
                    job_id: job.id.clone(),
                    line_count: job.line_count(),
                    elapsed_ms: job.elapsed_ms(),
                    idle_ms: job.idle_ms(),
                }),
            }
        }
        for activity in recover(self.activities.read()).iter() {
            let activity = activity.clone();
            out.push(if activity.duration_ms.is_some() {
                ServerEvent::ActivityFinished { activity }
            } else {
                ServerEvent::ActivityStarted { activity }
            });
        }
        // Server-side failures too, so a monitor opened after the fact still
        // shows why something broke.
        for diagnostic in diagnostics() {
            out.push(ServerEvent::Diagnostic { diagnostic });
        }
        out
    }

    /// Spawn the task that turns registry state into job events.
    pub fn spawn_job_watch(self: &Arc<Self>, registry: Arc<JobRegistry>) {
        let bus = Arc::clone(self);
        tokio::spawn(async move {
            let mut seen: HashMap<String, Watched> = HashMap::new();
            // Highest diagnostic id already broadcast. The `tracing` layer writes
            // to a static ring and cannot reach the bus (it is installed before
            // any state exists), so new entries are forwarded from here rather
            // than pushed at the point they are logged.
            let mut published_diagnostics: u64 = 0;
            let mut ticker = tokio::time::interval(WATCH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                bus.diff(&registry, &mut seen);
                for diagnostic in diagnostics() {
                    if diagnostic.id > published_diagnostics {
                        published_diagnostics = diagnostic.id;
                        bus.publish(ServerEvent::Diagnostic { diagnostic });
                    }
                }
            }
        });
    }

    /// One diff pass: publish whatever changed since `seen` was last updated,
    /// then bring `seen` up to date.
    ///
    /// A job created and finished inside a single interval emits both events, in
    /// order — a short job must not be reported as still running, nor skipped.
    fn diff(&self, registry: &JobRegistry, seen: &mut HashMap<String, Watched>) {
        let jobs = registry.snapshot();
        for job in &jobs {
            let running = job.state().is_running();
            let line_count = job.line_count();
            let elapsed_ms = job.elapsed_ms();
            let previous = seen.get(&job.id);

            if previous.is_none() {
                self.publish(ServerEvent::JobCreated {
                    job_id: job.id.clone(),
                    cmd: job.cmd.clone(),
                    args: job.args.clone(),
                    session_id: job.session_id.clone(),
                });
            }

            let mut last_progress = previous.map_or_else(Instant::now, |w| w.last_progress);
            if running {
                let grew = previous.is_none_or(|w| w.line_count != line_count);
                // The heartbeat is what keeps elapsed and idle live for a job
                // that is working but silent — the case a watcher most needs to
                // see moving.
                let stale = previous.is_none() || last_progress.elapsed() >= PROGRESS_HEARTBEAT;
                if grew || stale {
                    self.publish(ServerEvent::JobProgress {
                        job_id: job.id.clone(),
                        line_count,
                        elapsed_ms,
                        idle_ms: job.idle_ms(),
                    });
                    last_progress = Instant::now();
                }
            } else if previous.is_none_or(|w| w.running) {
                // Terminal transition, or a job first seen already finished.
                if let Some(outcome) = job.state().outcome() {
                    self.publish(ServerEvent::JobFinished {
                        job_id: job.id.clone(),
                        outcome,
                        exit_code: outcome.exit_code(),
                        elapsed_ms,
                        line_count,
                    });
                }
            }

            seen.insert(
                job.id.clone(),
                Watched {
                    line_count,
                    running,
                    last_progress,
                },
            );
        }

        seen.retain(|id, _| {
            if jobs.iter().any(|j| &j.id == id) {
                return true;
            }
            self.publish(ServerEvent::JobEvicted { job_id: id.clone() });
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    fn bus_and_registry() -> (Arc<EventBus>, Arc<JobRegistry>) {
        (EventBus::new(), JobRegistry::new(10, 1000, false, 600))
    }

    /// Everything currently queued, so a test can assert on a whole pass.
    fn drain(rx: &mut broadcast::Receiver<ServerEvent>) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => out.push(e),
                Err(TryRecvError::Empty | TryRecvError::Closed) => return out,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
    }

    fn kinds(events: &[ServerEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                ServerEvent::JobCreated { .. } => "created",
                ServerEvent::JobProgress { .. } => "progress",
                ServerEvent::JobFinished { .. } => "finished",
                ServerEvent::JobEvicted { .. } => "evicted",
                ServerEvent::ActivityStarted { .. } => "started",
                ServerEvent::ActivityFinished { .. } => "finished_activity",
                ServerEvent::Diagnostic { .. } => "diagnostic",
            })
            .collect()
    }

    #[tokio::test]
    async fn a_new_running_job_is_announced_and_reported() {
        let (bus, registry) = bus_and_registry();
        let mut rx = bus.subscribe();
        registry
            .create("sbt".into(), vec!["test".into()], None)
            .unwrap();

        let mut seen = HashMap::new();
        bus.diff(&registry, &mut seen);
        assert_eq!(kinds(&drain(&mut rx)), vec!["created", "progress"]);
    }

    /// The counters must not be re-sent every pass for a job that is producing
    /// nothing — that is what the heartbeat bounds.
    #[tokio::test]
    async fn an_unchanged_job_is_quiet_between_heartbeats() {
        let (bus, registry) = bus_and_registry();
        let mut rx = bus.subscribe();
        registry.create("sbt".into(), vec![], None).unwrap();

        let mut seen = HashMap::new();
        bus.diff(&registry, &mut seen);
        drain(&mut rx);
        bus.diff(&registry, &mut seen);
        assert!(
            drain(&mut rx).is_empty(),
            "a silent job re-reported inside the heartbeat window"
        );
    }

    #[tokio::test]
    async fn new_output_is_reported_immediately() {
        let (bus, registry) = bus_and_registry();
        let mut rx = bus.subscribe();
        let job = registry.create("sbt".into(), vec![], None).unwrap();

        let mut seen = HashMap::new();
        bus.diff(&registry, &mut seen);
        drain(&mut rx);

        job.push_line("compiling".into());
        bus.diff(&registry, &mut seen);
        match drain(&mut rx).as_slice() {
            [ServerEvent::JobProgress { line_count, .. }] => assert_eq!(*line_count, 1),
            other => panic!("expected one progress event, got {:?}", kinds(other)),
        }
    }

    #[tokio::test]
    async fn finishing_is_reported_once() {
        let (bus, registry) = bus_and_registry();
        let mut rx = bus.subscribe();
        let job = registry.create("sbt".into(), vec![], None).unwrap();

        let mut seen = HashMap::new();
        bus.diff(&registry, &mut seen);
        drain(&mut rx);

        job.finish(Outcome::Completed { exit_code: 0 });
        bus.diff(&registry, &mut seen);
        assert_eq!(kinds(&drain(&mut rx)), vec!["finished"]);

        bus.diff(&registry, &mut seen);
        assert!(drain(&mut rx).is_empty(), "finish was reported twice");
    }

    /// A job that starts and ends inside one interval must produce both events —
    /// short commands are exactly the ones a monitor was missing.
    #[tokio::test]
    async fn a_job_born_and_finished_in_one_pass_emits_both() {
        let (bus, registry) = bus_and_registry();
        let mut rx = bus.subscribe();
        let job = registry.create("echo".into(), vec![], None).unwrap();
        job.finish(Outcome::Completed { exit_code: 0 });

        let mut seen = HashMap::new();
        bus.diff(&registry, &mut seen);
        assert_eq!(kinds(&drain(&mut rx)), vec!["created", "finished"]);
    }

    #[tokio::test]
    async fn a_job_leaving_the_registry_is_announced() {
        let (bus, registry) = bus_and_registry();
        let mut rx = bus.subscribe();
        let job = registry.create("sbt".into(), vec![], None).unwrap();
        let id = job.id.clone();

        let mut seen = HashMap::new();
        bus.diff(&registry, &mut seen);
        drain(&mut rx);

        // Stand in for TTL eviction by diffing against a registry without it.
        let empty = JobRegistry::new(10, 1000, false, 600);
        bus.diff(&empty, &mut seen);
        match drain(&mut rx).as_slice() {
            [ServerEvent::JobEvicted { job_id }] => assert_eq!(job_id, &id),
            other => panic!("expected eviction, got {:?}", kinds(other)),
        }
        assert!(seen.is_empty(), "the evicted job must be forgotten");
    }

    /// The gap this whole module exists to close: `/exec` traffic is the bulk of
    /// what the sidecar does and touches no job.
    #[tokio::test]
    async fn a_one_shot_call_is_announced_at_both_ends() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let id = bus.start(ActivityKind::Exec, "git", &["status".into()], Some("/repo"));
        bus.finish(id, Some(0), None);

        match drain(&mut rx).as_slice() {
            [ServerEvent::ActivityStarted { activity: started }, ServerEvent::ActivityFinished { activity: done }] =>
            {
                assert_eq!(started.cmd, "git");
                assert_eq!(started.cwd.as_deref(), Some("/repo"));
                assert!(started.duration_ms.is_none(), "in flight has no duration");
                assert_eq!(done.id, id);
                assert_eq!(done.exit_code, Some(0));
                assert!(done.duration_ms.is_some());
            }
            other => panic!("expected start then finish, got {:?}", kinds(other)),
        }
    }

    #[tokio::test]
    async fn a_failed_call_carries_its_reason_not_an_exit_code() {
        let bus = EventBus::new();
        let id = bus.start(ActivityKind::Exec, "sudo", &[], None);
        bus.finish(id, None, Some("command not permitted".into()));

        let recorded = recover(bus.activities.read()).front().cloned().unwrap();
        assert_eq!(recorded.error.as_deref(), Some("command not permitted"));
        assert!(recorded.exit_code.is_none());
    }

    /// One sidecar serves every Claude Code session, so a one-shot call has to be
    /// able to say which one it came from — otherwise the whole log is one stream.
    #[tokio::test]
    async fn a_one_shot_call_keeps_its_session() {
        let bus = EventBus::new();
        let id = bus.start_for_session(
            ActivityKind::Exec,
            "git",
            &["status".into()],
            None,
            Some("sess-1"),
        );
        bus.finish(id, Some(0), None);

        let recorded = recover(bus.activities.read()).front().cloned().unwrap();
        assert_eq!(recorded.session_id.as_deref(), Some("sess-1"));
    }

    /// The plain `start` is used by endpoints with no session to report, and must
    /// not invent one.
    #[tokio::test]
    async fn a_call_without_a_session_reports_none() {
        let bus = EventBus::new();
        bus.start(ActivityKind::Browser, "chrome tab", &[], None);

        let recorded = recover(bus.activities.read()).front().cloned().unwrap();
        assert!(recorded.session_id.is_none());
    }

    #[tokio::test]
    async fn the_activity_ring_is_bounded() {
        let bus = EventBus::new();
        for _ in 0..MAX_ACTIVITIES + 50 {
            bus.start(ActivityKind::Exec, "git", &[], None);
        }
        assert_eq!(recover(bus.activities.read()).len(), MAX_ACTIVITIES);
    }

    /// A watcher attaching mid-session must reach current state from the replay
    /// alone, or it would need the poll it is meant to replace.
    #[tokio::test]
    async fn replay_describes_running_and_finished_jobs_and_recent_calls() {
        let (bus, registry) = bus_and_registry();
        let live = registry.create("sbt".into(), vec![], None).unwrap();
        live.push_line("compiling".into());
        let done = registry.create("echo".into(), vec![], None).unwrap();
        done.finish(Outcome::Completed { exit_code: 0 });
        let call = bus.start(ActivityKind::Exec, "git", &[], None);
        bus.finish(call, Some(0), None);

        let replay = bus.replay(&registry);
        let progress = replay.iter().any(|e| {
            matches!(e, ServerEvent::JobProgress { job_id, line_count, .. }
                if job_id == &live.id && *line_count == 1)
        });
        let finished = replay
            .iter()
            .any(|e| matches!(e, ServerEvent::JobFinished { job_id, .. } if job_id == &done.id));
        let activity = replay
            .iter()
            .any(|e| matches!(e, ServerEvent::ActivityFinished { .. }));
        assert!(progress, "the running job's counters must be replayed");
        assert!(finished, "the finished job's outcome must be replayed");
        assert!(activity, "recent one-shot calls must be replayed");
    }

    /// The gap `/logs` closes: a sidecar's own failures went to stderr only,
    /// which is unreachable once the launching terminal is gone.
    #[tokio::test]
    async fn diagnostics_are_retained_and_stamped() {
        clear_diagnostics();
        let d = record_diagnostic("ERROR", "claude_sidecar::job", "spill write failed".into());
        assert_eq!(d.level, "ERROR");
        assert_eq!(d.target, "claude_sidecar::job");
        assert!(!d.at.is_empty(), "must carry a wall-clock stamp");

        let all = diagnostics();
        assert!(all.iter().any(|x| x.message == "spill write failed"));
        clear_diagnostics();
    }

    /// A long-lived daemon must not grow this ring without bound.
    #[tokio::test]
    async fn the_diagnostic_ring_is_bounded() {
        clear_diagnostics();
        for i in 0..MAX_DIAGNOSTICS + 40 {
            record_diagnostic("WARN", "t", format!("problem {i}"));
        }
        let all = diagnostics();
        assert_eq!(all.len(), MAX_DIAGNOSTICS);
        // The newest survive — an old failure matters less than a current one.
        assert_eq!(
            all.last().map(|d| d.message.clone()),
            Some(format!("problem {}", MAX_DIAGNOSTICS + 39))
        );
        clear_diagnostics();
    }

    /// Ids must be unique, since the client dedupes replayed diagnostics by id.
    #[tokio::test]
    async fn diagnostic_ids_are_unique() {
        let a = record_diagnostic("WARN", "t", "one".into());
        let b = record_diagnostic("WARN", "t", "one".into());
        assert_ne!(a.id, b.id, "identical messages still need distinct ids");
    }
}
