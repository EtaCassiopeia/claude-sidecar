//! Durable call metrics: an append-only log, compacted to Parquet, queried with SQL.
//!
//! The sidecar already knew what it was doing — [`crate::events`] publishes every
//! call to whoever is watching — but none of it outlived the process. A restart
//! erased the record, so there was no way to ask "how much did I run this week"
//! or "which command is slowest".
//!
//! # Why two files and not one
//!
//! Parquet is footer-based and immutable: a file is sealed when its metadata is
//! written, so there is no append. Writing one file per call would produce
//! thousands of tiny files, and rewriting a single file per call is quadratic. So
//! calls land in a row-oriented write-ahead log first and are compacted into
//! columnar files on a day boundary — the same WAL-then-compact shape a real
//! lakehouse uses, in miniature.
//!
//! Compaction is what makes the layout useful rather than just tidy: sealed days
//! live at `parquet/date=YYYY-MM-DD/calls.parquet`, a Hive partition layout, so a
//! query filtering on `date` skips whole files without opening them.
//!
//! # What is deliberately not recorded
//!
//! Argument *values*, beyond a subcommand that passes [`safe_subcommand`]. argv
//! routinely carries credentials (`curl -u user:pass`, `gh api -H "Authorization:
//! Bearer …"`, `psql postgres://user:pw@host`), and the in-memory ring this
//! supplements died with the process. On disk it would become a durable secret
//! store, so only the binary name, an allowlisted subcommand, and an argument
//! *count* are persisted.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use datafusion::{
    arrow::{
        array::{Array, Int32Array, Int64Array, StringArray, UInt32Array},
        datatypes::{DataType, Field, Schema, SchemaRef},
        record_batch::RecordBatch,
    },
    datasource::{
        file_format::{options::JsonReadOptions, parquet::ParquetFormat},
        listing::ListingOptions,
    },
    parquet::{
        arrow::ArrowWriter,
        basic::{Compression, ZstdLevel},
        file::properties::WriterProperties,
    },
    prelude::SessionContext,
};
use serde::{Deserialize, Serialize};

use crate::job::recover;

/// How often the rollup task looks for a day that can be sealed. Compaction is
/// cheap and idempotent, so checking hourly costs nothing and means a sidecar
/// left running for weeks still seals each day soon after midnight.
const ROLLUP_INTERVAL: Duration = Duration::from_secs(3600);

/// Longest string kept as a subcommand. Secrets are usually longer than a verb,
/// so this is the cheapest filter that excludes most of them.
const MAX_SUB_LEN: usize = 24;

/// The view every query sees: sealed Parquet days plus the un-compacted tail.
///
/// Columns are listed explicitly rather than `SELECT *` because `UNION ALL` is
/// positional and the two sides do not agree on order — `date` is a partition
/// column on the archive side, so it arrives last, while the log carries it
/// inline.
const CALLS_VIEW: &str = "\
    CREATE OR REPLACE VIEW calls AS \
    SELECT date, ts, kind, cmd, sub, nargs, ms, exit, error, session FROM calls_archive \
    UNION ALL \
    SELECT date, ts, kind, cmd, sub, nargs, ms, exit, error, session FROM calls_log";

/// The view with no sealed days yet, so a first-run query still works.
const CALLS_VIEW_LOG_ONLY: &str = "\
    CREATE OR REPLACE VIEW calls AS \
    SELECT date, ts, kind, cmd, sub, nargs, ms, exit, error, session FROM calls_log";

const CALLS_VIEW_ARCHIVE_ONLY: &str = "\
    CREATE OR REPLACE VIEW calls AS \
    SELECT date, ts, kind, cmd, sub, nargs, ms, exit, error, session FROM calls_archive";

/// One completed call, as persisted.
///
/// `sub` and `nargs` are the whole of what is kept about arguments — see the
/// module docs for why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallRecord {
    /// Local calendar day, `YYYY-MM-DD`. Stored rather than derived so the log
    /// and the partitioned archive expose the same column without either side
    /// needing a timezone-dependent date function at query time.
    pub date: String,
    /// Epoch milliseconds at which the call started.
    pub ts: i64,
    /// Which endpoint produced it: `exec`, `batch`, `browser`, `gdocs`.
    pub kind: String,
    /// The binary, as invoked.
    pub cmd: String,
    /// First argument, when it looks like a subcommand. See [`safe_subcommand`].
    pub sub: Option<String>,
    /// How many arguments the call had, values excluded.
    pub nargs: u32,
    /// Wall-clock duration. Absent if the call never produced one.
    pub ms: Option<i64>,
    pub exit: Option<i32>,
    /// Why the call failed to run at all, when it did not exit with a code.
    pub error: Option<String>,
    /// Claude Code session that made the call, when the caller supplied one.
    /// Absent for callers that do not (a hand-written `curl`, an older hook).
    ///
    /// `#[serde(default)]` so the records written before this column existed
    /// still deserialize — the write-ahead log is read back on every rollup, and
    /// a missing field would otherwise fail the whole compaction.
    #[serde(default)]
    pub session: Option<String>,
}

