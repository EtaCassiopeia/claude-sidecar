use std::path::PathBuf;

/// Commands allowed to be executed by the sidecar. Edit this list to match the
/// build, test, and VCS tools you want reachable from the sandbox.
pub const ALLOWED_COMMANDS: &[&str] = &[
    // VCS & forges
    "gh",
    "git",
    // Build & test
    "go",
    "sbt",
    "cargo",
    "mvn",
    "gradle",
    "npm",
    "node",
    "python3",
    "pytest",
    // Network
    "curl",
    // Containers
    "docker",
    "docker-compose",
    // File inspection & editing
    "grep",
    "rg",
    "find",
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "diff",
    "sed",
    "awk",
    "sort",
    "uniq",
    "cut",
    "tr",
    "xargs",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "touch",
    "chmod",
    // Text / data
    "jq",
    "yq",
    // Shell
    "bash",
    "sh",
    // Shell utilities
    "which",
    "env",
    "printenv",
    "echo",
    "printf",
    "date",
    "uname",
    "sysctl",
    // Keychain
    "security",
];

/// Flags that allow arbitrary code execution when passed to interpreter
/// commands. Blocking these prevents `python3 -c "os.system(...)"` and
/// equivalent escapes through node, perl, ruby, etc.
///
/// The check is applied only to commands that are interpreters — tools like
/// `cargo` or `git` take `-c` with harmless semantics and are not in this set.
const INTERPRETER_EXEC_FLAGS: &[&str] = &[
    "-c",
    "--command", // python3, node, perl, ruby, sh, bash …
    "-e",
    "--eval", // node, perl, ruby
    "-",      // read script from stdin
    "--",     // some interpreters treat this as stdin marker
];

/// Commands that are scripting interpreters and must have their arguments
/// checked for inline-execution flags.
const INTERPRETER_COMMANDS: &[&str] = &["python3", "node", "perl", "ruby", "sh", "bash", "zsh"];

/// Check whether a command name is on the allowlist.
pub fn is_allowed(cmd: &str) -> bool {
    ALLOWED_COMMANDS.contains(&cmd)
}

/// Check whether the arguments are safe for the given command.
///
/// Returns `Ok(())` if safe, or `Err(reason)` describing why the invocation
/// was rejected. Called after `is_allowed` passes.
///
/// # What this blocks
///
/// Scripting interpreters (`python3`, `node`, …) accept flags like `-c` and
/// `-e` that execute an arbitrary string as code. Those strings can call
/// `os.system`, `subprocess`, `child_process.exec`, etc. — completely bypassing
/// the top-level allowlist. This function rejects any such invocation.
///
/// All other allowlisted commands (build tools, git, curl, …) are passed
/// through without argument inspection because they do not have inline
/// code-execution semantics.
pub fn check_args(cmd: &str, args: &[String]) -> Result<(), String> {
    if !INTERPRETER_COMMANDS.contains(&cmd) {
        return Ok(());
    }
    for arg in args {
        // Exact match against known exec flags.
        if INTERPRETER_EXEC_FLAGS.contains(&arg.as_str()) {
            return Err(format!(
                "inline execution flag `{arg}` is not allowed for `{cmd}`; \
                 pass a script file path instead"
            ));
        }
        // Combined short flags like `-ci` or `-ec` that embed an exec flag.
        if arg.starts_with('-') && !arg.starts_with("--") {
            let inner = arg.trim_start_matches('-');
            if inner.contains('c') || inner.contains('e') {
                return Err(format!(
                    "flag `{arg}` contains an inline execution flag and is not \
                     allowed for `{cmd}`"
                ));
            }
        }
    }
    Ok(())
}

/// Common install prefixes to probe before falling back to a `PATH` lookup
/// (covers Homebrew on macOS).
const INSTALL_PREFIXES: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

/// Runtime configuration passed to the server.
#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub verbose: bool,
    pub max_jobs: usize,
    /// Maximum output lines retained *in memory* per job.
    pub max_lines_per_job: usize,
    /// When true, lines beyond `max_lines_per_job` spill to a per-job temp file
    /// instead of being dropped, so the full log stays retrievable.
    pub spill_to_disk: bool,
    pub job_ttl_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8765,
            verbose: false,
            max_jobs: 100,
            max_lines_per_job: 50_000,
            spill_to_disk: false,
            job_ttl_secs: 600,
        }
    }
}

/// Resolve a command name to an absolute path.
///
/// Tries common install prefixes first, then falls back to a `PATH` search.
pub fn resolve(cmd: &str) -> Option<PathBuf> {
    for prefix in INSTALL_PREFIXES {
        let path = PathBuf::from(prefix).join(cmd);
        if path.exists() {
            return Some(path);
        }
    }

    // Fall back to searching PATH.
    std::env::var("PATH").ok().and_then(|path_var| {
        path_var.split(':').find_map(|dir| {
            let candidate = PathBuf::from(dir).join(cmd);
            candidate.exists().then_some(candidate)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn interpreter_inline_c_flag_blocked() {
        assert!(check_args("python3", &args(&["-c", "import os; os.system('id')"])).is_err());
        assert!(check_args(
            "node",
            &args(&["-e", "require('child_process').exec('id')"])
        )
        .is_err());
    }

    #[test]
    fn interpreter_stdin_flag_blocked() {
        assert!(check_args("python3", &args(&["-"])).is_err());
    }

    #[test]
    fn interpreter_combined_flag_blocked() {
        assert!(check_args("python3", &args(&["-ic"])).is_err());
    }

    #[test]
    fn interpreter_script_file_allowed() {
        assert!(check_args("python3", &args(&["script.py", "--verbose"])).is_ok());
        assert!(check_args("node", &args(&["index.js"])).is_ok());
    }

    #[test]
    fn non_interpreter_c_flag_allowed() {
        // `git -c` sets a config value — not an exec flag, must not be blocked.
        assert!(check_args("git", &args(&["-c", "user.email=x@y.com", "commit"])).is_ok());
        assert!(check_args("cargo", &args(&["test", "--", "-c"])).is_ok());
    }

    #[test]
    fn pytest_args_allowed() {
        assert!(check_args("pytest", &args(&["-v", "--tb=short", "tests/"])).is_ok());
    }
}
