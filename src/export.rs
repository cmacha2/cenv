//! The export pipeline: transcript `.jsonl` → readable Markdown + INDEX.md.
//!
//! Hook path is incremental: only lines after the stored per-session offset are
//! parsed, the rendered chunk is appended after the body marker, and the header
//! is regenerated from cached meta. Manual/backfill paths do a full rebuild.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config;
use crate::paths;
use crate::render::{self, BODY_MARKER, IndexRow};
use crate::state::{self, SessionState};
use crate::transcript::{self, Meta};

pub struct Outcome {
    pub out_path: PathBuf,
}

fn sidecar_path(out_dir: &Path, session_id: &str) -> PathBuf {
    out_dir.join(".summaries").join(format!("{session_id}.md"))
}

pub fn read_sidecar(out_dir: &Path, session_id: Option<&str>) -> String {
    session_id
        .and_then(|sid| fs::read_to_string(sidecar_path(out_dir, sid)).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn write_sidecar(out_dir: &Path, session_id: &str, prose: &str) -> Result<()> {
    let p = sidecar_path(out_dir, session_id);
    fs::create_dir_all(p.parent().unwrap())?;
    fs::write(p, format!("{}\n", prose.trim()))?;
    Ok(())
}

fn filename_for(meta: &Meta, sid: &str) -> String {
    let date = render::date_of(meta.started.as_deref());
    let date = if date.is_empty() {
        "undated".into()
    } else {
        date
    };
    let project = paths::slugify(&paths::project_name(meta.cwd.as_deref()), 30);
    let title = paths::slugify(&render::title_of(meta), 45);
    let sid8: String = sid.chars().take(8).collect();
    format!("{date}_{project}_{title}__{sid8}.md")
}

/// Where an export goes *relative to its store root* — the month layout nests
/// it under `sessions/<YYYY-MM>/`, keeping the store root free of the hundreds
/// of files a year of sessions produces.
fn relpath_for(meta: &Meta, sid: &str, layout: &str) -> PathBuf {
    let name = filename_for(meta, sid);
    if layout == config::LAYOUT_FLAT {
        return PathBuf::from(name);
    }
    Path::new(paths::SESSIONS_DIR)
        .join(paths::month_bucket(&render::date_of(
            meta.started.as_deref(),
        )))
        .join(name)
}

/// The store root an export path belongs to, for either layout: a bucketed
/// export sits two levels down (`<store>/sessions/<bucket>/x.md`), a flat one
/// directly in the root. Derived from the path itself rather than recomputed
/// from config, so it stays right for exports written under the other layout.
pub fn store_root_of(out_path: &Path) -> PathBuf {
    let parent = out_path.parent().unwrap_or(Path::new("."));
    if parent
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == paths::SESSIONS_DIR)
        && let Some(root) = parent.parent().and_then(|p| p.parent())
    {
        return root.to_path_buf();
    }
    parent.to_path_buf()
}

/// Store-relative path with `/` separators — what an INDEX.md link needs, and
/// the identity of a row in the index cache.
fn rel_str(store: &Path, p: &Path) -> String {
    p.strip_prefix(store)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Every export under a store, at whatever depth the layout put it. Dot-dirs
/// (`.index`, `.summaries`) and the store's own generated files are skipped,
/// and symlinks are never followed — a linked directory could otherwise walk
/// out of the store or into itself.
pub fn walk_exports(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let p = e.path();
        if ft.is_dir() {
            walk_exports(&p, out);
            continue;
        }
        if p.extension().and_then(|x| x.to_str()) != Some("md")
            || name == "INDEX.md"
            || name == crate::distill::STAGING_NAME
        {
            continue;
        }
        out.push(p);
    }
}

/// An export already on disk for this session id, found by the `__<sid8>.md`
/// suffix every filename carries. Searches the whole store: the file may have
/// been written under a different layout than the one now configured.
fn existing_export_for(out_dir: &Path, sid: &str) -> Option<PathBuf> {
    let suffix = format!("__{}.md", sid.chars().take(8).collect::<String>());
    let mut found = Vec::new();
    walk_exports(out_dir, &mut found);
    found.into_iter().find(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
    })
}

