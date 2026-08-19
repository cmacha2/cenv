//! Setup paths.
//!
//! Two ways in, by increasing commitment:
//!   `cenv enable-hooks`  — merge capture hooks into the existing
//!                          ~/.claude/settings.json. Standalone capture, no
//!                          env repo, nothing else touched.
//!   `cenv init` + `cenv install` — scaffold a private env repo and symlink
//!                          its config into ~/.claude (synced-config mode).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::paths;

/// (settings.json event, `cenv hook` subcommand)
const HOOK_EVENTS: [(&str, &str); 3] = [
    ("SessionStart", "session-start"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

/// Absolute path to the running binary. Hooks execute under a non-interactive
/// `/bin/sh`, which sources no shell profile, so `~/.cargo/bin` (or any other
/// install prefix) is simply not on PATH there — a bare `cenv` command fails
/// with "command not found" and capture silently never happens. Wiring the
/// resolved path of whatever binary the user just ran removes the assumption.
fn binary_path() -> PathBuf {
    std::env::current_exe()
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .unwrap_or_else(|_| PathBuf::from("cenv"))
}

fn hook_command(event_cmd: &str) -> String {
    let bin = binary_path();
    let bin = bin.to_string_lossy();
    // Quote only when needed, so the common case stays readable in settings.json.
    if bin.contains(char::is_whitespace) {
        format!("\"{bin}\" hook {event_cmd}")
    } else {
        format!("{bin} hook {event_cmd}")
    }
}

/// Is this settings.json command one of ours for `event_cmd`?
///
/// Matched by suffix so a reinstall under a different prefix still recognizes —
/// and replaces — the old entry. It deliberately does not match a command that
/// merely *contains* ours (`cenv hook stop && notify-me`): that is the user's
/// own wrapper, and removing it would silently discard their customization.
fn is_our_command(command: &str, event_cmd: &str) -> bool {
    let c = command.trim().trim_end_matches('"');
    let tail = format!("hook {event_cmd}");
    let Some(prefix) = c.strip_suffix(&tail) else {
        return false;
    };
    let prefix = prefix.trim_end().trim_end_matches('"');
    prefix.ends_with("cenv") || prefix.ends_with("cenv.exe")
}

fn hooks_value() -> Value {
    let mut hooks = serde_json::Map::new();
    for (event, event_cmd) in HOOK_EVENTS {
        hooks.insert(
            event.to_string(),
            json!([{ "hooks": [{ "type": "command", "command": hook_command(event_cmd) }] }]),
        );
    }
    Value::Object(hooks)
}

fn timestamp() -> String {
    jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string()
}

fn bak_name(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.bak.{}",
        path.file_name().unwrap().to_string_lossy(),
        timestamp()
    ))
}

/// Copy the file's current contents aside before we rewrite it. Symlinks are
/// followed on purpose: editing through a link still overwrites real content,
/// so that content is what needs a backup.
fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let bak = bak_name(path);
    fs::copy(path, &bak)?;
    Ok(Some(bak))
}

/// Record where a symlink we are about to replace used to point, so `uninstall`
/// can put the user's own link back (dotfile managers like stow own these).
fn backup_symlink(path: &Path) -> Result<Option<PathBuf>> {
    let Ok(target) = fs::read_link(path) else {
        return Ok(None);
    };
    let bak = bak_name(path).with_extension("symlink");
    fs::write(&bak, format!("{}\n", target.display()))?;
    Ok(Some(bak))
}

/// Merge cenv's hook entries into the live settings.json without disturbing
/// anything else in it. Idempotent; `remove` strips them instead.
pub fn enable_hooks(remove: bool) -> Result<()> {
    let settings_path = paths::claude_dir().join("settings.json");
    fs::create_dir_all(settings_path.parent().unwrap())?;

    // Only a genuinely absent file starts from an empty object. Any other read
    // error (permissions, non-UTF-8 bytes) must abort: treating it as "missing"
    // would replace the user's whole config with a hooks-only document.
    let mut root: Value = match fs::read_to_string(&settings_path) {
        Ok(raw) => serde_json::from_str(&raw).with_context(|| {
            format!(
                "{} is not valid JSON — fix it first",
                settings_path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("cannot read {}", settings_path.display()))
            );
        }
    };
    if !root.is_object() {
        bail!("{} is not a JSON object", settings_path.display());
    }

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        bail!("settings.json \"hooks\" is not an object");
    }

    for (event, event_cmd) in HOOK_EVENTS {
        let groups = hooks
            .as_object_mut()
            .unwrap()
            .entry(event)
            .or_insert_with(|| json!([]));
        let Some(arr) = groups.as_array_mut() else {
            bail!("hooks.{event} is not an array")
        };

        // Drop our previous entries (under any install prefix) before adding
        // the current one, so re-running stays idempotent and repairs a stale
        // path instead of stacking a second hook next to it.
        for g in arr.iter_mut() {
            if let Some(inner) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                inner.retain(|h| {
                    !h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|c| is_our_command(c, event_cmd))
                });
            }
        }
        arr.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|inner| !inner.is_empty())
        });

        if !remove {
            arr.push(json!({
                "hooks": [{ "type": "command", "command": hook_command(event_cmd) }]
            }));
        }
    }

    if let Some(bak) = backup(&settings_path)? {
        println!("  backed up: {}", bak.display());
    }
    crate::state::write_atomic(
        &settings_path,
        (serde_json::to_string_pretty(&root)? + "\n").as_bytes(),
    )?;
    println!(
        "  {} cenv hooks in {}",
        if remove { "removed" } else { "enabled" },
        settings_path.display()
    );
    if !remove {
        println!("  Open a new Claude session for the hooks to take effect.");
    }
    Ok(())
}

