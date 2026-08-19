//! Retroactive export of ALL existing transcripts into a browsable archive,
//! organized by project, never touching the original project directories.
//! Dry-run by default; the LLM pass is opt-in, resumable, and — unlike the
//! original two-script pipeline — one `claude -p` call per session covers both
//! the summary and the rule candidates.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{config, distill, export, llm, paths, transcript};

pub struct Options {
    pub export: bool,
    pub analyze: bool,
    pub archive: Option<PathBuf>,
    pub projects_dirs: Vec<PathBuf>,
}

fn find_transcripts(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let Ok(projects) = fs::read_dir(d) else {
            continue;
        };
        for p in projects.flatten() {
            if !p.path().is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(p.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

pub fn run(opts: Options) -> Result<()> {
    let dirs = if opts.projects_dirs.is_empty() {
        vec![paths::projects_dir()]
    } else {
        opts.projects_dirs.clone()
    };
    let archive = opts
        .archive
        .clone()
        .unwrap_or_else(|| paths::data_dir().join("archive"));

    let transcripts = find_transcripts(&dirs);
    let mb: f64 = transcripts
        .iter()
        .filter_map(|t| fs::metadata(t).ok())
        .map(|m| m.len() as f64)
        .sum::<f64>()
        / 1e6;

    println!("Transcript sources:");
    for d in &dirs {
        println!("  {}", d.display());
    }
    println!("\nFound {} transcript(s), {mb:.1} MB.", transcripts.len());
    if transcripts.is_empty() {
        return Ok(());
    }

    if !opts.export && !opts.analyze {
        println!("\nDRY RUN — nothing written.");
        println!(
            "To export them all (free, no model calls) into {}:",
            archive.display()
        );
        println!("  cenv backfill --export");
        println!(
            "To also add an AI summary + rule candidates per session ({} `claude -p` calls, one per session):",
            transcripts.len()
        );
        println!("  cenv backfill --export --analyze");
        return Ok(());
    }

    fs::create_dir_all(&archive)?;
    println!("\nExporting → {}", archive.display());
    let mut ok = 0;
    for (i, t) in transcripts.iter().enumerate() {
        match export::export_full(t, Some(&archive)) {
            Ok(Some(_)) => ok += 1,
            Ok(None) => {}
            Err(e) => println!(
                "  skip {}: {e}",
                t.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
        if (i + 1) % 20 == 0 || i + 1 == transcripts.len() {
            println!("  {}/{}", i + 1, transcripts.len());
        }
    }
    println!("Exported {ok} transcript(s).");

    if opts.analyze {
        analyze_all(&transcripts, &archive)?;
    }
    println!("\nDone → {}", archive.display());
    println!(
        "Browse {}/<project>/INDEX.md for per-project indexes.",
        archive.display()
    );
    Ok(())
}

fn analyze_all(transcripts: &[PathBuf], archive: &Path) -> Result<()> {
    let cfg = config::load();
    println!(
        "\nAnalyzing {} session(s) with `claude -p` (model={}); resumable — already-done ones are skipped…",
        transcripts.len(),
        cfg.llm.model
    );
    let (mut done, mut skipped, mut failed) = (0, 0, 0);
    for (i, t) in transcripts.iter().enumerate() {
        let outcome: Result<&str> = (|| {
            let events = transcript::read_all_events(t)?;
            let mut meta = transcript::Meta::default();
            meta.absorb(&events);
            let sid = meta
                .session
                .clone()
                .or_else(|| t.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default();
            let project = paths::project_name(meta.cwd.as_deref());
            let out_dir = archive.join(paths::slugify(&project, 40));

            if !export::read_sidecar(&out_dir, Some(&sid)).is_empty() {
                return Ok("skipped");
            }
            if meta.exchanges < cfg.llm.min_exchanges {
                return Ok("skipped");
            }
            let convo = transcript::plain_conversation(&events, 12_000);
            if convo.len() < cfg.llm.min_chars {
                return Ok("skipped");
            }
            let Some(analysis) = llm::analyze(&convo, &cfg.llm)? else {
                return Ok("failed");
            };
            if !analysis.summary.trim().is_empty() {
                export::write_sidecar(&out_dir, &sid, &analysis.summary)?;
                export::export_full(t, Some(archive))?; // re-render with prose in header + index
            }
            if !analysis.rules.is_empty() {
                distill::append_candidates(
                    &out_dir.join(distill::STAGING_NAME),
                    &distill::project_staging_header(&project, &out_dir),
                    &analysis.rules,
                    &project,
                    Some(&sid),
                    meta.cwd
                        .as_deref()
                        .map(|c| Path::new(c).join("CLAUDE.md"))
                        .as_deref(),
                )?;
            }
            Ok("done")
        })();
        match outcome {
            Ok("done") => done += 1,
            Ok("skipped") => skipped += 1,
            _ => failed += 1,
        }
        println!(
            "  {}/{} (new {done}, cached/skipped {skipped}, failed {failed})",
            i + 1,
            transcripts.len()
        );
    }
    println!("Analysis: {done} new, {skipped} skipped, {failed} failed.");
    Ok(())
}
