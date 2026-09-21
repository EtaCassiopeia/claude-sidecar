use std::convert::Infallible;

use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::{events::ServerEvent, job::now_ms, AppState};

/// `GET /events` — every server-wide event as SSE, so a watcher never polls.
///
/// The stream opens with a replay of current state (each tracked job, then the
/// recent one-shot calls), then forwards live events. Replay overlaps the live
/// stream rather than handing off at a boundary: events are keyed by job id or
/// activity id and applied idempotently, so a duplicate costs nothing while a
/// missed event would leave a watcher permanently wrong.
///
/// Loss is reported, not hidden. A subscriber that falls behind the broadcast
/// channel gets a `lagged` event naming how many it missed; unlike a job's line
/// buffer there is no authoritative history to resync from, so the honest move
/// is to say so and let the client reconcile against `/jobs`.
pub async fn stream(State(state): State<AppState>) -> Response {
    let mut rx = state.events.subscribe();
    // Subscribe before snapshotting, so anything happening during the replay is
    // queued rather than lost.
    let replay = state.events.replay(&state.registry);

    let stream = async_stream::stream! {
        for event in replay {
            yield Ok::<Event, Infallible>(encode(&event));
        }
        yield Ok(ready_event());

        loop {
            match rx.recv().await {
                Ok(event) => yield Ok(encode(&event)),
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("event subscriber lagged by {n} events");
                    yield Ok(lagged_event(n));
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn encode(event: &ServerEvent) -> Event {
    // `ServerEvent` always serializes; fall back rather than panicking on the
    // impossible error.
    Event::default()
        .json_data(event)
        .unwrap_or_else(|_| Event::default().data(r#"{"type":"unencodable"}"#))
}

/// Marks the end of the replay, so a client knows it has reached current state
/// and can drop any "connecting…" indicator.
fn ready_event() -> Event {
    Event::default().data(json!({ "type": "ready", "ts": now_ms() }).to_string())
}

fn lagged_event(dropped: u64) -> Event {
    Event::default()
        .data(json!({ "type": "lagged", "dropped": dropped, "ts": now_ms() }).to_string())
}
