use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

/// Commands refused regardless of configuration.
///
/// `sudo` is the one entry because privilege escalation is the only class the
/// sidecar can meaningfully refuse: everything else it might block is reachable
/// anyway through an exec wrapper (`env`, `xargs`, `make`, `sbt`, a shell
/// script), so a longer list would describe a boundary that does not exist.
///
/// This is not a containment boundary. The sidecar runs as the user, on the
/// user's machine — a caller who can reach it can already run code as that user.
/// The denylist exists so the obvious escalation attempt fails loudly rather
/// than silently succeeding.
pub const DEFAULT_DENIED_COMMANDS: &[&str] = &["sudo"];

/// Command-execution policy: deny-by-exception. Every command is permitted
/// except those named in `denied`.
///
/// Replaces the allowlist this file used to carry. That list had grown to ~75
/// entries and still could not hold its own stated line — `env /usr/bin/openssl`
/// reached a binary the list explicitly excluded — because so many allowlisted
/// tools are general-purpose exec wrappers. A denylist is the honest shape: it
/// states the few things deliberately not bridged instead of implying the rest
/// are contained.
#[derive(Debug, Clone)]
pub struct Policy {
    denied: HashSet<String>,
}

/// On-disk form of [`Policy`]. Separate from `Policy` so the file can grow
/// optional keys without the runtime type carrying `Option`s.
#[derive(Debug, Default, Deserialize)]
struct PolicyFile {
    /// Commands to refuse. Replaces the default set entirely when present.
    #[serde(default)]
    denied: Option<Vec<String>>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            denied: DEFAULT_DENIED_COMMANDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

impl Policy {
    /// Is this command permitted?
    ///
    /// Matched on the basename, so `/usr/bin/sudo` and `sudo` are the same
    /// decision — otherwise a denial would be one absolute path away from
    /// meaningless.
    pub fn allows(&self, cmd: &str) -> bool {
        !self.denied.contains(basename(cmd))
    }

    pub fn denied(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.denied.iter().map(String::as_str).collect();
        out.sort_unstable();
        out
    }

    /// Parse a policy from TOML. An absent or empty `denied` key means an empty
    /// denylist — the file is authoritative, so a user who writes `denied = []`
    /// gets exactly that rather than having the defaults reappear.
    fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        let file: PolicyFile = toml::from_str(text)?;
        Ok(match file.denied {
            Some(denied) => Self {
                denied: denied.into_iter().collect(),
            },
            None => Self::default(),
        })
    }
}

/// Strip any directory prefix from a command name.
fn basename(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

/// Path of the policy file: `$SIDECAR_CONFIG`, else
/// `$XDG_CONFIG_HOME/claude-sidecar/config.toml`, else
/// `~/.config/claude-sidecar/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("SIDECAR_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("claude-sidecar").join("config.toml"))
}

/// Load the policy from `path`.
///
/// A missing file yields the defaults — the sidecar must run out of the box. A
/// *malformed* file is a hard error: silently falling back to defaults would
/// mean a user who wrote a denial and fat-fingered the syntax gets a sidecar
/// that permits what they just tried to refuse.
pub fn load_policy_from(path: &Path) -> Result<Policy, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Policy::from_toml(&text)
            .map_err(|e| format!("invalid policy file {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
        Err(e) => Err(format!("cannot read policy file {}: {e}", path.display())),
    }
}

static POLICY: OnceLock<Policy> = OnceLock::new();

/// Install the process-wide policy. Called once at startup, before serving.
pub fn init_policy(policy: Policy) {
    let _ = POLICY.set(policy);
}

/// The active policy. Falls back to the defaults if `init_policy` was never
/// called (unit tests, and any future embedding of the library).
pub fn policy() -> &'static Policy {
    POLICY.get_or_init(Policy::default)
}

/// Check whether a command may be executed under the active policy.
pub fn is_allowed(cmd: &str) -> bool {
    policy().allows(cmd)
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
    /// Seconds a `SIGTERM`'d job gets to exit before `SIGKILL`. `0` skips
    /// straight to `SIGKILL` (the behavior before the ladder existed).
    pub kill_grace_secs: u64,
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
            kill_grace_secs: 5,
        }
    }
}

/// Resolve a command name to an absolute path.
///
/// A path-qualified command (`/usr/bin/env`, `./script.sh`) is used as given;
/// a bare name is looked up in the common install prefixes first, then `PATH`.
pub fn resolve(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        let path = PathBuf::from(cmd);
        return path.exists().then_some(path);
    }

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

    #[test]
    fn default_policy_denies_only_sudo() {
        let p = Policy::default();
        assert!(!p.allows("sudo"));
        assert_eq!(p.denied(), vec!["sudo"]);
    }

    #[test]
    fn default_policy_allows_everything_else() {
        let p = Policy::default();
        // Including the tools the old allowlist had to be edited to admit, and
        // the ones it excluded but could not actually keep out.
        for cmd in [
            "git",
            "sbt",
            "cargo",
            "aws",
            "claude-medic",
            "openssl",
            "ln",
            "env",
            "xargs",
            "some-new-tool-nobody-has-heard-of",
        ] {
            assert!(p.allows(cmd), "`{cmd}` should be permitted by default");
        }
    }

    #[test]
    fn denial_matches_on_basename() {
        // Otherwise `/usr/bin/sudo` walks straight past a `sudo` denial.
        let p = Policy::default();
        assert!(!p.allows("/usr/bin/sudo"));
        assert!(!p.allows("../../usr/bin/sudo"));
    }

    #[test]
    fn config_file_replaces_the_default_denylist() {
        let p = Policy::from_toml("denied = [\"rm\", \"dd\"]").expect("valid toml");
        assert!(!p.allows("rm"));
        assert!(!p.allows("dd"));
        // Replaces rather than extends, so `sudo` is no longer denied.
        assert!(p.allows("sudo"));
    }

    #[test]
    fn explicitly_empty_denylist_denies_nothing() {
        // A user who writes `denied = []` means it; the defaults must not
        // silently reappear.
        let p = Policy::from_toml("denied = []").expect("valid toml");
        assert!(p.allows("sudo"));
        assert!(p.denied().is_empty());
    }

    #[test]
    fn absent_key_yields_defaults() {
        let p = Policy::from_toml("").expect("empty toml is valid");
        assert!(!p.allows("sudo"));
    }

    #[test]
    fn malformed_config_is_an_error_not_a_silent_default() {
        // Falling back to defaults here would permit exactly what the user was
        // trying to deny when they made the typo.
        assert!(Policy::from_toml("denied = \"rm\"").is_err());
        assert!(Policy::from_toml("denied = [").is_err());
    }

    #[test]
    fn missing_file_yields_defaults_but_unreadable_path_errors() {
        let missing = std::env::temp_dir().join("sc-no-such-policy-file.toml");
        let _ = std::fs::remove_file(&missing);
        let p = load_policy_from(&missing).expect("missing file is not an error");
        assert!(!p.allows("sudo"));
    }

    #[test]
    fn policy_loads_from_a_real_file() {
        let path = std::env::temp_dir().join(format!("sc-policy-test-{}.toml", std::process::id()));
        std::fs::write(&path, "denied = [\"shutdown\"]\n").expect("write temp policy");
        let p = load_policy_from(&path).expect("valid policy file");
        assert!(!p.allows("shutdown"));
        assert!(p.allows("sudo"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_accepts_absolute_paths() {
        assert_eq!(resolve("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert_eq!(resolve("/nonexistent/binary/xyzzy"), None);
    }
}
