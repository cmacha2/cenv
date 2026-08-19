//! CLAUDE.md rule distillation. Candidates land in a human-reviewed staging
//! file (`- [ ]` checkboxes with JSON metadata in an HTML comment); `apply`
//! moves checked items into a clearly-marked managed block. Nothing is ever
//! written to a CLAUDE.md unattended, and the global one requires --confirm.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::llm::Rule;
use crate::paths;

pub const STAGING_NAME: &str = "claude-md-candidates.md";
const BLOCK_BEGIN: &str = "<!-- cenv:distilled:begin -->";
const BLOCK_END: &str = "<!-- cenv:distilled:end -->";

pub fn global_staging() -> PathBuf {
    paths::claude_dir().join("claude-md-candidates-global.md")
}

pub fn global_claude_md() -> PathBuf {
    paths::claude_dir().join("CLAUDE.md")
}

fn norm(s: &str) -> String {
    let mut out = String::new();
    let mut sp = true;
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            sp = false;
        } else if !sp {
            out.push(' ');
            sp = true;
        }
    }
    out.trim().to_string()
}

pub fn similar(a: &str, b: &str) -> bool {
    strsim::normalized_levenshtein(&norm(a), &norm(b)) >= 0.75
}

pub struct StagedItem {
    pub checked: bool,
    pub meta: Value,
    pub raw: String,
}

pub fn read_staging(path: &Path) -> Vec<StagedItem> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let start = line.find("<!--{")? + 4;
            let end = line.rfind("}-->")? + 1;
            let meta: Value = serde_json::from_str(&line[start..end]).ok()?;
            let trimmed = line.trim_start();
            let checked = trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]");
            Some(StagedItem {
                checked,
                meta,
                raw: line.to_string(),
            })
        })
        .collect()
}

fn staging_line(meta: &Value) -> String {
    let text = meta.get("text").and_then(Value::as_str).unwrap_or("");
    let cat = meta.get("category").and_then(Value::as_str).unwrap_or("?");
    let conf = meta
        .get("confidence")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let ev = meta.get("evidence").and_then(Value::as_str).unwrap_or("");
    let ev_s = if ev.is_empty() {
        String::new()
    } else {
        format!("  _\u{201c}{ev}\u{201d}_")
    };
    format!("- [ ] **[{cat}·{conf}]** {text}{ev_s}  <!--{meta}-->")
}

/// Append new candidates, deduplicating against the staging file itself and
/// against rules already present in the target CLAUDE.md. Returns how many landed.
pub fn append_candidates(
    staging: &Path,
    header: &str,
    rules: &[Rule],
    project: &str,
    session: Option<&str>,
    claude_md: Option<&Path>,
) -> Result<usize> {
    let mut seen: Vec<String> = read_staging(staging)
        .iter()
        .filter_map(|i| i.meta.get("text").and_then(Value::as_str).map(norm))
        .collect();
    let existing_md = claude_md
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|c| norm(&c))
        .unwrap_or_default();

    let mut new_lines = Vec::new();
    for r in rules {
        let text = r.text.trim();
        if text.is_empty() || !matches!(r.confidence.as_str(), "medium" | "high") {
            continue;
        }
        let n = norm(text);
        if n.is_empty() || seen.iter().any(|s| s == &n) || existing_md.contains(&n) {
            continue;
        }
        seen.push(n);
        let mut meta = json!({
            "text": text,
            "category": r.category,
            "confidence": r.confidence,
            "evidence": r.evidence,
            "project": project,
        });
        if let Some(sid) = session {
            meta["session"] = json!(sid.chars().take(8).collect::<String>());
        }
        new_lines.push(staging_line(&meta));
    }
    if new_lines.is_empty() {
        return Ok(0);
    }
    if let Some(dir) = staging.parent() {
        fs::create_dir_all(dir)?;
    }
    let fresh = !staging.exists();
    let mut content = if fresh {
        header.to_string()
    } else {
        fs::read_to_string(staging)?
    };
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&new_lines.join("\n"));
    content.push('\n');
    fs::write(staging, content)?;
    Ok(new_lines.len())
}