/// Columns of a sealed Parquet file: every field except `date`, which is carried
/// by the partition directory name instead of being repeated in every row.
fn archive_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("ts", DataType::Int64, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("cmd", DataType::Utf8, false),
        Field::new("sub", DataType::Utf8, true),
        Field::new("nargs", DataType::UInt32, false),
        Field::new("ms", DataType::Int64, true),
        Field::new("exit", DataType::Int32, true),
        Field::new("error", DataType::Utf8, true),
        Field::new("session", DataType::Utf8, true),
    ]))
}

/// Columns of the write-ahead log. Identical to the archive plus the inline
/// `date`, so the two can be unioned into one view.
fn log_schema() -> SchemaRef {
    let mut fields = archive_schema().fields().to_vec();
    fields.push(Arc::new(Field::new("date", DataType::Utf8, false)));
    Arc::new(Schema::new(fields))
}

/// Is `arg` safe to persist as a subcommand?
///
/// Best-effort, and deliberately conservative: a verb like `status` or `validate`
/// is kept, while anything that looks like a path, URL, flag, or credential is
/// dropped. The rules exist to exclude specific observed shapes —
/// `psql postgres://u:pw@host` (scheme punctuation), `curl -H …` (leading dash),
/// `aws --secret-key …` (ditto), and long opaque tokens (length).
///
/// A tool whose first argument is *itself* a short secret would still be
/// recorded; that residual risk is accepted, which is why nothing beyond the
/// first argument is ever considered.
pub fn safe_subcommand(arg: &str) -> Option<String> {
    if arg.is_empty() || arg.len() > MAX_SUB_LEN {
        return None;
    }
    // A subcommand starts with a letter: this drops every flag (`-v`, `--json`),
    // every absolute path, and anything numeric.
    if !arg.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    // Letters, digits, and the punctuation real subcommands use. Excluding `/`,
    // `@`, `:`, and `.` is what rejects paths, URLs, and user@host.
    if !arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    Some(arg.to_string())
}

