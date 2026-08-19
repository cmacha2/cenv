//! Env-repo sync: pull → mirror whitelisted memory → commit → gitleaks scan →
//! push. Manual by design (nothing leaves the machine unless you run it) and
//! fail-closed (no gitleaks, no push).
//!
//! Two deliberate improvements over the shell original:
//!   - memory is MIRRORED, not just copied — files you deleted locally are
//!     deleted from the repo too, instead of resurrecting forever;
//!   - the scan covers only commits not yet on the upstream by default, so one
//!     historical leak (from before the gate) doesn't brick every future sync.
//!     `--full-scan` (or `sync.scan = "full"`) audits all history.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::{config, paths};

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("running git")
}

fn git_ok(repo: &Path, args: &[&str]) -> Result<()> {
    let out = git(repo, args)?;
    if !out.status.success() {
        bail!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn has_remote(repo: &Path) -> bool {
    git(repo, &["remote"])
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

fn upstream(repo: &Path) -> Option<String> {
    let out = git(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn on_path(bin: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
        .or_else(|| {
            let fallback = paths::home().join(".local/bin").join(bin);
            fallback.is_file().then_some(fallback)
        })
}

/// Collect regular files under `dir`, relative to `base`.
///
/// Symlinks are never followed and never listed — on the source side that keeps
/// a linked directory's contents out of the synced repo; on the destination side
/// it is what stops the delete pass from resolving a link and removing the
/// user's real files somewhere else entirely. Read errors propagate: a directory
/// that only *seems* empty must never drive deletions.
fn walk_files(base: &Path, dir: &Path, rel: &mut Vec<PathBuf>) -> Result<()> {
    for e in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e.with_context(|| format!("reading an entry of {}", dir.display()))?;
        let p = e.path();
        let meta = fs::symlink_metadata(&p)?;
        if meta.file_type().is_symlink() {
            println!("  memory: SKIP symlink {}", p.display());
        } else if meta.is_dir() {
            walk_files(base, &p, rel)?;
        } else if meta.is_file() {
            rel.push(p.strip_prefix(base).unwrap().to_path_buf());
        }
    }
    Ok(())
}

/// Mirror `src` into `dst`: copy every regular file, then delete destination
/// files that no longer exist in the source. Returns (copied, deleted).
///
/// Deleting is the whole point (memory you removed locally must not resurrect in
/// the repo), which makes it the one operation here that can destroy data — so
/// it is bounded three ways: the source listing must have succeeded, an emptied
/// source never triggers a mass delete, and only paths reached without
/// traversing a symlink are eligible.
fn mirror(src: &Path, dst: &Path) -> Result<(usize, usize)> {
    let mut src_files = Vec::new();
    walk_files(src, src, &mut src_files)?;

    let mut copied = 0;
    for r in &src_files {
        let to = dst.join(r);
        fs::create_dir_all(to.parent().unwrap())?;
        fs::copy(src.join(r), &to)?;
        copied += 1;
    }

    let mut deleted = 0;
    if dst.exists() {
        let mut dst_files = Vec::new();
        walk_files(dst, dst, &mut dst_files)?;
        if src_files.is_empty() && !dst_files.is_empty() {
            println!(
                "  memory: source {} is empty but the repo has {} file(s) — refusing to delete them.\n\
                 If the removal is intentional, delete them in the repo yourself.",
                src.display(),
                dst_files.len()
            );
            return Ok((copied, 0));
        }
        for r in dst_files {
            if !src_files.contains(&r) {
                fs::remove_file(dst.join(&r))?;
                println!("  memory: deleted {} (gone from source)", r.display());
                deleted += 1;
            }
        }
    }
    Ok((copied, deleted))
}

pub fn run(full_scan: bool) -> Result<()> {
    let repo = paths::env_repo();
    let repo = fs::canonicalize(&repo)
        .with_context(|| format!("no env repo at {} — run `cenv init` first", repo.display()))?;
    if !repo.join(".git").exists() {
        bail!("{} is not a git repository", repo.display());
    }

    // Fail-closed gate checked up front: no scanner, no sync at all.
    let Some(gitleaks) = on_path("gitleaks") else {
        bail!(
            "gitleaks not found — refusing to sync without a secret scan. Install gitleaks first."
        );
    };

    if has_remote(&repo) {
        println!("==> Pulling…");
        git_ok(&repo, &["pull", "--rebase", "--autostash"])
            .context("pull failed — resolve conflicts in the repo, then re-run")?;
    }

    let local = config::load_local();
    if !local.memory.is_empty() {
        println!("==> Mirroring whitelisted memory…");
        for m in &local.memory {
            if m.path.is_dir() {
                let dst = repo.join("memory").join(&m.name);
                let (c, d) = mirror(&m.path, &dst)?;
                println!(
                    "  memory: {} <- {} ({c} file(s), {d} deleted)",
                    m.name,
                    m.path.display()
                );
            } else {
                println!("  memory: SKIP {} (no dir at {})", m.name, m.path.display());
            }
        }
    }

    git_ok(&repo, &["add", "-A"])?;
    let staged = git(&repo, &["diff", "--cached", "--quiet"])?;
    if staged.status.success() {
        println!("==> Nothing to sync.");
        return Ok(());
    }

    let host = Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".into());
    let msg = format!(
        "sync: {} on {host}",
        jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ")
    );
    git_ok(&repo, &["commit", "-q", "-m", &msg])?;

    println!("==> Scanning for secrets…");
    let cfg = config::load();
    let mut scan = Command::new(&gitleaks);
    scan.arg("git").arg("--redact").arg("--no-banner");
    let range_scan = !full_scan && cfg.sync.scan != "full";
    if range_scan && let Some(up) = upstream(&repo) {
        scan.arg(format!("--log-opts={up}..HEAD"));
    }
    scan.arg(&repo);
    let status = scan.status().context("running gitleaks")?;
    if !status.success() {
        // Undo the commit we just made so the working tree is exactly as the
        // user left it. A first-ever commit has no parent to reset to, so the
        // ref is deleted instead. Either way: nothing is pushed.
        let has_parent = git(&repo, &["rev-parse", "--verify", "-q", "HEAD~1"])
            .map(|o| o.status.success())
            .unwrap_or(false);
        let undone = if has_parent {
            git_ok(&repo, &["reset", "--soft", "HEAD~1"])
        } else {
            git_ok(&repo, &["update-ref", "-d", "HEAD"])
        };
        if let Err(e) = undone {
            bail!(
                "gitleaks flagged potential secrets and the commit could NOT be rolled back ({e}).\n\
                 Nothing was pushed. Review the findings above and clean up the commit manually."
            );
        }
        bail!(
            "gitleaks flagged potential secrets — commit rolled back; NOT pushing. Review above."
        );
    }
    println!("  clean ✓");

    if has_remote(&repo) {
        git_ok(&repo, &["push"])?;
        println!("==> Pushed.");
    } else {
        println!("==> No remote set. Add a PRIVATE one, then push:");
        println!(
            "    git -C {} remote add origin <your-private-repo-url>",
            repo.display()
        );
        println!("    git -C {} push -u origin main", repo.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_copies_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.md"), "a").unwrap();
        fs::write(src.join("sub/b.md"), "b").unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("stale.md"), "old").unwrap();

        let (copied, deleted) = mirror(&src, &dst).unwrap();
        assert_eq!((copied, deleted), (2, 1));
        assert!(dst.join("sub/b.md").exists());
        assert!(!dst.join("stale.md").exists());
    }

    #[test]
    fn mirror_never_deletes_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        let precious = tmp.path().join("precious");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::create_dir_all(&precious).unwrap();
        fs::write(src.join("a.md"), "a").unwrap();
        fs::write(precious.join("irreplaceable.md"), "do not delete").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&precious, dst.join("linked")).unwrap();

        mirror(&src, &dst).unwrap();
        assert!(
            precious.join("irreplaceable.md").exists(),
            "files behind a symlink in the destination must survive"
        );
    }

    #[test]
    fn mirror_refuses_mass_delete_when_source_went_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("kept.md"), "backup").unwrap();

        let (copied, deleted) = mirror(&src, &dst).unwrap();
        assert_eq!((copied, deleted), (0, 0));
        assert!(dst.join("kept.md").exists());
    }

    #[test]
    fn mirror_skips_source_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.md"), "not for the repo").unwrap();
        fs::write(src.join("ok.md"), "fine").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, src.join("link")).unwrap();

        let (copied, _) = mirror(&src, &dst).unwrap();
        assert_eq!(copied, 1, "only the real file is mirrored");
        assert!(!dst.join("link").exists());
    }
}
