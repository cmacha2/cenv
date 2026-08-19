//! Layered config: `config.toml` (synced, lives in the env repo) plus
//! `config.local.toml` (machine-local, gitignored: memory whitelist and
//! per-project opt-ins). Both optional — cenv works with pure defaults.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Config {
    pub capture: Capture,
    pub llm: Llm,
    pub sync: Sync,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Capture {
    /// "conversation" | "tools" | "full"
    pub detail: String,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            detail: "conversation".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Llm {
    pub model: String,
    pub timeout_secs: u64,
    /// Sessions below these thresholds are not worth a model call.
    pub min_exchanges: usize,
    pub min_chars: usize,
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            model: "haiku".into(),
            timeout_secs: 120,
            min_exchanges: 2,
            min_chars: 400,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Sync {
    /// "range" scans only commits not yet on the upstream; "full" scans all history.
    pub scan: String,
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            scan: "range".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct LocalConfig {
    /// Project memory directories to mirror into the env repo on `cenv sync`.
    pub memory: Vec<MemoryEntry>,
    /// Keyed by absolute project path.
    pub projects: BTreeMap<String, ProjectConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ProjectConfig {
    /// Set by `cenv adopt`: export history into `<project>/history/` instead
    /// of the central store.
    pub history_in_repo: bool,
}

fn first_existing(name: &str) -> Option<PathBuf> {
    [
        paths::env_repo().join(name),
        paths::config_fallback_dir().join(name),
    ]
    .into_iter()
    .find(|p| p.exists())
}

fn load_toml<T: Default + for<'de> Deserialize<'de>>(path: Option<PathBuf>) -> T {
    let Some(path) = path else {
        return T::default();
    };
    match fs::read_to_string(&path)
        .map_err(anyhow::Error::from)
        .and_then(|s| Ok(toml::from_str(&s)?))
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cenv: ignoring unreadable {}: {e}", path.display());
            T::default()
        }
    }
}

pub fn load() -> Config {
    load_toml(first_existing("config.toml"))
}

pub fn load_local() -> LocalConfig {
    load_toml(first_existing("config.local.toml"))
}

pub fn local_config_path() -> PathBuf {
    if paths::env_repo().is_dir() {
        paths::env_repo().join("config.local.toml")
    } else {
        paths::config_fallback_dir().join("config.local.toml")
    }
}

pub fn save_local(cfg: &LocalConfig) -> anyhow::Result<PathBuf> {
    let path = local_config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, toml::to_string_pretty(cfg)?)?;
    Ok(path)
}

/// Where a session's history lives, honoring `cenv adopt`.
///
/// The central store is keyed by project name, which is just the last path
/// component — two checkouts named `api` would otherwise share one directory
/// and one index. The first one to claim a name keeps it (recorded in
/// `.project`); later ones get the name plus a short hash of their full path.
pub fn history_dir_for(cwd: Option<&str>, local: &LocalConfig) -> PathBuf {
    if let Some(cwd) = cwd
        && local.projects.get(cwd).is_some_and(|p| p.history_in_repo)
        && Path::new(cwd).is_dir()
    {
        return Path::new(cwd).join("history");
    }
    let slug = paths::slugify(&paths::project_name(cwd), 40);
    let store = paths::history_store();
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return store.join(slug);
    };

    let preferred = store.join(&slug);
    let marker = preferred.join(".project");
    match fs::read_to_string(&marker) {
        Ok(owner) if owner.trim() != cwd => {
            store.join(format!("{slug}-{}", paths::short_hash(cwd)))
        }
        Ok(_) => preferred,
        Err(_) => {
            if preferred.is_dir() {
                // Pre-existing store from before this marker existed: adopt it
                // rather than orphaning the history already in there.
                let _ = fs::write(&marker, format!("{cwd}\n"));
                return preferred;
            }
            if fs::create_dir_all(&preferred).is_ok() {
                let _ = fs::write(&marker, format!("{cwd}\n"));
            }
            preferred
        }
    }
}