/// Scaffold a private env repo: synced Claude config + memory + cenv config.
pub fn init(path: Option<PathBuf>) -> Result<()> {
    let repo = path.unwrap_or_else(paths::env_repo);
    if repo.join("claude/settings.json").exists() {
        bail!("{} already looks like an env repo", repo.display());
    }
    fs::create_dir_all(repo.join("claude/commands"))?;
    fs::create_dir_all(repo.join("memory"))?;

    let settings = json!({ "hooks": hooks_value() });
    fs::write(
        repo.join("claude/settings.json"),
        serde_json::to_string_pretty(&settings)? + "\n",
    )?;
    fs::write(
        repo.join("claude/CLAUDE.md"),
        "# Global instructions\n\nYour cross-project instructions for Claude Code live here.\n",
    )?;
    fs::write(
        repo.join("config.toml"),
        "# cenv config (synced). All values shown are the defaults.\n\n\
         [capture]\n# detail = \"conversation\"   # conversation | tools | full\n\n\
         [llm]\n# model = \"haiku\"\n# timeout_secs = 120\n# min_exchanges = 2\n# min_chars = 400\n\n\
         [sync]\n# scan = \"range\"            # range | full\n",
    )?;
    fs::write(
        repo.join("config.local.toml.example"),
        "# Machine-local config — copy to config.local.toml (gitignored).\n\n\
         # Project memory mirrored into this repo on `cenv sync`:\n\
         # [[memory]]\n# name = \"my-project\"\n# path = \"/abs/path/to/.claude/projects/<enc>/memory\"\n",
    )?;
    fs::write(
        repo.join(".gitignore"),
        "config.local.toml\n*.bak.*\n.DS_Store\n",
    )?;
    fs::write(
        repo.join("README.md"),
        "# claude-env (private)\n\nPersonal Claude Code environment managed by \
         [cenv](https://github.com/cmacha2/cenv).\n\n\
         - `cenv install` on a new machine (then `claude login` — credentials never sync)\n\
         - `cenv sync` to pull → mirror memory → gitleaks scan → push\n\n\
         **Keep this repository private.** It accumulates your instructions and project memory.\n",
    )?;

    if which_git() {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .arg("-q")
            .status();
    }
    println!("Scaffolded env repo at {}", repo.display());
    println!("Next: `cenv install`, then create a PRIVATE remote and `cenv sync`.");
    Ok(())
}

fn which_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn link(src: &Path, dst: &Path) -> Result<()> {
    if dst.is_symlink() {
        // Someone else's symlink (a dotfile manager's, typically). Remember
        // where it pointed so uninstall can restore it instead of leaving the
        // user with no config at all.
        let already_ours = fs::canonicalize(dst).is_ok_and(|c| c == src);
        if !already_ours && let Some(bak) = backup_symlink(dst)? {
            println!("  recorded existing symlink: {}", bak.display());
        }
        fs::remove_file(dst)?;
    } else if let Some(bak) = backup(dst)? {
        fs::remove_file(dst)?;
        println!("  backed up: {}", bak.display());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(src, dst)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(src, dst)?;
    println!("  linked:    {} -> {}", dst.display(), src.display());
    Ok(())
}

/// Symlink an env repo's claude/ config into ~/.claude. Guards (learned from a
/// real silent-capture-loss incident in the template this tool grew out of):
/// never install from a temp path, never install a settings.json without cenv
/// hooks, and keep the canonical repo path pointing at what you installed.
pub fn install(from: Option<PathBuf>, force: bool) -> Result<()> {
    let repo = fs::canonicalize(from.unwrap_or_else(paths::env_repo))
        .context("env repo not found — run `cenv init` first")?;

    if paths::suspicious_temp(&repo) && !force {
        bail!(
            "refusing to install from a temporary path: {}\n\
             This is almost certainly a scratch copy. Install from your real clone (--force overrides).",
            repo.display()
        );
    }

    let settings_src = repo.join("claude/settings.json");
    let raw = fs::read_to_string(&settings_src)
        .with_context(|| format!("missing {} — not an env repo?", settings_src.display()))?;
    if !raw.contains("cenv hook") {
        bail!(
            "no cenv hooks in {} — installing it would silently disable capture. \
             Add them (see `cenv init` output) and re-run.",
            settings_src.display()
        );
    }

    let canon = paths::env_repo();
    if !canon.exists() {
        if let Some(parent) = canon.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo, &canon)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&repo, &canon)?;
        println!("  linked:    {} -> {}", canon.display(), repo.display());
    } else if fs::canonicalize(&canon)? != repo && !force {
        bail!(
            "{} resolves to {}, but you are installing from {}.\n\
             Repoint it or install from the canonical copy (--force overrides).",
            canon.display(),
            fs::canonicalize(&canon)?.display(),
            repo.display()
        );
    }

    let claude_dir = paths::claude_dir();
    fs::create_dir_all(claude_dir.join("commands"))?;
    link(&settings_src, &claude_dir.join("settings.json"))?;
    let claude_md = repo.join("claude/CLAUDE.md");
    if claude_md.exists() {
        link(&claude_md, &claude_dir.join("CLAUDE.md"))?;
    }
    if let Ok(entries) = fs::read_dir(repo.join("claude/commands")) {
        for e in entries.flatten() {
            link(&e.path(), &claude_dir.join("commands").join(e.file_name()))?;
        }
    }

    println!("\nSelf-check:");
    let code = crate::doctor::run(false);
    if code != 0 {
        bail!("post-install self-check failed — see above");
    }
    println!("\nIMPORTANT: run `claude login` on this machine — credentials never sync.");
    Ok(())
}