/// Local calendar day for an epoch-millisecond stamp, as `YYYY-MM-DD`.
///
/// Local rather than UTC for the same reason the request log is: these buckets
/// are read as "what did I do on Tuesday", which a UTC boundary splits in the
/// middle of the user's evening.
pub fn local_date(ts_ms: i64) -> String {
    let secs = (ts_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes into our owned, zeroed `tm` and reads `secs`
    // by pointer; both outlive the call. Null on failure, checked below.
    if unsafe { libc::localtime_r(&secs, &mut tm).is_null() } {
        // Fall back to UTC arithmetic rather than inventing a date.
        let days = ts_ms.div_euclid(86_400_000);
        return format!("epoch+{days}");
    }
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

// ─── Recorder ─────────────────────────────────────────────────────────────────

/// Owns the write-ahead log and the Parquet archive beside it.
pub struct MetricsRecorder {
    root: PathBuf,
    /// Serializes appends against compaction, which rewrites the same file.
    log: Mutex<()>,
}

impl MetricsRecorder {
    /// Open (creating if needed) the metrics directory at `root`.
    pub fn open(root: PathBuf) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(root.join("parquet"))?;
        Ok(Arc::new(Self {
            root,
            log: Mutex::new(()),
        }))
    }

    /// The default location, `~/.config/claude-sidecar/metrics`.
    pub fn default_root() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("claude-sidecar").join("metrics"))
    }

    fn log_path(&self) -> PathBuf {
        self.root.join("calls.jsonl")
    }

    fn archive_dir(&self) -> PathBuf {
        self.root.join("parquet")
    }

    /// Append one record. Failures are logged and dropped: metrics are
    /// observability, and a full disk must not fail the call being measured.
    pub fn record(&self, record: &CallRecord) {
        if let Err(e) = self.try_record(record) {
            tracing::warn!("metrics: could not record call: {e}");
        }
    }

    fn try_record(&self, record: &CallRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        let _guard = recover(self.log.lock());
        // One `write_all` of a complete line, so a concurrent reader sees whole
        // records: short appends to an O_APPEND handle do not interleave.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())?
            .write_all(line.as_bytes())
    }

    /// Read the log back. Malformed lines are skipped rather than failing the
    /// read — a torn final write costs one record, not the whole history.
    fn read_log(&self) -> std::io::Result<Vec<CallRecord>> {
        let path = self.log_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&path)?;
        Ok(BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str::<CallRecord>(&l).ok())
            .collect())
    }

    /// Seal every complete day in the log into a Parquet partition.
    ///
    /// `today` is passed rather than read from the clock so the boundary is
    /// testable. Records for `today` stay in the log, since more are coming.
    /// Returns the dates sealed.
    pub fn compact(&self, today: &str) -> std::io::Result<Vec<String>> {
        let _guard = recover(self.log.lock());
        let records = self.read_log()?;
        if records.is_empty() {
            return Ok(Vec::new());
        }

        let mut by_date: BTreeMap<String, Vec<CallRecord>> = BTreeMap::new();
        let mut keep = Vec::new();
        for record in records {
            if record.date.as_str() < today {
                by_date.entry(record.date.clone()).or_default().push(record);
            } else {
                keep.push(record);
            }
        }
        if by_date.is_empty() {
            return Ok(Vec::new());
        }

        let mut sealed = Vec::new();
        for (date, day) in by_date {
            let dir = self.archive_dir().join(format!("date={date}"));
            std::fs::create_dir_all(&dir)?;
            // A partition may already hold a file: the machine was off across a
            // day boundary and this is a catch-up pass, or the clock moved
            // backwards. Parquet cannot be appended to, and overwriting would
            // silently drop the rows already sealed — so add another file
            // alongside. A partition is a *set* of files to every reader, which
            // is what makes this safe rather than a workaround.
            write_parquet(&next_partition_file(&dir)?, &day)?;
            sealed.push(date);
        }

        // Rewrite the log with only what is still open. Written to a sibling and
        // renamed so a crash mid-write cannot truncate the history that was not
        // yet sealed.
        let tmp = self.root.join("calls.jsonl.tmp");
        {
            let mut out = BufWriter::new(File::create(&tmp)?);
            for record in &keep {
                let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
                out.write_all(line.as_bytes())?;
                out.write_all(b"\n")?;
            }
            out.flush()?;
        }
        std::fs::rename(&tmp, self.log_path())?;
        Ok(sealed)
    }

    /// Register the archive and the log, then a `calls` view over both.
    ///
    /// Either side may be missing — a first run has no sealed days, and a
    /// freshly-compacted sidecar has an empty log — so the view is built from
    /// whichever exist.
    pub async fn context(&self) -> anyhow::Result<SessionContext> {
        let ctx = SessionContext::new();

        let archive = self.archive_dir();
        let has_archive = std::fs::read_dir(&archive)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("date="))
            })
            .unwrap_or(false);
        if has_archive {
            let options = ListingOptions::new(Arc::new(ParquetFormat::default()))
                .with_file_extension(".parquet")
                // Declaring the partition column is what buys pruning: a query
                // filtering on `date` skips non-matching directories without
                // opening the files inside them.
                .with_table_partition_cols(vec![("date".to_string(), DataType::Utf8)]);
            ctx.register_listing_table(
                "calls_archive",
                archive.to_string_lossy().as_ref(),
                options,
                Some(archive_schema()),
                None,
            )
            .await?;
        }

        let log = self.log_path();
        let has_log = log.metadata().map(|m| m.len() > 0).unwrap_or(false);
        if has_log {
            // An explicit schema, not inference: a column that happens to be all
            // null in the current log (no failures yet, say) would otherwise be
            // typed `Null` and fail to union with its archived counterpart.
            let schema = log_schema();
            let options = JsonReadOptions::default()
                .file_extension(".jsonl")
                .schema(&schema);
            ctx.register_json("calls_log", log.to_string_lossy().as_ref(), options)
                .await?;
        }

        let view = match (has_archive, has_log) {
            (true, true) => CALLS_VIEW,
            (true, false) => CALLS_VIEW_ARCHIVE_ONLY,
            (false, true) => CALLS_VIEW_LOG_ONLY,
            // Nothing recorded yet. An empty table still answers queries, which
            // beats every caller special-casing a missing one.
            (false, false) => {
                ctx.register_batch("calls", RecordBatch::new_empty(log_schema()))?;
                return Ok(ctx);
            }
        };
        ctx.sql(view).await?;
        Ok(ctx)
    }

    /// Run one SQL statement against the `calls` view and render it as a table.
    pub async fn query(&self, sql: &str) -> anyhow::Result<String> {
        let ctx = self.context().await?;
        let batches = ctx.sql(sql).await?.collect().await?;
        Ok(datafusion::arrow::util::pretty::pretty_format_batches(&batches)?.to_string())
    }

    /// The aggregates the monitor's overlay draws.
    pub async fn snapshot(&self, days: usize) -> anyhow::Result<StatsSnapshot> {
        let ctx = self.context().await?;

        let per_day = ctx
            .sql(
                "SELECT date, \
                 count(*) AS calls, \
                 sum(CASE WHEN error IS NOT NULL OR exit <> 0 THEN 1 ELSE 0 END) AS failures \
                 FROM calls GROUP BY date ORDER BY date DESC",
            )
            .await?
            .collect()
            .await?;

        let mut buckets = Vec::new();
        for batch in &per_day {
            let dates = column::<StringArray>(batch, "date")?;
            let calls = column::<Int64Array>(batch, "calls")?;
            let failures = column::<Int64Array>(batch, "failures")?;
            for i in 0..batch.num_rows() {
                buckets.push(DayBucket {
                    date: dates.value(i).to_string(),
                    calls: calls.value(i).max(0) as u64,
                    failures: if failures.is_null(i) {
                        0
                    } else {
                        failures.value(i).max(0) as u64
                    },
                });
            }
        }
        // Newest `days` days, then back into calendar order for display.
        buckets.truncate(days);
        buckets.reverse();

        let top = ctx
            .sql(
                "SELECT cmd, count(*) AS n, \
                 CAST(approx_percentile_cont(ms, 0.5) AS BIGINT) AS p50 \
                 FROM calls GROUP BY cmd ORDER BY n DESC, cmd LIMIT 8",
            )
            .await?
            .collect()
            .await?;

        let mut commands = Vec::new();
        for batch in &top {
            let cmds = column::<StringArray>(batch, "cmd")?;
            let n = column::<Int64Array>(batch, "n")?;
            let p50 = column::<Int64Array>(batch, "p50")?;
            for i in 0..batch.num_rows() {
                commands.push(CommandStat {
                    cmd: cmds.value(i).to_string(),
                    calls: n.value(i).max(0) as u64,
                    p50_ms: (!p50.is_null(i)).then(|| p50.value(i).max(0) as u64),
                });
            }
        }

        Ok(StatsSnapshot {
            total_calls: buckets.iter().map(|b| b.calls).sum(),
            days: buckets,
            commands,
        })
    }

    /// Seal completed days at startup and then hourly.
    ///
    /// The startup pass is the one that matters on a laptop: the machine is off
    /// or asleep most of the time, so the hourly tick rarely coincides with a
    /// midnight boundary. `tokio::interval` fires immediately on its first tick,
    /// which is what makes the backlog get swept as soon as the sidecar starts.
    pub fn spawn_rollup(self: &Arc<Self>) {
        let recorder = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(ROLLUP_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let recorder = Arc::clone(&recorder);
                // Compaction reads and rewrites files, so it runs on the blocking
                // pool rather than stalling a runtime worker.
                let sealed = tokio::task::spawn_blocking(move || {
                    recorder.compact(&local_date(crate::job::now_ms() as i64))
                })
                .await;
                match sealed {
                    Ok(Ok(dates)) if !dates.is_empty() => {
                        tracing::info!(
                            "metrics: sealed {} day(s): {}",
                            dates.len(),
                            dates.join(", ")
                        )
                    }
                    Ok(Err(e)) => tracing::warn!("metrics: compaction failed: {e}"),
                    _ => {}
                }
            }
        });
    }
}

