//! Hook entry points. Contract: never break the session — parse errors and
//! missing data exit 0 silently. There is deliberately NO "newest transcript"
//! fallback here: with two sessions open in parallel, guessing exports the
//! wrong project's history. No transcript_path → no export.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::{config, distill, export, llm, paths, state, transcript};

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct HookInput {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
}

fn read_input() -> HookInput {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    serde_json::from_str(&raw).unwrap_or_default()
}

fn usable_transcript(input: &HookInput) -> Option<PathBuf> {
    if input.session_id.is_empty() || input.transcript_path.is_empty() {
        return None;
    }
    let p = PathBuf::from(&input.transcript_path);
    p.exists().then_some(p)
}

/// Hooks must never break a session, so their errors are swallowed — which
/// makes them equally invisible when something is genuinely wrong. `CENV_DEBUG=1`
/// narrates what a hook decided and why, on stderr, where Claude Code shows it.
fn debug(msg: impl std::fmt::Display) {
    if std::env::var_os("CENV_DEBUG").is_some() {
        eprintln!("cenv: {msg}");
    }
}

fn report(what: &str, r: anyhow::Result<impl Sized>) {
    if let Err(e) = r {
        debug(format!("{what} failed: {e:#}"));
    }
}

pub fn stop() {
    if paths::locked() {
        return;
    }
    let input = read_input();
    let Some(transcript) = usable_transcript(&input) else {
        debug("stop: no usable transcript_path in hook input, doing nothing");
        return;
    };
    report(
        "export",
        export::export_incremental(&transcript, &input.session_id),
    );
}

pub fn session_end() {
    if paths::locked() {
        return;
    }
    let input = read_input();
    let Some(transcript) = usable_transcript(&input) else {
        debug("session-end: no usable transcript_path in hook input, doing nothing");
        return;
    };
    report(
        "export",
        export::export_incremental(&transcript, &input.session_id),
    );
    report("analysis", analyze_session(&transcript, &input.session_id));
}

/// Does this session still deserve an analysis pass?
///
/// Used both by the SessionEnd hook and by `cenv analyze`, which exists because
/// SessionEnd is best-effort: the host may cancel it while the model call is in
/// flight (headless runs do this reliably), and a summary that only ever arrives
/// when a hook wins a race is not a feature.
pub fn analysis_pending(st: &state::SessionState, cfg: &config::Config) -> bool {
    if st.meta.exchanges < cfg.llm.min_exchanges {
        return false;
    }
    let Ok(mtime) = fs::metadata(&st.transcript)
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
    else {
        return false; // transcript gone — nothing to analyze
    };
    !st.llm_done_mtime.is_some_and(|done| done >= mtime)
}

/// Sessions still awaiting an analysis pass, newest first.
pub fn pending_analyses(cfg: &config::Config) -> Vec<(String, state::SessionState)> {
    state::all_sessions()
        .into_iter()
        .filter(|(_, st)| analysis_pending(st, cfg))
        .collect()
}

/// Run the analysis pass for sessions the SessionEnd hook never finished.
pub fn analyze_pending(limit: usize, only_cwd: Option<&str>) -> anyhow::Result<()> {
    let cfg = config::load();
    let pending: Vec<_> = pending_analyses(&cfg)
        .into_iter()
        .filter(|(_, st)| match only_cwd {
            Some(cwd) => st.meta.cwd.as_deref() == Some(cwd),
            None => true,
        })
        .take(limit)
        .collect();

    if pending.is_empty() {
        println!("Nothing pending — every captured session has been analyzed.");
        return Ok(());
    }
    println!(
        "Analyzing {} session(s) with `claude -p` (model={})…",
        pending.len(),
        cfg.llm.model
    );
    let mut done = 0;
    for (sid, st) in &pending {
        let title = crate::render::title_of(&st.meta);
        match analyze_session(&st.transcript, sid) {
            Ok(()) => {
                let fresh = state::load_session(sid);
                if fresh.and_then(|s| s.llm_done_mtime).is_some() {
                    done += 1;
                    println!("  ✓ {title}");
                } else {
                    println!("  – {title} (skipped: too trivial, or the model returned nothing)");
                }
            }
            Err(e) => println!("  ✗ {title}: {e:#}"),
        }
    }
    println!("Analyzed {done} of {}.", pending.len());
    Ok(())
}

