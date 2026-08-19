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

/// An export already on disk for this session id, found by the `__<sid8>.md`
/// suffix every filename carries.
fn existing_export_for(out_dir: &Path, sid: &str) -> Option<PathBuf> {
    let suffix = format!("__{}.md", sid.chars().take(8).collect::<String>());
    fs::read_dir(out_dir).ok()?.flatten().find_map(|e| {
        let p = e.path();
        p.file_name()?
            .to_string_lossy()
            .ends_with(&suffix)
            .then_some(p)
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
        file: out_path.file_name().unwrap().to_string_lossy().into_owned(),
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
        st.out_path = existing_export_for(&out_dir, session_id)
            .unwrap_or_else(|| out_dir.join(filename_for(&st.meta, session_id)));
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
    let out_path = if archive_dir.is_none() {
        state::load_session(&sid)
            .filter(|s| !s.out_path.as_os_str().is_empty())
            .map(|s| s.out_path)
            .or_else(|| existing_export_for(&out_dir, &sid))
            .unwrap_or_else(|| out_dir.join(filename_for(&meta, &sid)))
    } else {
        out_dir.join(filename_for(&meta, &sid))
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
    let out_dir = st.out_path.parent().unwrap().to_path_buf();
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
    for entry in fs::read_dir(store_dir)? {
        let p = entry?.path();
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if p.extension().and_then(|e| e.to_str()) != Some("md") || name == "INDEX.md" {
            continue;
        }
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
