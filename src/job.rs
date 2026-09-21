use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        Arc, PoisonError, RwLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::error::SidecarError;

pub type JobId = String;

/// Unix time in milliseconds. Used for line and event timestamps.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Recover a lock guard even if a previous holder panicked. We never hold these
/// guards across an `.await` and never panic while holding one, so poisoning is
/// not expected — but recovering keeps a stray panic from cascading into 500s.
pub(crate) fn recover<T>(result: Result<T, PoisonError<T>>) -> T {
    result.unwrap_or_else(PoisonError::into_inner)
}

// ─── JobLine ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLine {
    pub index: usize,
    pub text: String,
    /// Unix timestamp in milliseconds.
    pub ts: u64,
}

impl JobLine {
    fn new(index: usize, text: String) -> Self {
        Self {
            index,
            text,
            ts: now_ms(),
        }
    }
}

// ─── Spill (overflow to disk) ─────────────────────────────────────────────────

/// Per-job append-only overflow log. When a job is configured to spill, lines
/// evicted from the in-memory ring are appended here as JSONL instead of being
/// dropped, so `/lines` can serve the complete history.
///
/// The file is append-only, so already-written bytes are immutable: a reader
/// opening its own handle sees a stable prefix regardless of concurrent
/// appends, which is why reads need no file locking. `offsets[i]` is the byte
/// offset of spilled line `i`; the invariant `offsets.len() == first_index`
/// (every evicted line was spilled) holds as long as `enabled` is true.
struct Spill {
    path: PathBuf,
    file: Option<File>,
    offsets: Vec<u64>,
    len: u64,
    /// Cleared if a disk error forces a fallback to drop-mode.
    enabled: bool,
}

impl Spill {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            offsets: Vec::new(),
            len: 0,
            enabled: true,
        }
    }

    /// Append one evicted line. Returns `false` (and disables further spilling)
    /// on any I/O error, so the caller can fall back to dropping.
    fn append(&mut self, line: &JobLine) -> bool {
        if self.file.is_none() {
            match OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)
            {
                Ok(file) => self.file = Some(file),
                Err(e) => {
                    tracing::error!("spill open {}: {e}", self.path.display());
                    self.enabled = false;
                    return false;
                }
            }
        }

        let mut record = match serde_json::to_vec(line) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("spill encode: {e}");
                self.enabled = false;
                return false;
            }
        };
        record.push(b'\n');

        let file = self.file.as_mut().expect("opened above");
        if let Err(e) = file.write_all(&record) {
            tracing::error!("spill write {}: {e}", self.path.display());
            self.enabled = false;
            return false;
        }
        self.offsets.push(self.len);
        self.len += record.len() as u64;
        true
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        // Best-effort cleanup of the per-job overflow file.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read `count` spilled records starting at byte `offset`. Runs on a blocking
/// thread. Returns `None` on any I/O or decode error.
fn read_spilled(path: &Path, offset: u64, count: usize) -> Option<Vec<Arc<JobLine>>> {
    let mut file = File::open(path)
        .map_err(|e| tracing::error!("spill reopen {}: {e}", path.display()))
        .ok()?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| tracing::error!("spill seek: {e}"))
        .ok()?;

    let mut out = Vec::with_capacity(count);
    for line in BufReader::new(file).lines().take(count) {
        let line = line.map_err(|e| tracing::error!("spill read: {e}")).ok()?;
        let parsed = serde_json::from_str::<JobLine>(&line)
            .map_err(|e| tracing::error!("spill decode: {e}"))
            .ok()?;
        out.push(Arc::new(parsed));
    }
    Some(out)
}

// ─── LineBuffer ───────────────────────────────────────────────────────────────

/// A bounded ring buffer of a job's output lines, optionally backed by an
/// on-disk overflow log.
///
/// A build or test run can emit tens of thousands of lines, so we retain only
/// the most recent `cap` in memory. In the default (no-spill) mode the oldest
/// are dropped, bounding memory per job. With a `spill` backing, evicted lines
/// are written to disk instead so nothing is lost. Either way a line's
/// *logical* index (`JobLine.index`) is a monotonic counter independent of its
/// physical slot, so `?from=N` polling stays correct.
struct LineBuffer {
    recent: VecDeque<Arc<JobLine>>,
    /// Total lines ever pushed — also the next line's logical index.
    total: usize,
    cap: usize,
    spill: Option<Spill>,
}

