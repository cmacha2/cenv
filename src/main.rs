mod backfill;
mod config;
mod distill;
mod doctor;
mod export;
mod hook;
mod install;
mod llm;
mod paths;
mod render;
mod state;
mod sync;
mod transcript;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Portable Claude Code environment: automatic conversation→Markdown capture,
/// LLM session summaries, CLAUDE.md rule distillation, and fail-closed config
/// + memory sync across machines.
#[derive(Parser)]
#[command(name = "cenv", version, about, max_term_width = 100)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Merge cenv's capture hooks into ~/.claude/settings.json (standalone mode)
    EnableHooks {
        /// Strip cenv's hooks instead of adding them
        #[arg(long)]
        remove: bool,
    },
    /// Scaffold a private env repo (synced config + memory)
    Init {
        /// Where to create it (default: ~/.claude-env)
        path: Option<PathBuf>,
    },
    /// Symlink an env repo's claude/ config into ~/.claude
    Install {
        /// Repo to install from (default: ~/.claude-env)
        path: Option<PathBuf>,
        /// Override the safety guards
        #[arg(long)]
        force: bool,
    },
    /// Remove the symlinks created by install (restores backups)
    Uninstall,
    /// Verify the capture wiring is live
    Doctor {
        /// Print nothing when healthy; one line per problem otherwise
        #[arg(long, short)]
        quiet: bool,
    },
    /// Hook entry points (wired into settings.json; read hook JSON on stdin)
    #[command(subcommand)]
    Hook(HookCmd),
    /// Export transcripts to Markdown (current project's sessions by default)
    Export {
        /// Specific transcript .jsonl file(s); omit to export the current project's sessions
        paths: Vec<PathBuf>,
        /// Rebuild an INDEX.md from export frontmatter (recovery path)
        #[arg(long)]
        reindex: bool,
        /// History dir to reindex (default: this project's store)
        #[arg(long, requires = "reindex")]
        store: Option<PathBuf>,
    },
    /// Keep this project's history inside the repo (<project>/history/) instead of the central store
    Adopt {
        /// Revert to the central store
        #[arg(long)]
        central: bool,
    },
    /// Review-and-apply workflow for distilled CLAUDE.md rules
    #[command(subcommand)]
    Distill(DistillCmd),
    /// Sync the env repo: pull → mirror memory → gitleaks scan → push
    Sync {
        /// Scan the full git history instead of only unpushed commits
        #[arg(long)]
        full_scan: bool,
    },
    /// Export ALL existing transcripts into a browsable archive (dry-run by default)
    Backfill {
        /// Actually write the archive
        #[arg(long)]
        export: bool,
        /// Also run the LLM pass (summary + rule candidates, one call per session)
        #[arg(long)]
        analyze: bool,
        /// Archive directory (default: ~/.local/share/cenv/archive)
        #[arg(long)]
        archive: Option<PathBuf>,
        /// Transcript source dir (repeatable; default: ~/.claude/projects)
        #[arg(long = "projects-dir")]
        projects_dirs: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    SessionStart,
    Stop,
    SessionEnd,
}

#[derive(Subcommand)]
enum DistillCmd {
    /// Apply checked [x] staging candidates into the matching CLAUDE.md
    Apply {
        /// Project history dir holding the staging file (default: current project's store)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Target the GLOBAL ~/.claude/CLAUDE.md
        #[arg(long)]
        global: bool,
        /// Required to actually write the global CLAUDE.md (dry-run otherwise)
        #[arg(long)]
        confirm: bool,
    },
    /// Cluster candidates across all projects; recurring ones become global candidates
    ScanGlobal,
}

