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

pub fn problems() -> Vec<String> {
    let mut out = Vec::new();
    let settings = paths::claude_dir().join("settings.json");

    let hooks_live = settings_candidates()
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .any(|c| c.contains("cenv hook"));
    if !hooks_live {
        out.push(format!(
            "no cenv hooks in any active settings file (checked {}) — sessions are NOT being captured; run `cenv enable-hooks`",
            settings.display()
        ));
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