/// How to satisfy a read window: whatever is already in memory, plus an optional
/// spilled range to fetch from disk. Produced under the lock, consumed without
/// it (disk reads happen on a blocking thread).
struct ReadPlan {
    mem_lines: Vec<Arc<JobLine>>,
    disk: Option<DiskRead>,
    next_from: usize,
    dropped: usize,
}

struct DiskRead {
    path: PathBuf,
    offset: u64,
    count: usize,
}

impl LineBuffer {
    fn new(cap: usize, spill_path: Option<PathBuf>) -> Self {
        let cap = cap.max(1);
        Self {
            recent: VecDeque::with_capacity(cap.min(1024)),
            total: 0,
            cap,
            spill: spill_path.map(Spill::new),
        }
    }

    fn push(&mut self, text: String) -> Arc<JobLine> {
        let line = Arc::new(JobLine::new(self.total, text));
        self.total += 1;
        self.recent.push_back(Arc::clone(&line));

        while self.recent.len() > self.cap {
            // Spill the oldest line before dropping it from memory. We read the
            // front first so a failed spill write doesn't advance `first_index`
            // past what's on disk (the append then disables spilling).
            let spilled = match &mut self.spill {
                Some(spill) if spill.enabled => {
                    let front = self.recent.front().expect("len > cap");
                    spill.append(front)
                }
                _ => false,
            };
            let _ = spilled;
            self.recent.pop_front();
        }
        line
    }

    /// Logical index of the oldest in-memory line (== number of evicted lines).
    fn first_index(&self) -> usize {
        self.total - self.recent.len()
    }

    /// Are evicted lines still retrievable (i.e. spilling is active)?
    fn spill_active(&self) -> bool {
        self.spill.as_ref().is_some_and(|s| s.enabled)
    }

    /// Plan a read of up to `max` lines at logical index `from`.
    fn plan(&self, from: usize, max: usize) -> ReadPlan {
        let first = self.first_index();
        let end = (from + max).min(self.total);

        // Dropped is 0 while spilling is active (nothing is unrecoverable);
        // otherwise it's the count of lines evicted from memory.
        let dropped = if self.spill_active() { 0 } else { first };

        if from >= end {
            return ReadPlan {
                mem_lines: Vec::new(),
                disk: None,
                next_from: self.total,
                dropped,
            };
        }

        match &self.spill {
            Some(spill) if spill.enabled && from < first => {
                // Part (or all) of the window is on disk.
                let disk_end = end.min(first);
                let disk = Some(DiskRead {
                    path: spill.path.clone(),
                    offset: spill.offsets[from],
                    count: disk_end - from,
                });
                let mem_lines = if end > first {
                    self.recent.range(0..(end - first)).cloned().collect()
                } else {
                    Vec::new()
                };
                ReadPlan {
                    mem_lines,
                    disk,
                    next_from: end,
                    dropped,
                }
            }
            _ => {
                // Fully served from memory. Clamp `from` up into the retained
                // region (drop-mode) — in spill mode `from >= first` already.
                let start = from.max(first);
                let mem_lines = self
                    .recent
                    .range((start - first)..(end - first))
                    .cloned()
                    .collect();
                ReadPlan {
                    mem_lines,
                    disk: None,
                    next_from: end,
                    dropped,
                }
            }
        }
    }
}

// ─── Outcome & JobState ───────────────────────────────────────────────────────

/// How a job ended. Serialized into status/SSE payloads.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Completed {
        exit_code: i32,
    },
    /// Wall-clock timeout. `escalated` means the process ignored `SIGTERM` and
    /// had to be `SIGKILL`ed, so it did not get to clean up after itself.
    TimedOut {
        escalated: bool,
    },
    /// Canceled via DELETE /jobs/:id before the process finished naturally.
    Canceled {
        escalated: bool,
    },
    /// Killed for producing no output for longer than its idle timeout.
    IdleTimedOut {
        escalated: bool,
    },
    /// The process could not be spawned or the runner failed internally.
    Failed,
}