fn current_cwd_store() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    config::history_dir_for(cwd.to_str(), &config::load_local())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::EnableHooks { remove } => install::enable_hooks(remove)?,
        Cmd::Init { path } => install::init(path)?,
        Cmd::Install { path, force } => install::install(path, force)?,
        Cmd::Uninstall => install::uninstall()?,
        Cmd::Doctor { quiet } => std::process::exit(doctor::run(quiet)),
        Cmd::Hook(h) => match h {
            HookCmd::SessionStart => hook::session_start(),
            HookCmd::Stop => hook::stop(),
            HookCmd::SessionEnd => hook::session_end(),
        },
        Cmd::Export {
            paths,
            reindex,
            store,
        } => {
            if reindex {
                let store = store.unwrap_or_else(current_cwd_store);
                if !store.is_dir() {
                    anyhow::bail!(
                        "no history directory at {} — nothing to reindex. \
                         Pass --store <dir> to point at one.",
                        store.display()
                    );
                }
                let n = export::reindex(&store)?;
                println!("Reindexed {n} export(s) in {}", store.display());
                return Ok(());
            }
            let targets = if paths.is_empty() {
                let cwd = std::env::current_dir()?;
                let found = export::transcripts_for_cwd(&cwd);
                if found.is_empty() {
                    println!(
                        "No transcripts found for {} under {}.\n\
                         Pass a .jsonl path explicitly to export a specific session.",
                        cwd.display(),
                        paths::projects_dir().display()
                    );
                    return Ok(());
                }
                found
            } else {
                paths
            };
            for t in targets {
                match export::export_full(&t, None)? {
                    Some(o) => println!("{}", o.out_path.display()),
                    None => println!("skip (empty): {}", t.display()),
                }
            }
        }
        Cmd::Adopt { central } => {
            let cwd = std::env::current_dir()?;
            let key = cwd.to_string_lossy().into_owned();
            let mut local = config::load_local();
            local
                .projects
                .entry(key.clone())
                .or_default()
                .history_in_repo = !central;
            let path = config::save_local(&local)?;
            if central {
                println!(
                    "History for {key} now goes to the central store ({}).",
                    path.display()
                );
            } else {
                if cwd.join(".git").exists() {
                    ensure_gitignore_line(&cwd, "history/")?;
                }
                println!(
                    "History for {key} will be written to ./history/ (recorded in {}).\n\
                     Tip: point agents at it from CLAUDE.local.md — cenv never edits shared project files.",
                    path.display()
                );
            }
        }
        Cmd::Distill(d) => match d {
            DistillCmd::Apply {
                path,
                global,
                confirm,
            } => {
                if global {
                    distill::apply(
                        &distill::global_staging(),
                        &distill::global_claude_md(),
                        true,
                        confirm,
                    )?;
                } else {
                    let store = path.unwrap_or_else(current_cwd_store);
                    let staging = store.join(distill::STAGING_NAME);
                    // Project rules apply to the project the sessions came from.
                    let target = state::load_index(&store)
                        .rows
                        .first()
                        .map(|_| ())
                        .and_then(|_| detect_project_claude_md(&store))
                        .unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_default()
                                .join("CLAUDE.md")
                        });
                    distill::apply(&staging, &target, false, confirm)?;
                }
            }
            DistillCmd::ScanGlobal => {
                distill::scan_global(&distill::all_store_dirs(&config::load_local()))?
            }
        },
        Cmd::Sync { full_scan } => sync::run(full_scan)?,
        Cmd::Backfill {
            export,
            analyze,
            archive,
            projects_dirs,
        } => backfill::run(backfill::Options {
            export,
            analyze,
            archive,
            projects_dirs,
        })?,
    }
    Ok(())
}

/// A store dir maps back to its project via any export's frontmatter cwd — but
/// exports only carry the project name, so fall back to the adopted-project
/// layout (`<project>/history`) or the cwd.
fn detect_project_claude_md(store: &std::path::Path) -> Option<PathBuf> {
    if store.file_name().and_then(|n| n.to_str()) == Some("history") {
        return store.parent().map(|p| p.join("CLAUDE.md"));
    }
    None
}

fn ensure_gitignore_line(project: &std::path::Path, line: &str) -> Result<()> {
    let gi = project.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim().trim_end_matches('/') == line.trim_end_matches('/'))
    {
        return Ok(());
    }
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    std::fs::write(
        &gi,
        format!("{existing}{sep}# Claude Code session exports (cenv)\n{line}\n"),
    )?;
    println!("  added {line} to {}", gi.display());
    Ok(())
}
