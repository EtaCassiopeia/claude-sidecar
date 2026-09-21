//! Load rendered HTML onto the macOS clipboard as the `«class HTML»` flavor.
//!
//! `pbcopy` sets `public.utf8-plain-text`, which makes Docs paste visible tags —
//! the flavor is the whole point. AppleScript's `read … as «class HTML»` is the
//! only route to it without linking AppKit.
//!
//! Following the browser bridge's rule, the file path travels as an osascript
//! **argv item** and is never spliced into script text, so this endpoint can't be
//! turned into arbitrary AppleScript execution.

use std::{path::Path, time::Duration};

use tokio::{process::Command, time::timeout};

use crate::error::SidecarError;

/// Always present on macOS. As in the browser bridge, callers get these two
/// fixed scripts, not general osascript access.
const OSASCRIPT: &str = "/usr/bin/osascript";

const TIMEOUT_SECS: u64 = 15;

/// `POSIX file` takes the path as a value, so the caller's path stays data.
const SET_CLIPBOARD_SCRIPT: &str = r#"on run argv
    set the clipboard to (read (POSIX file (item 1 of argv)) as «class HTML»)
    return "ok"
end run"#;

/// Reports the flavors currently on the clipboard, e.g. `«class HTML», 48213`.
const CLIPBOARD_INFO_SCRIPT: &str = "return clipboard info";

/// Put `path`'s contents on the clipboard as HTML, then read the flavor back.
///
/// Returns the `clipboard info` description so the caller can assert the flavor
/// actually took, rather than trusting that a zero exit means success.
pub async fn set_html(path: &Path) -> Result<String, SidecarError> {
    if !cfg!(target_os = "macos") {
        return Err(SidecarError::Clipboard(
            "setting the clipboard requires macOS (it uses osascript)".into(),
        ));
    }

    let path = path.to_string_lossy().to_string();
    run(SET_CLIPBOARD_SCRIPT, &[&path]).await?;

    let info = run(CLIPBOARD_INFO_SCRIPT, &[]).await?;
    verify_flavor(&info)?;
    Ok(info)
}

/// The clipboard must carry HTML and nothing else — a lingering plain-text
/// flavor is what makes Docs paste visible tags.
fn verify_flavor(info: &str) -> Result<(), SidecarError> {
    if !info.contains("class HTML") {
        return Err(SidecarError::Clipboard(format!(
            "clipboard did not take the HTML flavor (clipboard info: {info})"
        )));
    }
    Ok(())
}

async fn run(script: &str, args: &[&str]) -> Result<String, SidecarError> {
    let mut cmd = Command::new(OSASCRIPT);
    cmd.arg("-e").arg(script).args(args).kill_on_drop(true);

    let output = timeout(Duration::from_secs(TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| SidecarError::Timeout { secs: TIMEOUT_SECS })??;

    if !output.status.success() {
        return Err(clipboard_error(&String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Map an osascript failure to an actionable error. The Automation-permission
/// prompt is the one every new user hits.
fn clipboard_error(stderr: &str) -> SidecarError {
    let msg = stderr.trim();
    let lower = msg.to_lowercase();
    let hint = if msg.contains("-1743") || lower.contains("not authorized") {
        Some(
            "grant this terminal Automation access in \
             System Settings > Privacy & Security > Automation",
        )
    } else if msg.contains("-10006") || lower.contains("can’t set clipboard") {
        // Seen when the process has no pasteboard access — e.g. running inside a
        // sandbox that blocks the HIServices XPC service.
        Some("the calling process has no pasteboard access; run the sidecar outside a sandbox")
    } else {
        None
    };
    match hint {
        Some(hint) => SidecarError::Clipboard(format!("{msg} ({hint})")),
        None => SidecarError::Clipboard(msg.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_html_flavor() {
        assert!(verify_flavor("«class HTML», 48213").is_ok());
    }

    #[test]
    fn rejects_plain_text_only_clipboard() {
        let err = verify_flavor("«class utf8», 120").unwrap_err();
        assert!(matches!(err, SidecarError::Clipboard(_)));
        // The failing value belongs in the message — otherwise diagnosing this
        // means re-running by hand.
        assert!(err.to_string().contains("class utf8"));
    }

    /// The path is a script argument, so nothing a caller supplies can reach the
    /// script body.
    #[test]
    fn script_does_not_interpolate_the_path() {
        assert!(SET_CLIPBOARD_SCRIPT.contains("item 1 of argv"));
        assert!(!SET_CLIPBOARD_SCRIPT.contains("{}"));
    }

    #[test]
    fn automation_denial_gets_a_remediation_hint() {
        let err = clipboard_error("execution error: Not authorized to send Apple events (-1743)");
        assert!(err.to_string().contains("Automation"), "got: {err}");
    }

    #[test]
    fn pasteboard_denial_gets_a_sandbox_hint() {
        let err = clipboard_error("0:34: execution error: Can’t set clipboard to \"x\". (-10006)");
        assert!(err.to_string().contains("sandbox"), "got: {err}");
    }
}