/// Remove the symlinks that point into the env repo, restoring whatever was
/// there before: a backed-up file, or the user's own symlink if we displaced one.
pub fn uninstall() -> Result<()> {
    let repo_path = paths::env_repo();
    let repo = fs::canonicalize(&repo_path).ok();
    if repo.is_none() {
        println!(
            "note: {} does not resolve — cleaning up any dangling links that look like ours.",
            repo_path.display()
        );
    }
    let claude_dir = paths::claude_dir();
    let mut targets = vec![
        claude_dir.join("settings.json"),
        claude_dir.join("CLAUDE.md"),
    ];
    if let Ok(entries) = fs::read_dir(claude_dir.join("commands")) {
        targets.extend(entries.flatten().map(|e| e.path()));
    }
    let mut touched = 0;
    for t in targets {
        if !t.is_symlink() {
            continue;
        }
        let dest = fs::read_link(&t).unwrap_or_default();
        let ours = match &repo {
            Some(r) => dest.starts_with(r) || fs::canonicalize(&t).is_ok_and(|c| c.starts_with(r)),
            // Repo gone: a dangling link into where it used to be is still ours
            // to clean up, and leaving it would keep Claude Code misconfigured.
            None => dest.starts_with(&repo_path) || !t.exists(),
        };
        if !ours {
            continue;
        }
        fs::remove_file(&t)?;
        touched += 1;
        println!("  removed:   {}", t.display());
        restore_previous(&t)?;
    }
    if touched == 0 {
        println!("Nothing to uninstall — no symlinks into the env repo were found.");
    } else {
        println!("Done. (The env repo itself was not touched.)");
    }
    Ok(())
}

/// Put back the newest thing we set aside for `path`: a `.symlink` record
/// (recreate the link) or a plain backup copy. Timestamps are zero-padded, so
/// lexicographic order is chronological.
fn restore_previous(path: &Path) -> Result<()> {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let mut baks: Vec<PathBuf> = fs::read_dir(path.parent().unwrap())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with(&format!("{name}.bak."))
        })
        .collect();
    baks.sort();
    let Some(latest) = baks.pop() else {
        return Ok(());
    };
    if latest.extension().is_some_and(|e| e == "symlink") {
        let target = fs::read_to_string(&latest)?.trim().to_string();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, path)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, path)?;
        fs::remove_file(&latest)?;
        println!(
            "  restored:  {} -> {target} (your original symlink)",
            path.display()
        );
    } else {
        fs::rename(&latest, path)?;
        println!("  restored:  {} <- {}", path.display(), latest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_command_is_an_absolute_path() {
        // Hooks run under a non-interactive shell with no profile sourced, so a
        // bare `cenv` would fail with "command not found" on every stop.
        let cmd = hook_command("stop");
        assert!(cmd.ends_with(" hook stop"), "{cmd}");
        let bin = cmd.trim_end_matches(" hook stop").trim_matches('"');
        assert!(Path::new(bin).is_absolute(), "must be absolute: {cmd}");
    }

    #[test]
    fn recognizes_our_commands_across_prefixes() {
        // Ours, under any install prefix — must be replaced on re-run.
        for c in [
            "cenv hook stop",
            "/Users/x/.cargo/bin/cenv hook stop",
            "\"/Users/a b/.cargo/bin/cenv\" hook stop",
            "  /usr/local/bin/cenv hook stop  ",
        ] {
            assert!(is_our_command(c, "stop"), "should match: {c}");
        }
        // Not ours — a wrapper or a different event must survive untouched.
        for c in [
            "cenv hook stop && notify-me",
            "my-wrapper 'cenv hook stop'",
            "/Users/x/.cargo/bin/cenv hook session-end",
            "cenvious hook stop",
            "python3 hooks/other.py",
        ] {
            assert!(!is_our_command(c, "stop"), "should NOT match: {c}");
        }
    }
}
