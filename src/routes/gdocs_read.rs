//! `POST /gdocs/read` — read a Google Doc/Sheet/Slides as text.
//! `GET  /gdocs/list` — index the Google Drive mount.
//!
//! A document is named one of three ways, checked in this order: `doc_id`,
//! `url`, or `title` (resolved against the Drive mount). Exactly one is
//! required — guessing between them would risk reading the wrong document.

use std::time::Instant;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::SidecarError,
    events::ActivityKind,
    gdocs::{
        drive::{self, DocKind, DriveDoc},
        export::{self, Export, Format},
    },
    logger, AppState,
};

#[derive(Debug, Deserialize)]
pub struct ReadRequest {
    /// Drive document id — the most direct form.
    pub doc_id: Option<String>,
    /// A `docs.google.com` URL; the id and document type come from it.
    pub url: Option<String>,
    /// Document title, resolved against the local Drive mount. An ambiguous
    /// title is an error listing the candidates.
    pub title: Option<String>,
    /// Defaults per document type: `md` for Docs, `csv` for Sheets, `txt` for
    /// Slides.
    pub format: Option<Format>,
    /// Needed with `doc_id` when the document is not a Doc; ignored when `url`
    /// or `title` already determines the type.
    pub kind: Option<DocKind>,
    /// Seconds to wait for the origin page to load (default 30, cap 180).
    pub wait_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub docs: Vec<DriveDoc>,
    pub count: usize,
    /// True when a traversal cap was hit, so absence from `docs` is not proof
    /// a document doesn't exist.
    pub truncated: bool,
    /// Mount directories that were indexed.
    pub mounts: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Case-insensitive substring filter on the title.
    pub filter: Option<String>,
}

/// `POST /gdocs/read`
pub async fn read(
    State(state): State<AppState>,
    Json(req): Json<ReadRequest>,
) -> Result<Json<Export>, SidecarError> {
    let target = resolve(&req)?;
    let format = req
        .format
        .unwrap_or_else(|| Format::default_for(target.kind));

    let label = target
        .title
        .clone()
        .unwrap_or_else(|| target.doc_id.clone());
    logger::log_request(
        "POST",
        "/gdocs/read",
        "chrome",
        std::slice::from_ref(&label),
        None,
    );
    let started = Instant::now();
    let activity = state.events.start(
        ActivityKind::Gdocs,
        "gdocs read",
        std::slice::from_ref(&label),
        None,
    );

    let result = export::fetch(
        &target.doc_id,
        target.kind,
        format,
        req.wait_secs,
        target.title,
    )
    .await;

    logger::log_completion(
        "/gdocs/read",
        Some(if result.is_ok() { 0 } else { 1 }),
        started.elapsed().as_millis(),
    );
    match &result {
        Ok(_) => state.events.finish(activity, Some(0), None),
        Err(e) => state.events.finish(activity, None, Some(e.to_string())),
    }
    result.map(Json)
}

/// `GET /gdocs/list`
pub async fn list(Query(q): Query<ListQuery>) -> Result<Json<ListResponse>, SidecarError> {
    logger::log_request("GET", "/gdocs/list", "drive", &[], None);
    let listing = drive::list()?;
    let mounts = drive::mounts()
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let docs = match q.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(filter) => {
            let needle = filter.to_lowercase();
            listing
                .docs
                .into_iter()
                .filter(|d| d.title.to_lowercase().contains(&needle))
                .collect()
        }
        None => listing.docs,
    };

    Ok(Json(ListResponse {
        count: docs.len(),
        docs,
        truncated: listing.truncated,
        mounts,
    }))
}

/// What to export: an id, its type, and a title when one is known.
struct Target {
    doc_id: String,
    kind: DocKind,
    title: Option<String>,
}

/// Resolve the request to a single document. Supplying more than one selector is
/// rejected rather than silently preferring one.
fn resolve(req: &ReadRequest) -> Result<Target, SidecarError> {
    let given = [req.doc_id.is_some(), req.url.is_some(), req.title.is_some()]
        .iter()
        .filter(|p| **p)
        .count();
    if given == 0 {
        return Err(SidecarError::InvalidRequest(
            "one of doc_id, url, or title is required".into(),
        ));
    }
    if given > 1 {
        return Err(SidecarError::InvalidRequest(
            "provide exactly one of doc_id, url, or title".into(),
        ));
    }

    if let Some(url) = &req.url {
        let (doc_id, kind) = export::parse_url(url)?;
        return Ok(Target {
            doc_id,
            kind,
            title: None,
        });
    }
    if let Some(doc_id) = &req.doc_id {
        export::validate_doc_id(doc_id)?;
        return Ok(Target {
            doc_id: doc_id.clone(),
            // A bare id carries no type, so Doc is the assumption; `kind`
            // overrides it.
            kind: req.kind.unwrap_or(DocKind::Document),
            title: None,
        });
    }

    let title = req.title.as_deref().unwrap_or_default();
    let doc = drive::resolve_title(title)?;
    Ok(Target {
        doc_id: doc.doc_id,
        kind: doc.kind,
        title: Some(doc.title),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(doc_id: Option<&str>, url: Option<&str>, title: Option<&str>) -> ReadRequest {
        ReadRequest {
            doc_id: doc_id.map(str::to_string),
            url: url.map(str::to_string),
            title: title.map(str::to_string),
            format: None,
            kind: None,
            wait_secs: None,
        }
    }

    #[test]
    fn requires_exactly_one_selector() {
        assert!(resolve(&req(None, None, None)).is_err());
        assert!(resolve(&req(
            Some("1HDS9lW9I81gXKUnTOunAkQt4KVCNx044"),
            Some("https://docs.google.com/document/d/1AAAAAAAAAAAAAAAAAAAA/edit"),
            None
        ))
        .is_err());
    }

    #[test]
    fn doc_id_defaults_to_a_document() {
        let target = resolve(&req(Some("1HDS9lW9I81gXKUnTOunAkQt4KVCNx044"), None, None)).unwrap();
        assert_eq!(target.kind, DocKind::Document);
        assert!(target.title.is_none());
    }

    #[test]
    fn explicit_kind_overrides_the_default() {
        let mut r = req(Some("1HDS9lW9I81gXKUnTOunAkQt4KVCNx044"), None, None);
        r.kind = Some(DocKind::Spreadsheet);
        assert_eq!(resolve(&r).unwrap().kind, DocKind::Spreadsheet);
    }

    /// A URL carries the type, so the caller never has to supply `kind`.
    #[test]
    fn url_determines_the_kind() {
        let target = resolve(&req(
            None,
            Some("https://docs.google.com/spreadsheets/d/1AAAAAAAAAAAAAAAAAAAA/edit#gid=0"),
            None,
        ))
        .unwrap();
        assert_eq!(target.kind, DocKind::Spreadsheet);
        assert_eq!(target.doc_id, "1AAAAAAAAAAAAAAAAAAAA");
    }

    #[test]
    fn malformed_doc_id_is_rejected_before_any_spawn() {
        assert!(resolve(&req(Some("short"), None, None)).is_err());
        assert!(resolve(&req(Some("1AAAA'+alert(1)+'AAAAAAAAAAA"), None, None)).is_err());
    }
}