fn write_output(out_path: &Path, meta: &Meta, prose: &str, body: &str) -> Result<()> {
    let content = format!(
        "{}{}\n",
        render::header(meta, prose),
        body.trim_start_matches('\n')
    );
    state::write_atomic(out_path, content.as_bytes())
}

/// The rendered body of an existing export, i.e. everything after the marker.
/// `None` means there is nothing safe to append to — the file is gone, or its
/// marker is missing — and the caller must re-render from scratch instead of
/// silently dropping the conversation so far.
fn existing_body(out_path: &Path) -> Option<String> {
    let content = fs::read_to_string(out_path).ok()?;
    content
        .split_once(BODY_MARKER)
        .map(|(_, b)| b.trim_start_matches('\n').to_string())
}

fn refresh_index(out_dir: &Path, meta: &Meta, out_path: &Path, prose: &str) -> Result<()> {
    let summary = prose
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .unwrap_or_else(|| render::trim_to(&meta.first_user, 110));
    let row = IndexRow {
        file: rel_str(out_dir, out_path),
        started: render::fmt_time(meta.started.as_deref()),
        title: render::title_of(meta),
        summary,
        files_touched: meta.files_touched.clone(),
    };
    let cache = state::upsert_index_row(out_dir, row)?;
    let project = paths::project_name(meta.cwd.as_deref());
    state::write_atomic(
        &out_dir.join("INDEX.md"),
        render::index_markdown(&project, &cache.rows).as_bytes(),
    )
}

/// Incremental export driven by a hook: parses only what's new.
///
/// Falls back to a full rebuild whenever the append cannot be trusted — no
/// state yet, the transcript moved or shrank, or the export it should append to
/// is gone. Appending blindly in that last case would keep the cumulative
/// header (which still claims every exchange) over a body holding only the
/// newest turn.
pub fn export_incremental(transcript: &Path, session_id: &str) -> Result<Option<Outcome>> {
    let cfg = config::load();
    let local = config::load_local();

    let prior = state::load_session(session_id)
        .filter(|s| s.transcript == transcript)
        .filter(|s| {
            fs::metadata(transcript)
                .map(|m| m.len() >= s.offset)
                .unwrap_or(false)
        })
        // The markdown must still be there, with its marker, to append to.
        .filter(|s| {
            s.offset == 0
                || (!s.out_path.as_os_str().is_empty() && existing_body(&s.out_path).is_some())
        });

    let (mut st, from_offset) = match prior {
        Some(s) => {
            let off = s.offset;
            (s, off)
        }
        None => (
            SessionState {
                transcript: transcript.to_path_buf(),
                ..Default::default()
            },
            0,
        ),
    };

    let (events, new_offset) = transcript::read_events_from(transcript, from_offset)
        .with_context(|| format!("reading {}", transcript.display()))?;
    if events.is_empty() && from_offset > 0 {
        return Ok(None); // nothing new
    }
    st.meta.absorb(&events);
    if st.meta.session.is_none() {
        st.meta.session = Some(session_id.to_string());
    }
    if st.meta.first_user.is_empty() && from_offset == 0 && events.is_empty() {
        return Ok(None); // empty transcript
    }

    let out_dir = config::history_dir_for(st.meta.cwd.as_deref(), &local);
    fs::create_dir_all(&out_dir)?;

    if st.out_path.as_os_str().is_empty() {
        st.out_path = existing_export_for(&out_dir, session_id).unwrap_or_else(|| {
            out_dir.join(relpath_for(&st.meta, session_id, &cfg.capture.layout))
        });
    }

    let prose = read_sidecar(&out_dir, Some(session_id));
    let old_body = if from_offset > 0 {
        existing_body(&st.out_path).unwrap_or_default()
    } else {
        String::new()
    };
    let chunk = render::conversation(&events, &cfg.capture.detail);
    let body = if old_body.is_empty() {
        chunk
    } else if chunk.is_empty() {
        old_body
    } else {
        format!("{}\n{}", old_body.trim_end(), chunk)
    };

    write_output(&st.out_path, &st.meta, &prose, &body)?;
    refresh_index(&out_dir, &st.meta, &st.out_path.clone(), &prose)?;

    st.offset = new_offset;
    state::save_session(session_id, &st)?;
    Ok(Some(Outcome {
        out_path: st.out_path.clone(),
    }))
}