pub fn project_staging_header(project: &str, dir: &Path) -> String {
    format!(
        "# CLAUDE.md candidates — {project}\n\n\
         Machine-proposed rules distilled from this project's sessions. **Nothing here is\n\
         applied yet.** Check `[x]` the ones you want, then run:\n\n\
         ```\ncenv distill apply --path {}\n```\n\n## Pending\n",
        dir.display()
    )
}

/// Insert texts into the target's managed block (created at end if absent).
fn apply_to_claude_md(target: &Path, texts: &[String]) -> Result<()> {
    let content = fs::read_to_string(target).unwrap_or_default();
    let body: String = texts.iter().map(|t| format!("- {t}\n")).collect();
    let new = if let Some((pre, rest)) = content.split_once(BLOCK_BEGIN) {
        let (inner, post) = rest.split_once(BLOCK_END).unwrap_or((rest, ""));
        format!(
            "{pre}{BLOCK_BEGIN}{}\n{body}{BLOCK_END}{post}",
            inner.trim_end()
        )
    } else {
        let sep = if content.is_empty() || content.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!(
            "{content}{sep}\n{BLOCK_BEGIN}\n## Distilled from past sessions\n{body}{BLOCK_END}\n"
        )
    };
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(target, new)?;
    Ok(())
}

/// Apply checked staging items into the CLAUDE.md next to them, then drop the
/// applied lines from staging. Dry-run prints instead when `confirm` is false
/// and the scope is global.
pub fn apply(staging: &Path, target: &Path, global: bool, confirm: bool) -> Result<()> {
    let items = read_staging(staging);
    let checked: Vec<String> = items
        .iter()
        .filter(|i| i.checked)
        .filter_map(|i| i.meta.get("text").and_then(Value::as_str).map(String::from))
        .collect();
    if checked.is_empty() {
        println!(
            "No checked [x] candidates in {}. Nothing to apply.",
            staging.display()
        );
        return Ok(());
    }
    if global && !confirm {
        println!("DRY RUN — would add to {}:", target.display());
        for t in &checked {
            println!("  + {t}");
        }
        println!("\nRe-run with --confirm to apply.");
        return Ok(());
    }
    apply_to_claude_md(target, &checked)?;

    let content = fs::read_to_string(staging)?;
    let head = content.split("## Pending").next().unwrap_or("").to_string();
    let remaining: Vec<&str> = items
        .iter()
        .filter(|i| !i.checked)
        .map(|i| i.raw.as_str())
        .collect();
    let mut out = format!("{head}## Pending\n");
    if !remaining.is_empty() {
        out.push_str(&remaining.join("\n"));
        out.push('\n');
    }
    fs::write(staging, out)?;
    println!("Applied {} rule(s) -> {}", checked.len(), target.display());
    Ok(())
}

/// Cluster candidates across every project's staging file; anything recurring
/// in >= 2 distinct projects becomes a global candidate.
pub fn scan_global(store_dirs: &[PathBuf]) -> Result<()> {
    let mut items: Vec<Value> = Vec::new();
    for dir in store_dirs {
        for item in read_staging(&dir.join(STAGING_NAME)) {
            if item.meta.get("text").and_then(Value::as_str).is_some() {
                items.push(item.meta);
            }
        }
    }

    let mut used = vec![false; items.len()];
    let mut globals: Vec<Rule> = Vec::new();
    for i in 0..items.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let text_i = items[i]["text"].as_str().unwrap_or("").to_string();
        let mut cluster = vec![&items[i]];
        for j in (i + 1)..items.len() {
            if !used[j] && similar(&text_i, items[j]["text"].as_str().unwrap_or("")) {
                used[j] = true;
                cluster.push(&items[j]);
            }
        }
        let projects: std::collections::BTreeSet<&str> = cluster
            .iter()
            .filter_map(|c| c.get("project").and_then(Value::as_str))
            .collect();
        if projects.len() < 2 {
            continue;
        }
        let rep = cluster
            .iter()
            .max_by_key(|c| {
                (
                    c.get("confidence").and_then(Value::as_str) == Some("high"),
                    c.get("text")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or(0),
                )
            })
            .unwrap();
        globals.push(Rule {
            text: rep["text"].as_str().unwrap_or("").to_string(),
            category: rep
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("preference")
                .into(),
            confidence: "high".into(),
            evidence: format!("recurs in {} projects", projects.len()),
        });
    }

    let header = "# GLOBAL CLAUDE.md candidates\n\n\
         Preferences that recurred across **2+ projects**. **Nothing here is applied yet.**\n\
         Check `[x]` the ones you want, then run:\n\n\
         ```\ncenv distill apply --global --confirm\n```\n\n## Pending\n"
        .to_string();
    let n = append_candidates(
        &global_staging(),
        &header,
        &globals,
        "global",
        None,
        Some(&global_claude_md()),
    )?;
    if n > 0 {
        println!("{n} global candidate(s) -> {}", global_staging().display());
    } else {
        println!("No new cross-project candidates.");
    }
    Ok(())
}

