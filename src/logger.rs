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

fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Print the startup banner to stderr.
pub fn print_banner(port: u16) {
    let allowed = crate::config::ALLOWED_COMMANDS.join(", ");
    if color_enabled() {
        eprintln!(
            "{CYAN}{BOLD}┌─────────────────────────────────────────┐{RESET}\n\
             {CYAN}{BOLD}│  claude-sidecar v3  │  :{WHITE}{port:<5}{RESET}{CYAN}{BOLD}         │{RESET}\n\
             {CYAN}{BOLD}└─────────────────────────────────────────┘{RESET}"
        );
        eprintln!();
        eprintln!(
            "  {DIM}Endpoints:{RESET}  POST /exec  POST /jobs  GET /jobs/{{id}}/lines  POST /browser/fetch  GET /browser/tab  GET /health{RESET}"
        );
        eprintln!("  {DIM}Allowed:{RESET}   {allowed}{RESET}");
    } else {
        eprintln!(
            "claude-sidecar v3 | port:{port}\n\
             Endpoints: POST /exec  POST /jobs  GET /jobs/{{id}}/lines  POST /browser/fetch  GET /browser/tab  GET /health\n\
             Allowed: {allowed}"
        );
    }
    eprintln!();
}

/// Log an incoming request.
pub fn log_request(method: &str, path: &str, cmd: &str, args: &[String], cwd: Option<&str>) {
    let t = now_hms();
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
    let t = now_hms();
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
    let t = now_hms();
    let colored = color_line(line);
    if color_enabled() {
        eprintln!("{DIM}{t}{RESET} {DIM}│{RESET} {colored}");
    } else {
        eprintln!("{t} | {line}");
    }
}