impl Outcome {
    pub fn exit_code(self) -> Option<i32> {
        match self {
            Outcome::Completed { exit_code } => Some(exit_code),
            _ => None,
        }
    }
}

/// A job is either running or finished. `Copy` so readers get a cheap snapshot
/// without holding the lock.
#[derive(Debug, Clone, Copy)]
pub enum JobState {
    Running,
    Finished {
        outcome: Outcome,
        finished_at: Instant,
    },
}

impl JobState {
    pub fn is_running(self) -> bool {
        matches!(self, JobState::Running)
    }

    pub fn outcome(self) -> Option<Outcome> {
        match self {
            JobState::Finished { outcome, .. } => Some(outcome),
            JobState::Running => None,
        }
    }

    fn finished_at(self) -> Option<Instant> {
        match self {
            JobState::Finished { finished_at, .. } => Some(finished_at),
            JobState::Running => None,
        }
    }
}

// ─── JobEvent ─────────────────────────────────────────────────────────────────

/// Live events broadcast to SSE subscribers. A terminal `Finished` event is
/// what lets watchers learn the job ended immediately — rather than waiting for
/// the broadcast channel to close when the job is finally evicted.
#[derive(Debug, Clone)]
pub enum JobEvent {
    Line(Arc<JobLine>),
    Finished(Outcome),
}

// ─── Job ──────────────────────────────────────────────────────────────────────

/// Broadcast buffer size. A slow SSE subscriber that falls this far behind is
/// dropped (lagged) rather than blocking producers; polling clients still see
/// every line via the retained `lines` buffer.
const BROADCAST_CAPACITY: usize = 1024;

pub struct Job {
    pub id: JobId,
    pub cmd: String,
    pub args: Vec<String>,
    /// Which client asked for this job — a Claude Code session id when the caller
    /// supplied one. Several sessions share one sidecar, so the pid in the logs
    /// cannot tell them apart; this can.
    pub session_id: Option<String>,
    pub started_at: Instant,
    /// Live event fan-out. Kept alive for the job's lifetime.
    events: broadcast::Sender<JobEvent>,
    /// Bounded line history (optionally disk-backed) for late joiners/polling.
    lines: RwLock<LineBuffer>,
    state: RwLock<JobState>,
    /// Registered by `pty::run` once the child PID is known; called by `cancel`.
    /// Returns whether a signal was actually delivered.
    #[allow(clippy::type_complexity)]
    kill_fn: RwLock<Option<Box<dyn Fn() -> bool + Send + Sync>>>,
    /// Set when `cancel()` is called so the runner task can detect it.
    canceled: AtomicBool,
    /// Unix-ms of the most recent output. An atomic rather than a lock because
    /// `push_line` is the highest-frequency write in the system.
    ///
    /// Updated on any received segment, not only on emitted lines: a progress
    /// bar redrawing in place produces no new line (see `pty::LineAssembler`) but
    /// is unambiguous evidence of progress, and treating it as idle would kill
    /// live jobs.
    last_output_ms: AtomicU64,
    /// Child PID once spawned, for users who need to inspect a wedged job.
    pid: AtomicI32,
}