/// Every history store dir that has a staging file (central store + adopted repos).
pub fn all_store_dirs(local: &crate::config::LocalConfig) -> Vec<PathBuf> {
    let mut dirs: BTreeMap<PathBuf, ()> = BTreeMap::new();
    if let Ok(rd) = fs::read_dir(paths::history_store()) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                dirs.insert(e.path(), ());
            }
        }
    }
    for (proj, cfg) in &local.projects {
        if cfg.history_in_repo {
            dirs.insert(Path::new(proj).join("history"), ());
        }
    }
    dirs.into_keys().collect()
}

/// Pending (unchecked) candidate count for a project — the SessionStart nudge.
pub fn pending_count(store_dir: &Path) -> usize {
    read_staging(&store_dir.join(STAGING_NAME))
        .iter()
        .filter(|i| !i.checked)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join(STAGING_NAME);
        let rules = vec![
            Rule {
                text: "Always use uv".into(),
                category: "preference".into(),
                confidence: "high".into(),
                evidence: "use uv".into(),
            },
            Rule {
                text: "low conf".into(),
                category: "fact".into(),
                confidence: "low".into(),
                evidence: "".into(),
            },
        ];
        let n = append_candidates(
            &staging,
            "# H\n\n## Pending\n",
            &rules,
            "demo",
            Some("abcd1234efgh"),
            None,
        )
        .unwrap();
        assert_eq!(n, 1); // low confidence dropped
        // re-append dedups
        let n2 =
            append_candidates(&staging, "# H\n\n## Pending\n", &rules, "demo", None, None).unwrap();
        assert_eq!(n2, 0);
        let items = read_staging(&staging);
        assert_eq!(items.len(), 1);
        assert!(!items[0].checked);
    }

    #[test]
    fn apply_moves_checked_into_block() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join(STAGING_NAME);
        let target = dir.path().join("CLAUDE.md");
        let rules = vec![Rule {
            text: "Use pathlib".into(),
            category: "convention".into(),
            confidence: "medium".into(),
            evidence: "".into(),
        }];
        append_candidates(&staging, "# H\n\n## Pending\n", &rules, "demo", None, None).unwrap();
        let content = fs::read_to_string(&staging)
            .unwrap()
            .replace("- [ ]", "- [x]");
        fs::write(&staging, content).unwrap();
        apply(&staging, &target, false, false).unwrap();
        let md = fs::read_to_string(&target).unwrap();
        assert!(md.contains("- Use pathlib"));
        assert!(md.contains(BLOCK_BEGIN));
        assert_eq!(read_staging(&staging).len(), 0);
        // applying again into the existing block keeps it single
        append_candidates(
            &staging,
            "# H\n\n## Pending\n",
            &[Rule {
                text: "Another rule here".into(),
                category: "fact".into(),
                confidence: "high".into(),
                evidence: "".into(),
            }],
            "demo",
            None,
            None,
        )
        .unwrap();
        let content = fs::read_to_string(&staging)
            .unwrap()
            .replace("- [ ]", "- [x]");
        fs::write(&staging, content).unwrap();
        apply(&staging, &target, false, false).unwrap();
        let md = fs::read_to_string(&target).unwrap();
        assert_eq!(md.matches(BLOCK_BEGIN).count(), 1);
        assert!(md.contains("- Use pathlib"));
        assert!(md.contains("- Another rule here"));
    }

    #[test]
    fn similarity() {
        assert!(similar(
            "Always use uv for python",
            "always use UV for Python!"
        ));
        assert!(!similar("Always use uv", "Never commit secrets"));
    }
}
