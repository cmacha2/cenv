//! Incremental capture state. One JSON file per session (no shared write
//! contention between concurrently-open Claude sessions) plus a per-store
//! index cache so INDEX.md never requires re-reading every export.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::render::IndexRow;
use crate::transcript::Meta;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionState {
    pub transcript: PathBuf,
    pub offset: u64,
    pub out_path: PathBuf,
    pub meta: Meta,
    /// Transcript mtime (secs) at the time we last paid for an LLM pass.
    pub llm_done_mtime: Option<i64>,
}

fn session_file(session_id: &str) -> PathBuf {
    paths::state_dir()
        .join("sessions")
        .join(format!("{}.json", paths::slugify(session_id, 64)))
}

pub fn load_session(session_id: &str) -> Option<SessionState> {
    let raw = fs::read_to_string(session_file(session_id)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_session(session_id: &str, st: &SessionState) -> Result<()> {
    let path = session_file(session_id);
    write_atomic(&path, &serde_json::to_vec_pretty(st)?)
}

/// Every tracked session, as (session id, state), newest transcript first.
/// The id is recovered from the state itself so a slugified filename can never
/// desynchronize from it.
pub fn all_sessions() -> Vec<(String, SessionState)> {
    let mut out: Vec<(String, SessionState)> = fs::read_dir(paths::state_dir().join("sessions"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .filter_map(|e| fs::read_to_string(e.path()).ok())
                .filter_map(|raw| serde_json::from_str::<SessionState>(&raw).ok())
                .filter_map(|st| st.meta.session.clone().map(|id| (id, st)))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| b.1.meta.updated.cmp(&a.1.meta.updated));
    out
}

/// Write via temp file + rename, so a reader never sees a half-written file and
/// an interrupted write leaves the previous contents intact.
///
/// A symlink target is resolved first: renaming onto the link itself would
/// replace it with a regular file, quietly detaching a config that is meant to
/// live in the env repo.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let resolved = if path.is_symlink() {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let path = resolved.as_path();
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IndexCache {
    pub rows: Vec<IndexRow>,
}

/// The index is stored as one small file per session rather than a single
/// shared document. Sessions of the same project write concurrently, and a
/// read-modify-write of one shared file loses whichever row lands first —
/// permanently, when the losing session has already ended. Row files are
/// disjoint, so writers never contend, and INDEX.md is a rendering of them.
fn index_dir(store_dir: &Path) -> PathBuf {
    store_dir.join(".index")
}

fn row_path(store_dir: &Path, row_file: &str) -> PathBuf {
    let key: String = row_file
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    index_dir(store_dir).join(format!("{key}.json"))
}

pub fn load_index(store_dir: &Path) -> IndexCache {
    let mut rows: Vec<IndexRow> = fs::read_dir(index_dir(store_dir))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .filter_map(|e| fs::read_to_string(e.path()).ok())
                .filter_map(|raw| serde_json::from_str::<IndexRow>(&raw).ok())
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| b.started.cmp(&a.started));
    IndexCache { rows }
}

pub fn upsert_index_row(store_dir: &Path, row: IndexRow) -> Result<IndexCache> {
    write_atomic(&row_path(store_dir, &row.file), &serde_json::to_vec(&row)?)?;
    Ok(load_index(store_dir))
}

pub fn replace_index(store_dir: &Path, rows: Vec<IndexRow>) -> Result<IndexCache> {
    let dir = index_dir(store_dir);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    for row in &rows {
        write_atomic(&row_path(store_dir, &row.file), &serde_json::to_vec(row)?)?;
    }
    // Legacy single-file cache from earlier versions; harmless but stale.
    let _ = fs::remove_file(store_dir.join(".index.json"));
    Ok(IndexCache { rows })
}