/// One `Bar` of the daily chart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DayBucket {
    pub date: String,
    pub calls: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandStat {
    pub cmd: String,
    pub calls: u64,
    pub p50_ms: Option<u64>,
}

/// What `GET /stats` serves and the overlay draws.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StatsSnapshot {
    /// Oldest first, so the chart reads left-to-right.
    pub days: Vec<DayBucket>,
    pub total_calls: u64,
    pub commands: Vec<CommandStat>,
}

/// Fetch a column by name and downcast it, naming the column when either fails.
///
/// Positional access would silently read the wrong column if a query's projection
/// ever changed, which is a much harder failure to notice than an error here.
fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> anyhow::Result<&'a T> {
    let array = batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("query result has no column {name}"))?;
    array
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| anyhow::anyhow!("column {name} has unexpected type {:?}", array.data_type()))
}

/// The next free file name in a partition directory.
///
/// Sealing a day usually writes `calls.parquet`. It may already exist — the
/// machine was asleep across midnight and this is a catch-up pass — and since
/// Parquet has no append, the second batch becomes `calls-1.parquet`. The
/// listing table reads every `.parquet` in the directory, so all of them count.
fn next_partition_file(dir: &Path) -> std::io::Result<PathBuf> {
    let first = dir.join("calls.parquet");
    if !first.exists() {
        return Ok(first);
    }
    // Bounded: a directory this full means something is wrong, and spinning
    // forever would be worse than reporting it.
    for n in 1..10_000 {
        let candidate = dir.join(format!("calls-{n}.parquet"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::other(format!(
        "partition {} already holds 10000 files",
        dir.display()
    )))
}

/// Write one sealed day. zstd because these files are written once and read many
/// times, and the columns (a handful of repeated command names) compress hard.
fn write_parquet(path: &Path, records: &[CallRecord]) -> std::io::Result<()> {
    let schema = archive_schema();
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int64Array::from_iter_values(records.iter().map(|r| r.ts))),
            Arc::new(StringArray::from_iter_values(
                records.iter().map(|r| r.kind.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                records.iter().map(|r| r.cmd.as_str()),
            )),
            Arc::new(StringArray::from_iter(
                records.iter().map(|r| r.sub.as_deref()),
            )),
            Arc::new(UInt32Array::from_iter_values(
                records.iter().map(|r| r.nargs),
            )),
            Arc::new(Int64Array::from_iter(records.iter().map(|r| r.ms))),
            Arc::new(Int32Array::from_iter(records.iter().map(|r| r.exit))),
            Arc::new(StringArray::from_iter(
                records.iter().map(|r| r.error.as_deref()),
            )),
            Arc::new(StringArray::from_iter(
                records.iter().map(|r| r.session.as_deref()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;

    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(3).map_err(std::io::Error::other)?,
        ))
        .build();

    // Written beside the target and renamed, so a reader never sees a Parquet
    // file without its footer — which is unreadable, not merely short.
    let tmp = path.with_extension("parquet.tmp");
    let file = File::create(&tmp)?;
    let mut writer =
        ArrowWriter::try_new(file, schema, Some(properties)).map_err(std::io::Error::other)?;
    writer.write(&batch).map_err(std::io::Error::other)?;
    writer.close().map_err(std::io::Error::other)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(date: &str, ts: i64, cmd: &str, sub: Option<&str>, ms: i64, exit: i32) -> CallRecord {
        CallRecord {
            date: date.into(),
            ts,
            kind: "exec".into(),
            cmd: cmd.into(),
            sub: sub.map(str::to_string),
            nargs: 1,
            ms: Some(ms),
            exit: Some(exit),
            error: None,
            session: None,
        }
    }

    fn recorder() -> (Arc<MetricsRecorder>, PathBuf) {
        // A per-test directory: these write real files, and a shared one would
        // make the tests order-dependent.
        let unique = format!(
            "sidecar-metrics-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let dir = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_dir_all(&dir);
        (MetricsRecorder::open(dir.clone()).unwrap(), dir)
    }

    /// The whole point of the privacy rule: a bearer token passed as argv must
    /// not become a durable on-disk record.
    #[test]
    fn credential_shaped_arguments_are_never_kept() {
        for arg in [
            "-H",
            "--secret-key",
            "postgres://user:pw@host/db",
            "https://example.com/?token=abc",
            "/Users/me/.ssh/id_rsa",
            "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
            "user@host",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ] {
            assert_eq!(safe_subcommand(arg), None, "{arg} must not be persisted");
        }
    }

    /// The counterpart: real subcommands are what make the numbers useful, so
    /// they have to survive the filter.
    #[test]
    fn real_subcommands_are_kept() {
        for arg in ["status", "validate", "test", "api", "pr", "build", "run"] {
            assert_eq!(
                safe_subcommand(arg).as_deref(),
                Some(arg),
                "{arg} should be kept"
            );
        }
    }

    #[test]
    fn a_recorded_call_reads_back_identically() {
        let (recorder, _dir) = recorder();
        let want = record("2026-09-18", 1_758_214_003_221, "git", Some("status"), 8, 0);
        recorder.record(&want);
        assert_eq!(recorder.read_log().unwrap(), vec![want]);
    }

    /// A torn final write (a crash mid-append) must cost one record, not the
    /// whole history — which is the reason for a line-oriented log.
    #[test]
    fn a_truncated_trailing_line_is_skipped() {
        let (recorder, _dir) = recorder();
        recorder.record(&record("2026-09-18", 1, "git", Some("status"), 8, 0));
        // Simulate a partial append.
        let mut f = OpenOptions::new()
            .append(true)
            .open(recorder.log_path())
            .unwrap();
        f.write_all(b"{\"date\":\"2026-09-18\",\"ts\":2,\"cm")
            .unwrap();
        drop(f);

        let read = recorder.read_log().unwrap();
        assert_eq!(read.len(), 1, "the intact record must survive");
        assert_eq!(read[0].ts, 1);
    }

    /// Compaction's contract: completed days move to Parquet, today stays in the
    /// log because more calls are coming.
    #[test]
    fn compaction_seals_past_days_and_keeps_today() {
        let (recorder, dir) = recorder();
        recorder.record(&record("2026-09-16", 1, "git", Some("status"), 8, 0));
        recorder.record(&record("2026-09-17", 2, "sbt", Some("test"), 500, 0));
        recorder.record(&record("2026-09-18", 3, "gh", Some("pr"), 40, 0));

        let sealed = recorder.compact("2026-09-18").unwrap();
        assert_eq!(sealed, vec!["2026-09-16", "2026-09-17"]);

        for date in &sealed {
            assert!(
                dir.join("parquet")
                    .join(format!("date={date}"))
                    .join("calls.parquet")
                    .exists(),
                "{date} should be sealed into a partition"
            );
        } // Only today is left open.
        let remaining = recorder.read_log().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].date, "2026-09-18");
    }

    /// Running twice must not duplicate or lose anything — the rollup task fires
    /// hourly and most runs have nothing to do.
    #[test]
    fn compaction_is_idempotent() {
        let (recorder, _dir) = recorder();
        recorder.record(&record("2026-09-17", 1, "git", Some("status"), 8, 0));
        assert_eq!(recorder.compact("2026-09-18").unwrap().len(), 1);
        assert!(
            recorder.compact("2026-09-18").unwrap().is_empty(),
            "a second pass has nothing left to seal"
        );
        assert!(recorder.read_log().unwrap().is_empty());
    }

    /// No temp files may be left behind: `.parquet.tmp` in a partition directory
    /// would be picked up by the listing table and fail the read.
    #[test]
    fn compaction_leaves_no_temporary_files() {
        let (recorder, dir) = recorder();
        recorder.record(&record("2026-09-17", 1, "git", Some("status"), 8, 0));
        recorder.compact("2026-09-18").unwrap();

        assert!(!dir.join("calls.jsonl.tmp").exists());
        let partition = dir.join("parquet").join("date=2026-09-17");
        let stray: Vec<_> = std::fs::read_dir(&partition)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "left temp files behind: {stray:?}");
    }

    /// A query must span both halves of the store, or every number is wrong for
    /// the rest of the day after a compaction.
    #[tokio::test]
    async fn queries_span_sealed_days_and_the_open_log() {
        let (recorder, _dir) = recorder();
        recorder.record(&record("2026-09-17", 1, "git", Some("status"), 8, 0));
        recorder.record(&record("2026-09-18", 2, "sbt", Some("test"), 500, 0));
        recorder.compact("2026-09-18").unwrap();

        let snapshot = recorder.snapshot(7).await.unwrap();
        assert_eq!(snapshot.total_calls, 2, "{snapshot:?}");
        let dates: Vec<&str> = snapshot.days.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, vec!["2026-09-17", "2026-09-18"], "oldest first");
    }

    /// A first run has no Parquet and no log, and must still answer rather than
    /// making every caller handle a missing store.
    #[tokio::test]
    async fn an_empty_store_answers_with_zeroes() {
        let (recorder, _dir) = recorder();
        let snapshot = recorder.snapshot(7).await.unwrap();
        assert_eq!(snapshot.total_calls, 0);
        assert!(snapshot.days.is_empty());
    }

    /// Failures are what you open the overlay for, so they must be counted from
    /// both a non-zero exit and a call that never ran.
    #[tokio::test]
    async fn failures_count_both_bad_exits_and_errors() {
        let (recorder, _dir) = recorder();
        recorder.record(&record("2026-09-18", 1, "git", Some("status"), 8, 0));
        recorder.record(&record("2026-09-18", 2, "sbt", Some("test"), 9, 2));
        recorder.record(&CallRecord {
            error: Some("command not allowed: sudo".into()),
            exit: None,
            ..record("2026-09-18", 3, "sudo", None, 0, 0)
        });

        let snapshot = recorder.snapshot(7).await.unwrap();
        assert_eq!(snapshot.days.len(), 1);
        assert_eq!(snapshot.days[0].calls, 3);
        assert_eq!(snapshot.days[0].failures, 2, "{:?}", snapshot.days);
    }

    /// The overlay shows a handful of days; a month of history must not stretch
    /// the chart past its panel.
    #[tokio::test]
    async fn only_the_requested_number_of_days_is_returned() {
        let (recorder, _dir) = recorder();
        for day in 1..=20 {
            recorder.record(&record(
                &format!("2026-09-{day:02}"),
                day as i64,
                "git",
                Some("status"),
                8,
                0,
            ));
        }
        let snapshot = recorder.snapshot(7).await.unwrap();
        assert_eq!(snapshot.days.len(), 7);
        // The newest seven, in calendar order.
        assert_eq!(snapshot.days.first().unwrap().date, "2026-09-14");
        assert_eq!(snapshot.days.last().unwrap().date, "2026-09-20");
    }

    /// Ad-hoc SQL is the reason for the query engine; it must reach both halves
    /// and support the aggregates hand-rolled code would have cost.
    #[tokio::test]
    async fn ad_hoc_sql_can_aggregate_across_the_whole_store() {
        let (recorder, _dir) = recorder();
        recorder.record(&record("2026-09-17", 1, "sbt", Some("validate"), 42_100, 0));
        recorder.record(&record("2026-09-18", 2, "sbt", Some("test"), 18_400, 1));
        recorder.compact("2026-09-18").unwrap();

        let table = recorder
            .query(
                "SELECT cmd || ' ' || sub AS command, \
                 CAST(approx_percentile_cont(ms, 0.95) AS BIGINT) AS p95 \
                 FROM calls GROUP BY command ORDER BY command",
            )
            .await
            .unwrap();
        assert!(table.contains("sbt validate"), "{table}");
        assert!(table.contains("sbt test"), "{table}");
    }

    /// Dates are bucket keys and sort keys, so the format has to be exactly
    /// `YYYY-MM-DD` — lexical order is what makes `date < today` correct.
    #[test]
    fn a_local_date_is_zero_padded_and_sortable() {
        let date = local_date(crate::job::now_ms() as i64);
        assert_eq!(date.len(), 10, "{date}");
        let parts: Vec<&str> = date.split('-').collect();
        assert_eq!(parts.len(), 3, "{date}");
        assert_eq!((parts[0].len(), parts[1].len(), parts[2].len()), (4, 2, 2));
    }

    /// The machine was off for a week. Nothing ran, so nothing compacted — and
    /// the backlog must seal on the next startup rather than needing one run per
    /// missed day. This is why compaction sweeps *every* past date in the log
    /// instead of only yesterday.
    #[tokio::test]
    async fn a_week_of_downtime_seals_in_one_pass() {
        let (recorder, dir) = recorder();
        for day in 10..=16 {
            recorder.record(&record(
                &format!("2026-09-{day}"),
                day as i64,
                "git",
                Some("status"),
                8,
                0,
            ));
        }
        // First run after the laptop comes back.
        let sealed = recorder.compact("2026-09-17").unwrap();
        assert_eq!(
            sealed.len(),
            7,
            "every missed day seals at once: {sealed:?}"
        );
        for day in 10..=16 {
            assert!(dir
                .join("parquet")
                .join(format!("date=2026-09-{day}"))
                .exists());
        }
        // And no rows were lost on the way through.
        assert_eq!(recorder.snapshot(30).await.unwrap().total_calls, 7);
    }

    /// Asleep across midnight: today's calls were logged, the machine suspended
    /// before the hourly tick, and it woke on a later day. The partition for that
    /// day may already exist from an earlier pass — Parquet cannot be appended to,
    /// so the rows must go to a second file rather than overwriting the first.
    #[tokio::test]
    async fn sealing_a_day_twice_keeps_both_batches() {
        let (recorder, dir) = recorder();
        recorder.record(&record("2026-09-17", 1, "git", Some("status"), 8, 0));
        recorder.compact("2026-09-18").unwrap();

        // A late-arriving record for the same, already-sealed day.
        recorder.record(&record("2026-09-17", 2, "sbt", Some("test"), 99, 0));
        recorder.compact("2026-09-18").unwrap();

        let partition = dir.join("parquet").join("date=2026-09-17");
        let files: Vec<String> = std::fs::read_dir(&partition)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".parquet"))
            .collect();
        assert_eq!(files.len(), 2, "both batches must survive: {files:?}");

        // The query side must see every file in the partition, not just the first.
        let snapshot = recorder.snapshot(7).await.unwrap();
        assert_eq!(snapshot.total_calls, 2, "{snapshot:?}");
        assert_eq!(snapshot.days.len(), 1, "still one day");
        assert_eq!(snapshot.days[0].calls, 2);
    }

    /// Nothing is lost by never compacting at all: an un-sealed log is queried
    /// directly. A machine that is only ever on for minutes at a time still
    /// reports correct numbers — compaction is an optimization, not the
    /// write path.
    #[tokio::test]
    async fn an_uncompacted_log_is_fully_queryable() {
        let (recorder, dir) = recorder();
        for day in 10..=16 {
            recorder.record(&record(
                &format!("2026-09-{day}"),
                day as i64,
                "git",
                Some("status"),
                8,
                0,
            ));
        }
        // Deliberately no compact() call.
        assert!(
            !dir.join("parquet").join("date=2026-09-10").exists(),
            "precondition: nothing sealed"
        );
        let snapshot = recorder.snapshot(30).await.unwrap();
        assert_eq!(snapshot.total_calls, 7);
        assert_eq!(snapshot.days.len(), 7);
    }

    /// Long-running jobs are the slow work most worth measuring, and they never
    /// pass through the one-shot call path — so they get their own `kind`, and a
    /// killed job (no exit code) must still record *why* it ended.
    #[tokio::test]
    async fn job_records_carry_an_outcome_when_they_have_no_exit_code() {
        let (recorder, _dir) = recorder();
        recorder.record(&CallRecord {
            kind: "job".into(),
            cmd: "sbt".into(),
            sub: Some("validate".into()),
            exit: None,
            error: Some("timedout".into()),
            ms: Some(3_600_000),
            ..record("2026-09-18", 1, "sbt", Some("validate"), 0, 0)
        });
        recorder.record(&CallRecord {
            kind: "job".into(),
            ..record("2026-09-18", 2, "cargo", Some("test"), 45_000, 0)
        });

        let table = recorder
            .query("SELECT kind, cmd, exit, error FROM calls ORDER BY ts")
            .await
            .unwrap();
        assert!(table.contains("job"), "{table}");
        assert!(table.contains("timedout"), "{table}");

        // A job with no exit code is still a failure for the daily count.
        let snapshot = recorder.snapshot(7).await.unwrap();
        assert_eq!(snapshot.days[0].calls, 2);
        assert_eq!(snapshot.days[0].failures, 1, "{:?}", snapshot.days);
    }

    /// Calls can be grouped by originating session — the reason the column exists.
    #[tokio::test]
    async fn calls_can_be_grouped_by_session() {
        let (recorder, _dir) = recorder();
        recorder.record(&CallRecord {
            session: Some("sess-a".into()),
            ..record("2026-09-18", 1, "git", Some("status"), 10, 0)
        });
        recorder.record(&CallRecord {
            session: Some("sess-a".into()),
            ..record("2026-09-18", 2, "cargo", Some("build"), 20, 0)
        });
        recorder.record(&CallRecord {
            session: Some("sess-b".into()),
            ..record("2026-09-18", 3, "git", Some("log"), 30, 0)
        });

        let table = recorder
            .query("SELECT session, count(*) AS n FROM calls GROUP BY session ORDER BY session")
            .await
            .unwrap();
        assert!(table.contains("sess-a"), "{table}");
        assert!(table.contains("sess-b"), "{table}");
    }

    /// Rows written before the `session` column existed must still load. The
    /// write-ahead log is re-read on every rollup, so a missing field failing to
    /// deserialize would break compaction for the whole day, not just one row.
    #[test]
    fn a_record_without_a_session_still_deserializes() {
        let legacy = r#"{"date":"2026-09-18","ts":1,"kind":"exec","cmd":"git",
            "sub":"status","nargs":1,"ms":10,"exit":0,"error":null}"#;
        let parsed: CallRecord = serde_json::from_str(legacy).expect("legacy record");
        assert_eq!(parsed.cmd, "git");
        assert!(parsed.session.is_none());
    }

    /// A sealed day written before the column existed is read back with `session`
    /// null rather than failing the scan — the archive schema supplies the column
    /// even when the file on disk lacks it.
    #[tokio::test]
    async fn an_archive_without_the_session_column_still_queries() {
        let (recorder, dir) = recorder();
        // A Parquet file with the pre-session column set.
        let legacy_schema = Arc::new(Schema::new(vec![
            Field::new("ts", DataType::Int64, false),
            Field::new("kind", DataType::Utf8, false),
            Field::new("cmd", DataType::Utf8, false),
            Field::new("sub", DataType::Utf8, true),
            Field::new("nargs", DataType::UInt32, false),
            Field::new("ms", DataType::Int64, true),
            Field::new("exit", DataType::Int32, true),
            Field::new("error", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&legacy_schema),
            vec![
                Arc::new(Int64Array::from_iter_values([1i64])),
                Arc::new(StringArray::from_iter_values(["exec"])),
                Arc::new(StringArray::from_iter_values(["git"])),
                Arc::new(StringArray::from_iter([Some("status")])),
                Arc::new(UInt32Array::from_iter_values([1u32])),
                Arc::new(Int64Array::from_iter([Some(10i64)])),
                Arc::new(Int32Array::from_iter([Some(0i32)])),
                Arc::new(StringArray::from_iter([None::<&str>])),
            ],
        )
        .unwrap();

        let day = dir.join("parquet").join("date=2026-09-17");
        std::fs::create_dir_all(&day).unwrap();
        let file = File::create(day.join("calls.parquet")).unwrap();
        let mut writer = ArrowWriter::try_new(file, legacy_schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let table = recorder
            .query("SELECT cmd, session FROM calls ORDER BY ts")
            .await
            .unwrap();
        assert!(table.contains("git"), "{table}");
    }
}
