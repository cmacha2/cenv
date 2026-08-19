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

pub fn stop() {
    if paths::locked() {
        return;
    }
    let input = read_input();
    let Some(transcript) = usable_transcript(&input) else {
        return;
    };
    let _ = export::export_incremental(&transcript, &input.session_id);
}

pub fn session_end() {
    if paths::locked() {
        return;
    }
    let input = read_input();
    let Some(transcript) = usable_transcript(&input) else {
        return;
    };
    let _ = export::export_incremental(&transcript, &input.session_id);
    let _ = analyze_session(&transcript, &input.session_id);
}

/// The single LLM pass: summary + rule candidates in one `claude -p` call.
/// Skips trivial sessions and anything already analyzed at this mtime.
fn analyze_session(transcript: &Path, session_id: &str) -> anyhow::Result<()> {
    let cfg = config::load();
    let local = config::load_local();

    let Some(mut st) = state::load_session(session_id) else {
        return Ok(());
    };
    if st.meta.exchanges < cfg.llm.min_exchanges {
        return Ok(());
    }
    let mtime = fs::metadata(transcript)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if st.llm_done_mtime.is_some_and(|done| done >= mtime) {
        return Ok(());
    }

    let events = transcript::read_all_events(transcript)?;
    let convo = transcript::plain_conversation(&events, 12_000);
    if convo.len() < cfg.llm.min_chars {
        return Ok(());
    }
    let Some(analysis) = llm::analyze(&convo, &cfg.llm)? else {
        return Ok(());
    };

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
