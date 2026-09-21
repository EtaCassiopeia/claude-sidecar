use std::io::IsTerminal;

/// ANSI escape codes — only emitted when `NO_COLOR` is unset and stderr is a TTY.
fn color_enabled() -> bool {
    std::env::var("NO_COLOR").is_err() && std::io::stderr().is_terminal()
}

// Colour constants (ANSI SGR codes).
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";
const WHITE: &str = "\x1b[97m";

/// Wall-clock `HH:MM:SS` in the machine's local timezone.
///
/// Local, not UTC: these timestamps exist so a failure can be correlated with
/// what the user was doing when it happened, which a UTC-offset clock defeats.
pub fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes into our owned, zeroed `tm` and reads `t` by
    // pointer; both outlive the call. Returns null on failure, checked below.
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
    }

    // Fall back to UTC rather than printing a bogus 00:00:00.
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}Z")
}

/// This process's PID, stamped on every line.
///
/// Several sidecars can append to one log (an autostart race, a manual launch),
/// and without this the loser's failures read as the healthy instance's.
fn pid() -> u32 {
    std::process::id()
}

/// `HH:MM:SS pid:NNNN` — the identity prefix shared by every log line.
pub fn prefix() -> String {
    format!("{} pid:{}", now_hms(), pid())
}

/// Print the startup banner to stderr.
pub fn print_banner(port: u16) {
    let denied = crate::config::policy().denied();
    let policy = if denied.is_empty() {
        "all commands permitted (no denials configured)".to_string()
    } else {
        format!("all commands except: {}", denied.join(", "))
    };
    let started = prefix();
    if color_enabled() {
        eprintln!(
            "{CYAN}{BOLD}┌─────────────────────────────────────────┐{RESET}\n\
             {CYAN}{BOLD}│  claude-sidecar v3  │  :{WHITE}{port:<5}{RESET}{CYAN}{BOLD}         │{RESET}\n\
             {CYAN}{BOLD}└─────────────────────────────────────────┘{RESET}"
        );
        eprintln!();
        eprintln!("  {DIM}Started:{RESET}   {started}");
        eprintln!(
            "  {DIM}Endpoints:{RESET}  POST /exec  POST /batch  POST /jobs  GET /jobs/{{id}}/lines  GET /events  GET /health{RESET}"
        );
        eprintln!("  {DIM}Policy:{RESET}    {policy}{RESET}");
    } else {
        eprintln!(
            "claude-sidecar v3 | port:{port} | {started}\n\
             Endpoints: POST /exec  POST /batch  POST /jobs  GET /jobs/{{id}}/lines  GET /events  GET /health\n\
             Policy: {policy}"
        );
    }
    eprintln!();
}

/// Report a fatal startup failure in terms the reader can act on.
///
/// The bare `anyhow` dump for the common case — another sidecar already owns the
/// port — reads as a crash of the *running* server when both append to one log,
/// so name the situation and the PID that is exiting.
pub fn print_startup_failure(port: u16, err: &anyhow::Error) {
    let p = prefix();
    let already_in_use = err
        .chain()
        .any(|e| e.to_string().contains("Address already in use"));

    if already_in_use {
        eprintln!(
            "{p} ✗ startup aborted: port {port} is already owned by another \
             claude-sidecar. This process is exiting; the existing server is \
             unaffected. Find the owner with: lsof -nP -iTCP:{port} -sTCP:LISTEN"
        );
    } else {
        eprintln!("{p} ✗ startup aborted on port {port}: {err:#}");
    }
}

/// Log an incoming request.
pub fn log_request(method: &str, path: &str, cmd: &str, args: &[String], cwd: Option<&str>) {
    let t = prefix();
    let args_s = args.join(" ");
    let cwd_s = cwd
        .map(|c| {
            // Shorten home dir.
            if let Ok(home) = std::env::var("HOME") {
                c.replacen(&home, "~", 1)
            } else {
                c.to_string()
            }
        })
        .unwrap_or_default();

    if color_enabled() {
        eprintln!(
            "{DIM}{t}{RESET} {CYAN}→{RESET} {BOLD}{method} {path}{RESET}  {GREEN}{cmd}{RESET} {args_s}  {DIM}{cwd_s}{RESET}"
        );
    } else {
        eprintln!("{t} → {method} {path}  {cmd} {args_s}  {cwd_s}");
    }
}

/// Log a completed request.
pub fn log_completion(path: &str, exit_code: Option<i32>, elapsed_ms: u128) {
    let t = prefix();
    let elapsed_s = elapsed_ms as f64 / 1000.0;
    let code = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());

    if color_enabled() {
        let arrow = if exit_code == Some(0) {
            format!("{GREEN}{BOLD}←{RESET}")
        } else {
            format!("{RED}{BOLD}←{RESET}")
        };
        eprintln!("{DIM}{t}{RESET} {arrow} {path}  exit:{code}  {elapsed_s:.1}s");
    } else {
        eprintln!("{t} ← {path}  exit:{code}  {elapsed_s:.1}s");
    }
}

/// Colour-annotate a single output line based on its content.
///
/// Returns the line with ANSI escapes prepended/appended, or the raw line
/// when colours are disabled.
pub fn color_line(line: &str) -> String {
    if !color_enabled() {
        return line.to_string();
    }

    // sbt patterns
    if line.contains("[error]") {
        return format!("{RED}{line}{RESET}");
    }
    if line.contains("[warn]") {
        return format!("{YELLOW}{line}{RESET}");
    }
    if line.contains("[success]") {
        return format!("{GREEN}{BOLD}{line}{RESET}");
    }
    if line.contains("[info] Compiling") || line.contains("[info] compiling") {
        return format!("{BLUE}{line}{RESET}");
    }
    if line.contains("[info] Resolving") || line.contains("[info] Fetching") {
        return format!("{DIM}{line}{RESET}");
    }

    // cargo patterns
    if line.starts_with("error") || line.starts_with("error[") {
        return format!("{RED}{BOLD}{line}{RESET}");
    }
    if line.trim_start().starts_with("Compiling") {
        return format!("{BLUE}{line}{RESET}");
    }
    if line.trim_start().starts_with("Finished") {
        return format!("{GREEN}{BOLD}{line}{RESET}");
    }

    // pytest patterns
    if line.contains("FAILED") {
        return format!("{RED}{line}{RESET}");
    }
    if line.contains(" passed") {
        return format!("{GREEN}{BOLD}{line}{RESET}");
    }

    // go test patterns
    if line.starts_with("FAIL") || line.contains("--- FAIL") {
        return format!("{RED}{BOLD}{line}{RESET}");
    }
    if line.starts_with("ok ") || line.contains("--- PASS") {
        return format!("{GREEN}{line}{RESET}");
    }

    line.to_string()
}

/// Log a single output line (only in verbose mode).
pub fn log_line(line: &str) {
    let t = prefix();
    let colored = color_line(line);
    if color_enabled() {
        eprintln!("{DIM}{t}{RESET} {DIM}│{RESET} {colored}");
    } else {
        eprintln!("{t} | {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_hms_tracks_local_time_not_utc() {
        // `date +%H:%M:%S` is local by definition; ours must agree with it.
        let out = std::process::Command::new("date")
            .arg("+%H:%M")
            .output()
            .expect("date");
        let expected = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let got = now_hms();
        assert!(
            got.starts_with(&expected),
            "expected local time {expected}, got {got}"
        );
    }

    #[test]
    fn prefix_carries_this_pid() {
        assert!(prefix().contains(&format!("pid:{}", std::process::id())));
    }
}