impl Job {
    fn new(
        id: JobId,
        cmd: String,
        args: Vec<String>,
        session_id: Option<String>,
        max_lines: usize,
        spill_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let (events, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            id,
            cmd,
            args,
            session_id,
            started_at: Instant::now(),
            events,
            lines: RwLock::new(LineBuffer::new(max_lines, spill_path)),
            state: RwLock::new(JobState::Running),
            kill_fn: RwLock::new(None),
            canceled: AtomicBool::new(false),
            last_output_ms: AtomicU64::new(now_ms()),
            pid: AtomicI32::new(0),
        })
    }

    /// A standalone job for tests that need to drive a runner without a registry.
    #[cfg(test)]
    pub fn new_for_test(id: &str, max_lines: usize) -> Arc<Self> {
        Self::new(id.to_string(), "test".into(), vec![], None, max_lines, None)
    }

    /// Append a line and broadcast it. Single write-lock — the logical index is
    /// assigned under that same lock, so it cannot race.
    pub fn push_line(&self, text: String) {
        self.mark_output();
        let line = recover(self.lines.write()).push(text);
        // No subscribers is fine.
        let _ = self.events.send(JobEvent::Line(line));
    }

    /// Record that the child produced output just now, without storing a line.
    ///
    /// The runner calls this for in-place redraws too, so idle detection tracks
    /// *activity* rather than line count.
    pub fn mark_output(&self) {
        self.last_output_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Milliseconds since the last output, or since start if there has been none.
    pub fn idle_ms(&self) -> u64 {
        now_ms().saturating_sub(self.last_output_ms.load(Ordering::Relaxed))
    }

    /// Publish the child's PID once known.
    pub fn set_pid(&self, pid: i32) {
        self.pid.store(pid, Ordering::Relaxed);
    }

    /// The child's PID, or `None` before spawn.
    pub fn pid(&self) -> Option<i32> {
        match self.pid.load(Ordering::Relaxed) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// Transition to a terminal state and broadcast the outcome. Idempotent:
    /// the first terminal transition wins.
    pub fn finish(&self, outcome: Outcome) {
        {
            let mut state = recover(self.state.write());
            if !state.is_running() {
                return;
            }
            *state = JobState::Finished {
                outcome,
                finished_at: Instant::now(),
            };
        }
        let _ = self.events.send(JobEvent::Finished(outcome));
    }

    pub fn state(&self) -> JobState {
        *recover(self.state.read())
    }

    /// Total lines the job has produced (including any evicted from memory).
    pub fn line_count(&self) -> usize {
        recover(self.lines.read()).total
    }

    /// Elapsed wall-clock time, frozen once the job finishes.
    pub fn elapsed_ms(&self) -> u64 {
        let end = self.state().finished_at().unwrap_or_else(Instant::now);
        end.saturating_duration_since(self.started_at).as_millis() as u64
    }

    /// A window of up to `max` lines starting at logical index `from`. Returns
    /// the lines, the cursor to poll from next, and how many lines were
    /// unrecoverably dropped before the window (0 in spill mode, or when the
    /// buffer cap was never exceeded).
    ///
    /// Async because spilled lines are read from disk on a blocking thread; the
    /// common in-memory case returns without ever awaiting real work.
    pub async fn read_window(&self, from: usize, max: usize) -> (Vec<Arc<JobLine>>, usize, usize) {
        let ReadPlan {
            mem_lines,
            disk,
            next_from,
            dropped,
        } = recover(self.lines.read()).plan(from, max);

        let mut lines = Vec::new();
        if let Some(d) = disk {
            match tokio::task::spawn_blocking(move || read_spilled(&d.path, d.offset, d.count))
                .await
            {
                Ok(Some(mut disk_lines)) => lines.append(&mut disk_lines),
                Ok(None) => tracing::error!("spilled lines unreadable; returning gap"),
                Err(e) => tracing::error!("spill read task failed: {e}"),
            }
        }
        lines.extend(mem_lines);
        (lines, next_from, dropped)
    }

    /// The next logical line index — the SSE replay/live boundary: history is
    /// `[0, next_index)`, live events are everything at or beyond it.
    pub fn next_index(&self) -> usize {
        recover(self.lines.read()).total
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.events.subscribe()
    }

    /// Register a kill callback (called by `pty::run` once the child PID is
    /// known). Stored as a type-erased fn so `job.rs` doesn't depend on `pty.rs`.
    /// The callback reports whether a signal was actually delivered.
    pub fn arm_kill<F: Fn() -> bool + Send + Sync + 'static>(&self, f: F) {
        *recover(self.kill_fn.write()) = Some(Box::new(f));
    }

    /// Signal the job's process group with SIGKILL and mark it as canceled.
    ///
    /// Returns whether a signal was actually delivered. `false` means either the
    /// job had already finished or the child had not been spawned yet — callers
    /// must not report a cancel as effective on the strength of intent alone.
    pub fn cancel(&self) -> bool {
        if !self.state().is_running() {
            return false;
        }
        self.canceled.store(true, Ordering::SeqCst);
        match recover(self.kill_fn.read()).as_ref() {
            Some(f) => f(),
            None => false,
        }
    }

    pub fn was_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }
}

// ─── JobRegistry ──────────────────────────────────────────────────────────────