/// The single LLM pass: summary + rule candidates in one `claude -p` call.
/// Skips trivial sessions and anything already analyzed at this mtime.
fn analyze_session(transcript: &Path, session_id: &str) -> anyhow::Result<()> {
    let cfg = config::load();
    let local = config::load_local();

    let Some(mut st) = state::load_session(session_id) else {
        debug(format!("analysis: no state for session {session_id}"));
        return Ok(());
    };
    if st.meta.exchanges < cfg.llm.min_exchanges {
        debug(format!(
            "analysis: skipped, {} exchange(s) < min_exchanges {}",
            st.meta.exchanges, cfg.llm.min_exchanges
        ));
        return Ok(());
    }
    let mtime = fs::metadata(transcript)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if st.llm_done_mtime.is_some_and(|done| done >= mtime) {
        debug("analysis: already done for this transcript revision");
        return Ok(());
    }

    let events = transcript::read_all_events(transcript)?;
    let convo = transcript::plain_conversation(&events, 12_000);
    if convo.len() < cfg.llm.min_chars {
        debug(format!(
            "analysis: skipped, {} chars < min_chars {}",
            convo.len(),
            cfg.llm.min_chars
        ));
        return Ok(());
    }
    debug(format!(
        "analysis: calling `claude -p` (model={}) with {} chars",
        cfg.llm.model,
        convo.len()
    ));
    let Some(analysis) = llm::analyze(&convo, &cfg.llm)? else {
        debug("analysis: the model call produced nothing usable");
        return Ok(());
    };
    debug(format!(
        "analysis: got {} char summary and {} rule(s)",
        analysis.summary.len(),
        analysis.rules.len()
    ));

    let out_dir = config::history_dir_for(st.meta.cwd.as_deref(), &local);
    if !analysis.summary.trim().is_empty() {
        export::write_sidecar(&out_dir, session_id, &analysis.summary)?;
        export::refresh_header(session_id)?;
    }
    if !analysis.rules.is_empty() {
        let project = paths::project_name(st.meta.cwd.as_deref());
        let claude_md = st
            .meta
            .cwd
            .as_deref()
            .map(|c| Path::new(c).join("CLAUDE.md"));
        distill::append_candidates(
            &out_dir.join(distill::STAGING_NAME),
            &distill::project_staging_header(&project, &out_dir),
            &analysis.rules,
            &project,
            Some(session_id),
            claude_md.as_deref(),
        )?;
    }

    st.llm_done_mtime = Some(mtime);
    state::save_session(session_id, &st)?;
    Ok(())
}

/// SessionStart: inject discoverability instead of mutating project files.
/// Points the new session at this project's history index and nudges about
/// pending distilled candidates; surfaces doctor problems as a system message.
pub fn session_start() {
    if paths::locked() {
        return;
    }
    let input = read_input();
    let cwd = if input.cwd.is_empty() {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    } else {
        Some(input.cwd.clone())
    };

    let local = config::load_local();
    let store = config::history_dir_for(cwd.as_deref(), &local);

    let mut context = Vec::new();
    let mut system = Vec::new();

    let index = store.join("INDEX.md");
    if index.exists() {
        let sessions = state::load_index(&store).rows.len();
        context.push(format!(
            "Prior Claude Code sessions for this project ({sessions} of them) are summarized in \
             {} — scan it first if past context would help; open a full transcript only for detail.",
            index.display()
        ));
    }

    // Mention an analysis backlog, never work through it here: SessionStart must
    // not block a session on model calls.
    let unanalyzed = pending_analyses(&config::load()).len();
    if unanalyzed > 0 {
        system.push(format!(
            "cenv: {unanalyzed} session(s) awaiting a summary — run `cenv analyze` when convenient"
        ));
    }

    let pending = distill::pending_count(&store);
    if pending > 0 {
        let staging = store.join(distill::STAGING_NAME);
        system.push(format!(
            "📋 {pending} CLAUDE.md candidate(s) pending review — see {}",
            staging.display()
        ));
        context.push(format!(
            "{pending} pending CLAUDE.md rule candidate(s) await review in {}. If the user is \
             interested, offer to walk through them; checked [x] items are applied with \
             `cenv distill apply --path {}`.",
            staging.display(),
            store.display()
        ));
    }

    let problems = crate::doctor::problems();
    for p in &problems {
        system.push(format!("⚠️ cenv doctor: {p}"));
    }

    if context.is_empty() && system.is_empty() {
        return;
    }
    let mut out = json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context.join("\n\n"),
        }
    });
    if !system.is_empty() {
        out["systemMessage"] = json!(system.join("\n"));
    }
    println!("{out}");
}
