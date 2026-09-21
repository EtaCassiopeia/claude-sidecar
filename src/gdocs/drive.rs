//! Google Drive File Provider mount — a local *index* of Drive documents.
//!
//! Google-native files do not sync their content: every `.gdoc`/`.gsheet`/
//! `.gslides` on disk is a ~200-byte JSON stub holding a `doc_id` and nothing
//! else. So the mount cannot answer "what does this document say" — but it is a
//! good index, resolving a human-readable title to the id that [`super::export`]
//! needs without a network round-trip.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::SidecarError;

/// Where macOS mounts File Provider volumes.
const CLOUD_STORAGE: &str = "Library/CloudStorage";
/// Mount directory prefix for Drive accounts, e.g. `GoogleDrive-user@corp.com`.
const DRIVE_PREFIX: &str = "GoogleDrive-";

/// Depth and entry caps so a pathological tree cannot stall a request. Hitting
/// either sets [`Listing::truncated`], which callers surface rather than
/// reporting a partial index as complete.
const MAX_DEPTH: usize = 16;
const MAX_ENTRIES: usize = 50_000;

/// Which Google editor owns a document. Determines the export URL and which
/// formats are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Document,
    Spreadsheet,
    Presentation,
}

impl DocKind {
    /// The stub extension Drive writes for this kind.
    fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "gdoc" => Some(DocKind::Document),
            "gsheet" => Some(DocKind::Spreadsheet),
            "gslides" => Some(DocKind::Presentation),
            _ => None,
        }
    }

    /// First path segment of the editor URL — also the export path prefix.
    /// Note the plural: Sheets is `/spreadsheets/`, the other two are singular.
    pub fn url_segment(self) -> &'static str {
        match self {
            DocKind::Document => "document",
            DocKind::Spreadsheet => "spreadsheets",
            DocKind::Presentation => "presentation",
        }
    }

    pub fn from_url_segment(segment: &str) -> Option<Self> {
        match segment {
            "document" => Some(DocKind::Document),
            "spreadsheets" => Some(DocKind::Spreadsheet),
            "presentation" => Some(DocKind::Presentation),
            _ => None,
        }
    }
}

/// A Google-native document found on the mount.
#[derive(Debug, Clone, Serialize)]
pub struct DriveDoc {
    /// File stem — the document title as it appears in Drive.
    pub title: String,
    pub doc_id: String,
    pub kind: DocKind,
    pub path: String,
}

/// The stub file's contents. Only `doc_id` matters; the rest is Drive's own
/// bookkeeping (including a `""` key holding a DO-NOT-EDIT warning).
#[derive(Deserialize)]
struct Stub {
    doc_id: String,
}

/// Result of walking the mount. `truncated` means a cap was hit, so a
/// "not found" from this listing is inconclusive.
#[derive(Debug, Default)]
pub struct Listing {
    pub docs: Vec<DriveDoc>,
    pub truncated: bool,
}

/// Locate Drive mounts under `~/Library/CloudStorage`. Multiple accounts each
/// get their own directory, so this returns all of them.
pub fn mounts() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let cloud = Path::new(&home).join(CLOUD_STORAGE);
    let Ok(entries) = std::fs::read_dir(&cloud) else {
        return Vec::new();
    };

    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(DRIVE_PREFIX))
                && p.is_dir()
        })
        .collect();
    // Stable order so repeated calls agree on which mount wins a tie.
    found.sort();
    found
}

/// Index every Google-native document across all mounts.
pub fn list() -> Result<Listing, SidecarError> {
    let mounts = mounts();
    if mounts.is_empty() {
        return Err(SidecarError::InvalidRequest(
            "no Google Drive mount found under ~/Library/CloudStorage — \
             install Google Drive for desktop, or pass doc_id/url instead of title"
                .into(),
        ));
    }

    let mut listing = Listing::default();
    for mount in mounts {
        walk(&mount, 0, &mut listing);
    }
    listing.docs.sort_by(|a, b| a.title.cmp(&b.title));
    Ok(listing)
}

/// Recursive descent collecting stub files.
///
/// Dot-directories are skipped deliberately: `.shortcut-targets-by-id` reports
/// 65535 links and walking it is both pointless and slow.
fn walk(dir: &Path, depth: usize, out: &mut Listing) {
    if depth > MAX_DEPTH || out.docs.len() >= MAX_ENTRIES {
        out.truncated = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        // An unreadable subtree (permissions, a mount still warming up) is not
        // fatal to the rest of the index.
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&path, depth + 1, out);
        } else if let Some(doc) = read_stub(&path) {
            if out.docs.len() >= MAX_ENTRIES {
                out.truncated = true;
                return;
            }
            out.docs.push(doc);
        }
    }
}

/// Parse a stub file into a [`DriveDoc`], or `None` if it isn't one.
fn read_stub(path: &Path) -> Option<DriveDoc> {
    let kind = DocKind::from_extension(path.extension()?.to_str()?)?;
    let title = path.file_stem()?.to_str()?.to_string();
    let raw = std::fs::read_to_string(path).ok()?;
    let stub: Stub = serde_json::from_str(&raw).ok()?;
    // A stub whose id is unusable would fail later with a confusing browser
    // error, so it is dropped from the index here instead.
    super::export::validate_doc_id(&stub.doc_id).ok()?;
    Some(DriveDoc {
        title,
        doc_id: stub.doc_id,
        kind,
        path: path.to_string_lossy().to_string(),
    })
}