/// Full rebuild of one transcript. `archive_dir` overrides the destination
/// (backfill mode) and skips per-session state entirely.
pub fn export_full(transcript: &Path, archive_dir: Option<&Path>) -> Result<Option<Outcome>> {
    let cfg = config::load();
    let local = config::load_local();

    let (events, consumed) = transcript::read_all_events_with_offset(transcript)?;
    if events.is_empty() {
        return Ok(None);
    }
    let mut meta = Meta::default();
    meta.absorb(&events);
    let sid = meta
        .session
        .clone()
        .or_else(|| {
            transcript
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown".into());

    let out_dir = match archive_dir {
        Some(dir) => dir.join(paths::slugify(
            &paths::project_name(meta.cwd.as_deref()),
            40,
        )),
        None => config::history_dir_for(meta.cwd.as_deref(), &local),
    };
    fs::create_dir_all(&out_dir)?;

    // A tracked session keeps the filename it already has: the recorded one, or
    // an export found on disk for this session id (state can be lost while the
    // markdown survives — minting a new name there would orphan the old file
    // and leave two index rows for one session).
    //
    // Archive mode names purely from content, which is already stable across
    // re-runs, and stays strictly one file per transcript: reusing a name by
    // session id could make two transcripts overwrite each other.
    let relpath = relpath_for(&meta, &sid, &cfg.capture.layout);
    let out_path = if archive_dir.is_none() {
        state::load_session(&sid)
            .filter(|s| !s.out_path.as_os_str().is_empty())
            .map(|s| s.out_path)
            .or_else(|| existing_export_for(&out_dir, &sid))
            .unwrap_or_else(|| out_dir.join(&relpath))
    } else {
        out_dir.join(&relpath)
    };

    let prose = read_sidecar(&out_dir, Some(&sid));
    let body = render::conversation(&events, &cfg.capture.detail);
    write_output(&out_path, &meta, &prose, &body)?;
    refresh_index(&out_dir, &meta, &out_path, &prose)?;

    if archive_dir.is_none() {
        let offset = consumed;
        let llm_done = state::load_session(&sid).and_then(|s| s.llm_done_mtime);
        state::save_session(
            &sid,
            &SessionState {
                transcript: transcript.to_path_buf(),
                offset,
                out_path: out_path.clone(),
                meta: meta.clone(),
                llm_done_mtime: llm_done,
            },
        )?;
    }
    Ok(Some(Outcome { out_path }))
}

/// After an LLM summary lands, refresh header + index without touching the body.
pub fn refresh_header(session_id: &str) -> Result<()> {
    let Some(st) = state::load_session(session_id) else {
        return Ok(());
    };
    if st.out_path.as_os_str().is_empty() || !st.out_path.exists() {
        return Ok(());
    }
    let out_dir = store_root_of(&st.out_path);
    let prose = read_sidecar(&out_dir, Some(session_id));
    // No marker means no body boundary to preserve — leave the file alone
    // rather than rewriting it from a header plus nothing.
    let Some(body) = existing_body(&st.out_path) else {
        return Ok(());
    };
    write_output(&st.out_path, &st.meta, &prose, &body)?;
    refresh_index(&out_dir, &st.meta, &st.out_path, &prose)
}

/// Rebuild the index cache of a store dir by re-reading every export's
/// frontmatter (recovery path; normal operation never needs it).
pub fn reindex(store_dir: &Path) -> Result<usize> {
    let mut rows = Vec::new();
    let mut project = String::from("unknown");
    let mut exports = Vec::new();
    walk_exports(store_dir, &mut exports);
    for p in exports {
        let name = rel_str(store_dir, &p);
        let Ok(content) = fs::read_to_string(&p) else {
            continue;
        };
        let fm = parse_frontmatter(&content);
        let get = |k: &str| fm.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !get("project").is_empty() {
            project = get("project");
        }
        rows.push(IndexRow {
            file: name,
            started: get("started"),
            title: get("title"),
            summary: get("summary"),
            files_touched: fm
                .get("files_touched")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    let n = rows.len();
    let cache = state::replace_index(store_dir, rows)?;
    state::write_atomic(
        &store_dir.join("INDEX.md"),
        render::index_markdown(&project, &cache.rows).as_bytes(),
    )?;
    Ok(n)
}

/// One export relocated by `reorganize`.
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Default)]
pub struct Plan {
    pub moves: Vec<Move>,
    /// Exports whose destination is already taken — reported, never overwritten.
    pub conflicts: Vec<PathBuf>,
}

/// Bring a store's existing exports under the configured layout.
///
/// Filenames are never rewritten — only the directory they sit in — so links
/// people already saved keep resolving to the same basename, and the `__<sid8>`
/// lookup keeps working. The month comes from the filename's own date prefix,
/// which is what named the file in the first place, so no file is read to place
/// it. Reports the moves without touching anything unless `apply`.
pub fn reorganize(store_dir: &Path, layout: &str, apply: bool) -> Result<Plan> {
    let mut exports = Vec::new();
    walk_exports(store_dir, &mut exports);
    exports.sort();

    let mut plan = Plan::default();
    for from in exports {
        let name = from
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let want = if layout == config::LAYOUT_FLAT {
            PathBuf::from(&name)
        } else {
            Path::new(paths::SESSIONS_DIR)
                .join(paths::month_bucket(&name))
                .join(&name)
        };
        let to = store_dir.join(&want);
        if to == from {
            continue;
        }
        if to.exists() {
            plan.conflicts.push(from);
            continue;
        }
        plan.moves.push(Move { from, to });
    }
    if !apply || plan.moves.is_empty() {
        return Ok(plan);
    }

    for m in &plan.moves {
        fs::create_dir_all(m.to.parent().unwrap_or(store_dir))?;
        fs::rename(&m.from, &m.to).with_context(|| format!("moving {}", m.from.display()))?;
    }
    // Empty month buckets (or the store root) left behind by a layout flip.
    prune_empty_dirs(store_dir);
    // Row keys are derived from the store-relative path, so every row moved.
    reindex(store_dir)?;
    repoint_state(&plan.moves)?;
    Ok(plan)
}

/// A session's recorded `out_path` must follow its export, or the next Stop
/// hook finds nothing to append to and re-renders the whole transcript.
fn repoint_state(moves: &[Move]) -> Result<()> {
    for (sid, mut st) in state::all_sessions() {
        if let Some(m) = moves.iter().find(|m| m.from == st.out_path) {
            st.out_path = m.to.clone();
            state::save_session(&sid, &st)?;
        }
    }
    Ok(())
}

fn prune_empty_dirs(store_dir: &Path) {
    let sessions = store_dir.join(paths::SESSIONS_DIR);
    if let Ok(rd) = fs::read_dir(&sessions) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = fs::remove_dir(e.path()); // only succeeds when empty
            }
        }
    }
    let _ = fs::remove_dir(&sessions);
}

pub fn parse_frontmatter(content: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return map;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim();
            let parsed = serde_json::from_str(v).unwrap_or(serde_json::Value::from(v));
            map.insert(k.trim().to_string(), parsed);
        }
    }
    map
}

/// Transcripts for the project whose cwd matches `cwd`, newest first.
/// Scoped: never returns another project's sessions.
pub fn transcripts_for_cwd(cwd: &Path) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(dirs) = fs::read_dir(paths::projects_dir()) else {
        return Vec::new();
    };
    for d in dirs.flatten() {
        let dir = d.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if transcript_cwd(&p).as_deref() == Some(cwd.to_string_lossy().as_ref())
                && let Ok(m) = fs::metadata(&p)
            {
                found.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), p));
            }
        }
    }
    found.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Cheap cwd sniff: first line that carries a `cwd` field.
pub fn transcript_cwd(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(f).lines().take(25).flatten() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some(c) = v.get("cwd").and_then(|c| c.as_str())
        {
            return Some(c.to_string());
        }
    }
    None
}