/// In-memory registry of live and recently-finished jobs. Reads (polling,
/// streaming, status) take a short read lock; job creation and eviction take a
/// brief write lock. All critical sections are synchronous and lock-free of any
/// `.await`, so a plain `std` `RwLock` is both correct and faster here than an
/// async lock.
pub struct JobRegistry {
    jobs: RwLock<HashMap<JobId, Arc<Job>>>,
    max_jobs: usize,
    max_lines: usize,
    /// Base directory for per-job spill files; `None` disables spilling.
    spill_dir: Option<PathBuf>,
    ttl: Duration,
}

impl JobRegistry {
    pub fn new(max_jobs: usize, max_lines: usize, spill: bool, ttl_secs: u64) -> Arc<Self> {
        let spill_dir = spill.then(Self::init_spill_dir).flatten();
        Arc::new(Self {
            jobs: RwLock::new(HashMap::new()),
            max_jobs,
            max_lines,
            spill_dir,
            ttl: Duration::from_secs(ttl_secs),
        })
    }

    /// Create (once) a process-scoped directory under the system temp dir to
    /// hold per-job overflow files. Returns `None` if it can't be created, in
    /// which case jobs fall back to the in-memory ring buffer.
    fn init_spill_dir() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("claude-sidecar-{}", std::process::id()));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "job output spilling to disk enabled");
                Some(dir)
            }
            Err(e) => {
                tracing::error!(
                    "could not create spill dir {}: {e}; spilling disabled",
                    dir.display()
                );
                None
            }
        }
    }

    pub fn create(
        &self,
        cmd: String,
        args: Vec<String>,
        session_id: Option<String>,
    ) -> Result<Arc<Job>, SidecarError> {
        let mut jobs = recover(self.jobs.write());
        if jobs.len() >= self.max_jobs {
            return Err(SidecarError::TooManyJobs { max: self.max_jobs });
        }
        let id = Uuid::new_v4().to_string();
        let spill_path = self
            .spill_dir
            .as_ref()
            .map(|dir| dir.join(format!("{id}.jsonl")));
        let job = Job::new(
            id.clone(),
            cmd,
            args,
            session_id,
            self.max_lines,
            spill_path,
        );
        jobs.insert(id, Arc::clone(&job));
        Ok(job)
    }

    pub fn get(&self, id: &str) -> Result<Arc<Job>, SidecarError> {
        recover(self.jobs.read())
            .get(id)
            .map(Arc::clone)
            .ok_or_else(|| SidecarError::JobNotFound(id.to_string()))
    }

    pub fn snapshot_ids(&self) -> Vec<JobId> {
        recover(self.jobs.read()).keys().cloned().collect()
    }

    pub fn snapshot(&self) -> Vec<Arc<Job>> {
        recover(self.jobs.read()).values().cloned().collect()
    }

    /// Force a terminal state on jobs whose runner is gone.
    ///
    /// Backstop for the case the per-job supervision cannot cover: if the runner
    /// task is aborted (runtime shutdown) rather than panicking, nothing calls
    /// `finish`, and a job left in `Running` is never evicted — it holds a
    /// `max_jobs` slot for the life of the process and reports as live forever.
    /// A running job whose `Arc` is held only by this registry has no runner.
    fn reap_orphans(self: &Arc<Self>) {
        for job in self.snapshot() {
            // 2 = the registry map + our snapshot clone. Any runner would hold a
            // third. Checked before `is_running` so a job finishing concurrently
            // is simply skipped on this pass.
            if Arc::strong_count(&job) <= 2 && job.state().is_running() {
                tracing::error!(job_id = %job.id, "job runner vanished; marking failed");
                job.finish(Outcome::Failed);
            }
        }
    }

    fn evict_expired(&self) {
        let mut jobs = recover(self.jobs.write());
        jobs.retain(|id, job| match job.state().finished_at() {
            Some(finished_at) if finished_at.elapsed() > self.ttl => {
                tracing::debug!(job_id = %id, "evicted expired job");
                false
            }
            _ => true,
        });
    }

    /// Spawn the background task that periodically evicts finished jobs past TTL.
    pub fn spawn_cleanup(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                self.reap_orphans();
                self.evict_expired();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNBOUNDED: usize = 100_000;

    fn job(cap: usize, spill_path: Option<PathBuf>) -> Arc<Job> {
        Job::new("j".into(), "cargo".into(), vec![], None, cap, spill_path)
    }

    #[tokio::test]
    async fn push_line_assigns_sequential_indices() {
        let job = job(UNBOUNDED, None);
        job.push_line("a".into());
        job.push_line("b".into());
        let (lines, next, dropped) = job.read_window(0, 10).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].index, 0);
        assert_eq!(lines[1].index, 1);
        assert_eq!(lines[0].text, "a");
        assert_eq!(next, 2);
        assert_eq!(dropped, 0);
    }

    #[tokio::test]
    async fn read_window_pages_and_reports_next() {
        let job = job(UNBOUNDED, None);
        for i in 0..5 {
            job.push_line(format!("line{i}"));
        }
        let (lines, next, dropped) = job.read_window(2, 2).await;
        assert_eq!(next, 4);
        assert_eq!(dropped, 0);
        assert_eq!(
            lines.iter().map(|l| l.index).collect::<Vec<_>>(),
            vec![2, 3]
        );

        // `from` past the end yields nothing and clamps `next`.
        let (empty, next, _) = job.read_window(99, 10).await;
        assert!(empty.is_empty());
        assert_eq!(next, 5);
    }

    #[tokio::test]
    async fn ring_buffer_drops_oldest_without_spill() {
        // Cap of 3: after 5 pushes, lines 0 and 1 are dropped (no spill).
        let job = job(3, None);
        for i in 0..5 {
            job.push_line(format!("line{i}"));
        }
        assert_eq!(job.line_count(), 5); // total produced

        let (lines, next, dropped) = job.read_window(0, 10).await;
        assert_eq!(
            lines.iter().map(|l| l.index).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(dropped, 2);
        assert_eq!(next, 5);
    }

    #[test]
    fn next_index_tracks_total_not_buffer_len() {
        // The SSE boundary must be the next *logical* index, so live events
        // aren't mistaken for already-replayed history after eviction.
        let job = job(2, None);
        for i in 0..5 {
            job.push_line(format!("line{i}"));
        }
        assert_eq!(job.next_index(), 5); // 5 produced, even though only 2 retained
    }

    #[tokio::test]
    async fn spill_retains_all_lines_across_the_boundary() {
        let dir = std::env::temp_dir().join(format!("sc-spill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spill-a.jsonl");
        // Cap 3, spill on: 10 lines produced, 7 spill to disk, 3 stay in memory.
        let job = job(3, Some(path.clone()));
        for i in 0..10 {
            job.push_line(format!("line{i}"));
        }
        assert_eq!(job.line_count(), 10);

        // Reading from 0 returns the full history and nothing is dropped, even
        // though only 3 lines are in memory.
        let (lines, next, dropped) = job.read_window(0, 100).await;
        assert_eq!(dropped, 0);
        assert_eq!(next, 10);
        assert_eq!(
            lines.iter().map(|l| l.index).collect::<Vec<_>>(),
            (0..10).collect::<Vec<_>>()
        );
        assert_eq!(lines[0].text, "line0");
        assert_eq!(lines[9].text, "line9");

        // A window straddling the disk/memory boundary is stitched correctly.
        let (straddle, _, _) = job.read_window(6, 3).await;
        assert_eq!(
            straddle.iter().map(|l| l.index).collect::<Vec<_>>(),
            vec![6, 7, 8]
        );

        drop(job); // triggers Spill::drop -> file removal
        assert!(!path.exists(), "spill file should be cleaned up on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn finish_is_idempotent_and_first_wins() {
        let job = job(UNBOUNDED, None);
        assert!(job.state().is_running());
        job.finish(Outcome::Completed { exit_code: 0 });
        job.finish(Outcome::TimedOut { escalated: true }); // ignored
        assert_eq!(job.state().outcome().and_then(Outcome::exit_code), Some(0));
        assert!(!job.state().is_running());
    }

    #[test]
    fn registry_enforces_max_jobs() {
        let reg = JobRegistry::new(1, UNBOUNDED, false, 600);
        assert!(reg.create("cargo".into(), vec![], None).is_ok());
        match reg.create("cargo".into(), vec![], None) {
            Err(SidecarError::TooManyJobs { max }) => assert_eq!(max, 1),
            _ => panic!("expected TooManyJobs error when at capacity"),
        }
    }
}