/// Resolve a title to exactly one document.
///
/// Exact (case-insensitive) matches win outright; only if there are none does
/// this fall back to substring matching. Ambiguity is an error listing the
/// candidates — picking one silently would read the wrong document.
pub fn resolve_title(query: &str) -> Result<DriveDoc, SidecarError> {
    let listing = list()?;
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Err(SidecarError::InvalidRequest(
            "title must not be empty".into(),
        ));
    }

    let exact: Vec<&DriveDoc> = listing
        .docs
        .iter()
        .filter(|d| d.title.to_lowercase() == needle)
        .collect();
    let matches = if exact.is_empty() {
        listing
            .docs
            .iter()
            .filter(|d| d.title.to_lowercase().contains(&needle))
            .collect()
    } else {
        exact
    };

    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(SidecarError::InvalidRequest(format!(
            "no document on the Drive mount matches {query:?}{}",
            if listing.truncated {
                " (the index was truncated, so it may exist deeper in the tree)"
            } else {
                ""
            }
        ))),
        many => Err(SidecarError::InvalidRequest(format!(
            "{:?} matches {} documents: {}. Use a more specific title, or pass doc_id.",
            query,
            many.len(),
            many.iter()
                .take(10)
                .map(|d| format!("{:?}", d.title))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_maps_to_and_from_url_segments() {
        // The plural on spreadsheets is a real Google inconsistency; a wrong
        // segment yields a 404 from the export endpoint.
        assert_eq!(DocKind::Spreadsheet.url_segment(), "spreadsheets");
        assert_eq!(DocKind::Document.url_segment(), "document");
        assert_eq!(DocKind::Presentation.url_segment(), "presentation");
        for kind in [
            DocKind::Document,
            DocKind::Spreadsheet,
            DocKind::Presentation,
        ] {
            assert_eq!(DocKind::from_url_segment(kind.url_segment()), Some(kind));
        }
        assert_eq!(DocKind::from_url_segment("drawings"), None);
    }

    #[test]
    fn stub_extensions_recognized() {
        assert_eq!(DocKind::from_extension("gdoc"), Some(DocKind::Document));
        assert_eq!(
            DocKind::from_extension("gsheet"),
            Some(DocKind::Spreadsheet)
        );
        assert_eq!(
            DocKind::from_extension("gslides"),
            Some(DocKind::Presentation)
        );
        // Real binaries that live on the mount are not stubs.
        assert_eq!(DocKind::from_extension("xlsx"), None);
        assert_eq!(DocKind::from_extension("md"), None);
    }

    /// The stub shape is Google's, including a `""` key whose value is a
    /// DO-NOT-EDIT warning — serde must tolerate the extra fields.
    #[test]
    fn reads_a_real_shaped_stub() {
        let dir = std::env::temp_dir().join(format!("drive-stub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Design Notes.gdoc");
        std::fs::write(
            &path,
            r#"{"":"WARNING! DO NOT EDIT THIS FILE!","doc_id":"1HDS9lW9I81gXKUnTOunAkQt4KVCNx044","resource_key":"","email":"a@b.com"}"#,
        )
        .unwrap();

        let doc = read_stub(&path).expect("stub parses");
        assert_eq!(doc.title, "Design Notes");
        assert_eq!(doc.doc_id, "1HDS9lW9I81gXKUnTOunAkQt4KVCNx044");
        assert_eq!(doc.kind, DocKind::Document);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stub carrying an id we would refuse to put in a URL is dropped at index
    /// time rather than failing later as an opaque browser error.
    #[test]
    fn stub_with_unusable_id_is_skipped() {
        let dir = std::env::temp_dir().join(format!("drive-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Bad.gdoc");
        std::fs::write(&path, r#"{"doc_id":"nope'; alert(1)"}"#).unwrap();
        assert!(read_stub(&path).is_none());

        let short = dir.join("Short.gdoc");
        std::fs::write(&short, r#"{"doc_id":"abc"}"#).unwrap();
        assert!(read_stub(&short).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_stub_files_are_not_docs() {
        let dir = std::env::temp_dir().join(format!("drive-other-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.xlsx");
        std::fs::write(&path, "PK\x03\x04").unwrap();
        assert!(read_stub(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dot-directories are the reason the walk is usable at all: the real mount
    /// has a `.shortcut-targets-by-id` with 65535 entries.
    #[test]
    fn walk_skips_dot_directories() {
        let root = std::env::temp_dir().join(format!("drive-walk-{}", std::process::id()));
        let hidden = root.join(".shortcut-targets-by-id");
        let visible = root.join("My Drive");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::create_dir_all(&visible).unwrap();
        std::fs::write(
            hidden.join("Hidden.gdoc"),
            r#"{"doc_id":"1AAAAAAAAAAAAAAAAAAAAAAA"}"#,
        )
        .unwrap();
        std::fs::write(
            visible.join("Visible.gdoc"),
            r#"{"doc_id":"1BBBBBBBBBBBBBBBBBBBBBBB"}"#,
        )
        .unwrap();

        let mut listing = Listing::default();
        walk(&root, 0, &mut listing);
        let titles: Vec<&str> = listing.docs.iter().map(|d| d.title.as_str()).collect();
        assert_eq!(titles, vec!["Visible"]);
        assert!(!listing.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn depth_cap_reports_truncation_rather_than_lying() {
        let root = std::env::temp_dir().join(format!("drive-deep-{}", std::process::id()));
        let mut deep = root.clone();
        for i in 0..(MAX_DEPTH + 3) {
            deep = deep.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            deep.join("Buried.gdoc"),
            r#"{"doc_id":"1CCCCCCCCCCCCCCCCCCCCCCC"}"#,
        )
        .unwrap();

        let mut listing = Listing::default();
        walk(&root, 0, &mut listing);
        assert!(listing.docs.is_empty());
        assert!(
            listing.truncated,
            "hitting the depth cap must be reported, not silently treated as empty"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
