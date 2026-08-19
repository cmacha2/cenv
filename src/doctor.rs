//! Health checks. Exists because silent capture loss is the worst failure
//! mode of this kind of tooling: everything looks fine until you need the
//! history that was never written.

use std::fs;
use std::path::Path;

use crate::paths;

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|d| {
            let p = d.join(bin);
            p.is_file() || p.with_extension("exe").is_file()
        })
    })
}

/// Problems that should surface inside a session (used by the SessionStart hook).
/// Claude Code merges settings from several files; hooks configured in any of
/// them are live, so all are checked before reporting capture as broken.
fn settings_candidates() -> Vec<std::path::PathBuf> {
    let mut v = vec![
        paths::claude_dir().join("settings.json"),
        paths::claude_dir().join("settings.local.json"),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        v.push(cwd.join(".claude/settings.json"));
        v.push(cwd.join(".claude/settings.local.json"));
    }
    v
}

/// Every `cenv hook …` command string found across the active settings files.
fn configured_hook_commands() -> Vec<String> {
    let mut out = Vec::new();
    for path in settings_candidates() {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(events) = json.get("hooks").and_then(|h| h.as_object()) else {
            continue;
        };
        for groups in events.values() {
            for g in groups.as_array().into_iter().flatten() {
                for h in g
                    .get("hooks")
                    .and_then(|x| x.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(cmd) = h.get("command").and_then(|c| c.as_str())
                        && cmd.contains("cenv hook")
                    {
                        out.push(cmd.to_string());
                    }
                }
            }
        }
    }
    out
}

/// The executable a hook command will actually try to run.
fn hook_binary(command: &str) -> String {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        return rest.split('"').next().unwrap_or(rest).to_string();
    }
    c.split_whitespace().next().unwrap_or(c).to_string()
}

fn resolvable(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    // A bare name depends on PATH — and hooks run under a non-interactive shell
    // that sources no profile, so PATH there is the bare system default.
    [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/opt/homebrew/bin",
        "/usr/sbin",
        "/sbin",
    ]
    .iter()
    .any(|d| Path::new(d).join(bin).is_file())
}

pub fn problems() -> Vec<String> {
    let mut out = Vec::new();
    let settings = paths::claude_dir().join("settings.json");

    let commands = configured_hook_commands();
    if commands.is_empty() {
        out.push(format!(
            "no cenv hooks in any active settings file (checked {}) — sessions are NOT being captured; run `cenv enable-hooks`",
            settings.display()
        ));
    }
    // A hook whose binary the hook shell cannot find fails with "command not
    // found" on every stop — visible only as a hook error, never as missing
    // history, so it is worth naming explicitly.
    for cmd in &commands {
        let bin = hook_binary(cmd);
        if !resolvable(&bin) {
            out.push(format!(
                "hook command points at `{bin}`, which won't resolve in the hook shell \
                 (it starts with no shell profile, so ~/.cargo/bin is not on PATH) — \
                 re-run `cenv enable-hooks` to wire the absolute path"
            ));
            break;
        }
    }

    if let Ok(target) = fs::read_link(&settings) {
        let resolved = settings.parent().unwrap_or(Path::new("/")).join(&target);
        if !resolved.exists() && !target.exists() {
            out.push(format!(
                "settings.json is a DANGLING symlink -> {}",
                target.display()
            ));
        } else if paths::suspicious_temp(&target) {
            out.push(format!(
                "settings.json points into a temp path ({}) — it will break when tmp is cleaned",
                target.display()
            ));
        }
    }

    let state = paths::state_dir();
    if fs::create_dir_all(state.join("sessions")).is_err() {
        out.push(format!(
            "state dir {} is not writable — incremental capture disabled",
            state.display()
        ));
    }
    out
}

/// Softer advisories for the full report only.
fn advisories() -> Vec<String> {
    let mut out = Vec::new();
    if !on_path("claude") {
        out.push(
            "`claude` CLI not on PATH — session summaries and rule distillation will be skipped"
                .into(),
        );
    }
    if !on_path("gitleaks") {
        out.push("`gitleaks` not on PATH — `cenv sync` will refuse to push (fail-closed)".into());
    }
    let repo = paths::env_repo();
    if repo.exists() {
        if paths::suspicious_temp(&fs::canonicalize(&repo).unwrap_or_else(|_| repo.clone())) {
            out.push(format!(
                "{} resolves into a temp path — it will break when tmp is cleaned",
                repo.display()
            ));
        }
    } else {
        out.push(format!(
            "no env repo at {} — config/memory sync disabled (capture still works); `cenv init` creates one",
            repo.display()
        ));
    }
    out
}

pub fn run(quiet: bool) -> i32 {
    let problems = problems();
    let advisories = advisories();

    if quiet {
        for p in &problems {
            println!("⚠️  cenv doctor: {p}");
        }
        return if problems.is_empty() { 0 } else { 1 };
    }

    let settings = paths::claude_dir().join("settings.json");
    if problems.is_empty() {
        println!("  ok:   capture hooks live in {}", settings.display());
        println!("  ok:   state dir {}", paths::state_dir().display());
    }
    for p in &problems {
        println!("  FAIL: {p}");
    }
    for a in &advisories {
        println!("  note: {a}");
    }
    if problems.is_empty() {
        println!("  cenv wiring healthy ✓");
        0
    } else {
        1
    }
}
