//! Every well-known location, resolved once. Each is overridable via env var
//! so tests (and unusual setups) can redirect the whole tool.

use std::env;
use std::path::PathBuf;

fn from_env(var: &str, default: PathBuf) -> PathBuf {
    env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

pub fn home() -> PathBuf {
    from_env(
        "CENV_HOME",
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
    )
}

/// Claude Code's own config dir (settings.json, CLAUDE.md, projects/).
pub fn claude_dir() -> PathBuf {
    from_env("CENV_CLAUDE_DIR", home().join(".claude"))
}

/// Where Claude Code writes transcripts.
pub fn projects_dir() -> PathBuf {
    from_env("CENV_PROJECTS_DIR", claude_dir().join("projects"))
}

/// The user's private env repo (synced config + memory). Optional.
pub fn env_repo() -> PathBuf {
    from_env("CENV_REPO", home().join(".claude-env"))
}

/// Central history store: markdown exports live here unless a project opts
/// into in-repo history via `cenv adopt`.
pub fn data_dir() -> PathBuf {
    from_env("CENV_DATA_DIR", home().join(".local/share/cenv"))
}

pub fn history_store() -> PathBuf {
    data_dir().join("history")
}

/// Per-session incremental state (offsets, cached meta).
pub fn state_dir() -> PathBuf {
    from_env("CENV_STATE_DIR", home().join(".local/state/cenv"))
}

pub fn config_fallback_dir() -> PathBuf {
    from_env("CENV_CONFIG_DIR", home().join(".config/cenv"))
}

/// Set in the environment of `claude -p` children we spawn, so their hooks
/// short-circuit instead of recursing.
pub const LOCK_ENV: &str = "CENV_LOCK";

pub fn locked() -> bool {
    env::var_os(LOCK_ENV).is_some()
}

pub fn is_temp_path(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    [
        "/tmp/",
        "/private/tmp/",
        "/var/folders/",
        "/private/var/folders/",
    ]
    .iter()
    .any(|pre| s.starts_with(pre))
}

/// A temp path OUTSIDE the home dir — the "installed from a scratch copy that
/// will vanish" failure mode. Anything under home is by definition not that.
/// Both sides are canonicalized (macOS aliases /var/folders ↔ /private/var/folders).
pub fn suspicious_temp(p: &std::path::Path) -> bool {
    let canon = |x: &std::path::Path| std::fs::canonicalize(x).unwrap_or_else(|_| x.to_path_buf());
    let p = canon(p);
    is_temp_path(&p) && !p.starts_with(canon(&home()))
}

/// Subdirectory holding a store's session exports, bucketed by month.
/// The store root keeps only generated files: INDEX.md, the rule-candidate
/// staging file, `.project`, `.index/`, `.summaries/`.
pub const SESSIONS_DIR: &str = "sessions";

/// `YYYY-MM` bucket for an export date, or `undated` for anything that isn't
/// one — a transcript with no usable timestamp must still land somewhere.
pub fn month_bucket(date: &str) -> String {
    let ym: String = date.chars().take(7).collect();
    let shaped = ym.len() == 7
        && ym.bytes().enumerate().all(|(i, b)| {
            if i == 4 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if shaped { ym } else { "undated".into() }
}

/// Slug for project directory names inside the history store.
pub fn slugify(text: &str, maxlen: usize) -> String {
    let mut s = String::new();
    let mut last_dash = true;
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
            last_dash = false;
        } else if !last_dash {
            s.push('-');
            last_dash = true;
        }
        if s.len() >= maxlen {
            break;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "session".into() } else { s }
}

/// Short stable hash (FNV-1a) for disambiguating same-named projects.
pub fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) as u32)
}

/// Project name = basename of the session's cwd.
pub fn project_name(cwd: Option<&str>) -> String {
    cwd.and_then(|c| {
        std::path::Path::new(c.trim_end_matches('/'))
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    })
    .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_basics() {
        assert_eq!(slugify("Hello, World! 2024", 45), "hello-world-2024");
        assert_eq!(slugify("", 45), "session");
        assert_eq!(slugify("---", 45), "session");
    }

    #[test]
    fn month_buckets() {
        assert_eq!(month_bucket("2026-08-31"), "2026-08");
        assert_eq!(month_bucket("2026-08"), "2026-08");
        assert_eq!(month_bucket(""), "undated");
        assert_eq!(month_bucket("undated"), "undated");
        assert_eq!(month_bucket("202608-31"), "undated");
    }

    #[test]
    fn temp_paths() {
        assert!(is_temp_path(std::path::Path::new("/tmp/x")));
        assert!(is_temp_path(std::path::Path::new("/private/var/folders/a")));
        assert!(!is_temp_path(std::path::Path::new("/Users/x/.claude-env")));
    }
}
