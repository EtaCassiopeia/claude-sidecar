use std::time::Duration;

use futures::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event as SseEvent, EventSource};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct JobSummary {
    pub job_id: String,
    pub cmd: String,
    pub args: Vec<String>,
    /// The client that created the job, when it identified itself.
    #[serde(default)]
    pub session_id: Option<String>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub line_count: usize,
    pub elapsed_ms: u64,
    /// Milliseconds since this job last produced output. A running job silent for
    /// minutes is the wedged-process signature worth surfacing.
    #[serde(default)]
    pub idle_ms: u64,
    /// Absent while the job runs.
    #[serde(default)]
    pub outcome: Option<Outcome>,
}

/// How a job ended. The server tags the enum internally, so the discriminant is
/// a field named `outcome` rather than the object itself.
#[derive(Debug, Clone, Deserialize)]
pub struct Outcome {
    #[serde(rename = "outcome")]
    pub kind: String,
    /// True when the process ignored `SIGTERM` and had to be `SIGKILL`ed.
    #[serde(default)]
    pub escalated: bool,
}

impl JobSummary {
    /// The command line as invoked, for display.
    pub fn command(&self) -> String {
        if self.args.is_empty() {
            self.cmd.clone()
        } else {
            format!("{} {}", self.cmd, self.args.join(" "))
        }
    }

    /// Did this job end for any reason other than running to completion?
    pub fn ended_abnormally(&self) -> bool {
        self.outcome.as_ref().is_some_and(|o| o.kind != "completed")
    }

    /// Short form of the session id for display. Full UUIDs are too wide for a
    /// list, and the first block is already unique in practice.
    pub fn session_short(&self) -> Option<&str> {
        self.session_id.as_deref().map(|s| &s[..s.len().min(8)])
    }
}

/// A one-shot call — `/exec`, one `/batch` step, `/browser/*`, `/gdocs/*`.
///
/// These touch no job, so before `/events` existed they were invisible to the
/// monitor even though they are the bulk of the sidecar's traffic.
#[derive(Debug, Clone, Deserialize)]
pub struct Activity {
    pub id: u64,
    pub kind: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub started_ms: u64,
    /// Absent while the call is in flight.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Set when the call failed to run at all rather than exiting with a code.
    #[serde(default)]
    pub error: Option<String>,
    /// Which Claude Code session made the call, when it said.
    #[serde(default)]
    pub session_id: Option<String>,
}

impl Activity {
    pub fn command(&self) -> String {
        if self.args.is_empty() {
            self.cmd.clone()
        } else {
            format!("{} {}", self.cmd, self.args.join(" "))
        }
    }

    /// Short form of the session id, matching [`JobSummary::session_short`].
    pub fn session_short(&self) -> Option<&str> {
        self.session_id.as_deref().map(|s| &s[..s.len().min(8)])
    }

    pub fn running(&self) -> bool {
        self.duration_ms.is_none()
    }

    pub fn failed(&self) -> bool {
        self.error.is_some() || self.exit_code.is_some_and(|c| c != 0)
    }
}

