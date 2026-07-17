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
    pub running: bool,
    pub exit_code: Option<i32>,
    pub line_count: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthInfo {
    pub status: String,
    pub version: String,
    pub jobs: usize,
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
    Exit { exit_code: Option<i32> },
}

#[derive(Debug, Clone, Deserialize)]
struct TypedPayload {
    #[serde(rename = "type")]
    event_type: Option<String>,
    exit_code: Option<i32>,
}

#[derive(Clone)]
pub struct SidecarClient {
    client: Client,
    base_url: String,
}

impl SidecarClient {
    pub fn new(port: u16) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
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
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<JobSummary>>()
            .await?;
        Ok(jobs)
    }

    pub async fn cancel(&self, job_id: &str) -> anyhow::Result<()> {
        self.client
            .delete(format!("{}/jobs/{}", self.base_url, job_id))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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
                        // Distinguish exit events (have "type":"exit") from line events.
                        match serde_json::from_str::<TypedPayload>(&msg.data) {
                            Ok(p) if p.event_type.as_deref() == Some("exit") => {
                                yield Ok(StreamEvent::Exit { exit_code: p.exit_code });
                                break;
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