/// One server-side warning or error, as retained by the sidecar.
#[derive(Debug, Clone, Deserialize)]
pub struct Diagnostic {
    pub id: u64,
    pub at: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// One server-wide event from `/events`.
///
/// Mirrors the server's `ServerEvent` but is deliberately a separate type: the
/// server serializes, the client deserializes, and an unknown `type` is ignored
/// rather than failing the stream — so a newer sidecar cannot break this client.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    JobCreated {
        job_id: String,
        cmd: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
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
        #[serde(default)]
        exit_code: Option<i32>,
        elapsed_ms: u64,
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
    /// A warning or error the sidecar logged about itself. These reach stderr
    /// only, which is unreachable for a daemon whose launching terminal is gone —
    /// so the monitor retains them instead.
    Diagnostic {
        diagnostic: Diagnostic,
    },
    /// End of the opening replay — state is now current.
    Ready,
    /// The stream fell behind and `dropped` events were lost. There is no
    /// authoritative history to resync from, so the client reconciles against
    /// `/jobs` rather than pretending nothing happened.
    Lagged {
        dropped: u64,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthInfo {
    pub status: String,
    pub version: String,
    pub jobs: usize,
}

/// One day of recorded activity, from `/stats`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DayBucket {
    pub date: String,
    pub calls: u64,
    pub failures: u64,
}

impl DayBucket {
    /// Three-letter weekday for the chart's label, derived from the `YYYY-MM-DD`
    /// date. Falls back to the day-of-month if the date cannot be parsed, so a
    /// malformed row still labels its bar.
    pub fn weekday(&self) -> String {
        let parts: Vec<u32> = self
            .date
            .split('-')
            .filter_map(|p| p.parse::<u32>().ok())
            .collect();
        let [y, m, d] = parts[..] else {
            return self.date.clone();
        };
        // Sakamoto's algorithm: no date library needed for a weekday, and these
        // strings are the only reason one would be pulled in.
        const OFFSETS: [u32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
        if !(1..=12).contains(&m) {
            return self.date.clone();
        }
        let y = if m < 3 { y.wrapping_sub(1) } else { y };
        let index = (y + y / 4 - y / 100 + y / 400 + OFFSETS[(m - 1) as usize] + d) % 7;
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][index as usize].to_string()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CommandStat {
    pub cmd: String,
    pub calls: u64,
    #[serde(default)]
    pub p50_ms: Option<u64>,
}

/// The durable metrics summary the stats overlay draws.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct StatsSnapshot {
    /// Oldest first, so the chart reads left to right.
    #[serde(default)]
    pub days: Vec<DayBucket>,
    #[serde(default)]
    pub total_calls: u64,
    #[serde(default)]
    pub commands: Vec<CommandStat>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobLine {
    pub index: usize,
    pub text: String,
    pub ts: u64,
}

/// An event yielded by the SSE stream for a job.
#[derive(Debug)]
pub enum StreamEvent {
    Line(JobLine),
    /// Output was lost before this point — the buffer evicted it, or this
    /// subscriber fell behind. Surfaced so the display can mark the break
    /// instead of showing a silently truncated log.
    Gap {
        dropped: usize,
    },
    Exit {
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct TypedPayload {
    #[serde(rename = "type")]
    event_type: Option<String>,
    exit_code: Option<i32>,
    #[serde(default)]
    dropped: usize,
}

/// Budget for a one-shot fetch (`/health`, `/jobs`). Applied per request rather
/// than on the client, because a client-wide timeout also caps the SSE streams —
/// and those are meant to stay open indefinitely.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct SidecarClient {
    client: Client,
    base_url: String,
}

impl SidecarClient {
    pub fn new(port: u16) -> Self {
        let client = Client::builder()
            // Deliberately no `.timeout()`: it is a *total request* deadline, so
            // it killed `/events` and every job log stream at 10s and made the
            // monitor flash "stream lost" on a perfectly healthy sidecar. Only
            // the connect phase is bounded here; the fetches set their own.
            .connect_timeout(FETCH_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            base_url: format!("http://127.0.0.1:{port}"),
        }
    }

    pub async fn health(&self) -> anyhow::Result<HealthInfo> {
        let info = self
            .client
            .get(format!("{}/health", self.base_url))
            .timeout(FETCH_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json::<HealthInfo>()
            .await?;
        Ok(info)
    }

    pub async fn list_jobs(&self) -> anyhow::Result<Vec<JobSummary>> {
        let jobs = self
            .client
            .get(format!("{}/jobs", self.base_url))
            .timeout(FETCH_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<JobSummary>>()
            .await?;
        Ok(jobs)
    }

    /// The durable metrics summary. Fetched on demand rather than polled: it
    /// reads Parquet off disk, and nothing shows it until the overlay is opened.
    pub async fn stats(&self) -> anyhow::Result<StatsSnapshot> {
        let snapshot = self
            .client
            .get(format!("{}/stats", self.base_url))
            .timeout(FETCH_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json::<StatsSnapshot>()
            .await?;
        Ok(snapshot)
    }

    /// Open the server-wide event stream. Yields until the stream ends.
    ///
    /// This is the primary channel: job lifecycle, counters, and one-shot calls
    /// all arrive here as they happen, so nothing depends on a poll interval.
    /// Unknown event types are skipped rather than ending the stream — a newer
    /// sidecar must not break an older monitor.
    pub fn events(&self) -> impl futures::Stream<Item = anyhow::Result<ServerEvent>> {
        let url = format!("{}/events", self.base_url);
        let mut es = EventSource::new(self.client.get(&url)).expect("valid request builder");

        async_stream::stream! {
            while let Some(event) = es.next().await {
                match event {
                    Ok(SseEvent::Message(msg)) => {
                        match serde_json::from_str::<ServerEvent>(&msg.data) {
                            Ok(parsed) => yield Ok(parsed),
                            // An unrecognized `type` is forward compatibility,
                            // not an error worth surfacing to the user.
                            Err(_) => continue,
                        }
                    }
                    Ok(SseEvent::Open) => {}
                    Err(e) => {
                        yield Err(anyhow::anyhow!("event stream: {e}"));
                        break;
                    }
                }
            }
        }
    }

    /// Open an SSE stream for a job. Yields `StreamEvent`s until the stream ends.
    pub fn stream(&self, job_id: &str) -> impl futures::Stream<Item = anyhow::Result<StreamEvent>> {
        let url = format!("{}/jobs/{}/stream", self.base_url, job_id);
        let rb = self.client.get(&url);
        let mut es = EventSource::new(rb).expect("valid request builder");

        async_stream::stream! {
            while let Some(event) = es.next().await {
                match event {
                    Ok(SseEvent::Message(msg)) => {
                        // Typed events carry a "type" field; anything else is a line.
                        match serde_json::from_str::<TypedPayload>(&msg.data) {
                            Ok(p) if p.event_type.as_deref() == Some("exit") => {
                                yield Ok(StreamEvent::Exit { exit_code: p.exit_code });
                                break;
                            }
                            Ok(p) if p.event_type.as_deref() == Some("gap") => {
                                yield Ok(StreamEvent::Gap { dropped: p.dropped });
                            }
                            _ => {
                                match serde_json::from_str::<JobLine>(&msg.data) {
                                    Ok(line) => yield Ok(StreamEvent::Line(line)),
                                    Err(e) => yield Err(anyhow::anyhow!("sse decode: {e}")),
                                }
                            }
                        }
                    }
                    Ok(SseEvent::Open) => {}
                    Err(e) => {
                        yield Err(anyhow::anyhow!("sse error: {e}"));
                        break;
                    }
                }
            }
        }
    }
}
