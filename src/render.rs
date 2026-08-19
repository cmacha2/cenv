//! Markdown rendering: the export header (frontmatter + summary block), the
//! conversation body, and the per-project INDEX.md.

use jiff::Timestamp;
use serde_json::Value;

use crate::paths::project_name;
use crate::transcript::{Event, Meta, text_of};

pub const BODY_MARKER: &str = "<!-- cenv:body -->";

pub fn fmt_time(ts: Option<&str>) -> String {
    let Some(ts) = ts else { return String::new() };
    match ts.parse::<Timestamp>() {
        Ok(t) => t
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
        Err(_) => ts.to_string(),
    }
}

pub fn date_of(ts: Option<&str>) -> String {
    let Some(ts) = ts else { return String::new() };
    match ts.parse::<Timestamp>() {
        Ok(t) => t
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d")
            .to_string(),
        Err(_) => ts.chars().take(10).collect(),
    }
}

/// Defuse a literal body marker in user-derived text. The incremental export
/// splits a file on that marker to find where the rendered body starts, so a
/// copy of it inside the header — easy to produce by asking Claude about the
/// marker itself — would make every later append re-absorb part of the header.
pub fn scrub_marker(s: &str) -> String {
    if s.contains(BODY_MARKER) {
        return s.replace(BODY_MARKER, "<!-- cenv·body -->");
    }
    s.to_string()
}

pub fn trim_to(s: &str, n: usize) -> String {
    let joined = scrub_marker(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if joined.chars().count() <= n {
        return joined;
    }
    let cut: String = joined.chars().take(n.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

fn esc_cell(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Push body headings one level deeper so they nest under turn markers;
/// fence-aware so `#` inside code blocks is untouched.
fn demote_headings(md: &str) -> String {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in md.lines() {
        let stripped = line.trim_start();
        if stripped.starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            continue;
        }
        if !in_fence {
            let hashes = stripped.chars().take_while(|c| *c == '#').count();
            if (1..=5).contains(&hashes) && stripped.chars().nth(hashes) == Some(' ') {
                out.push(format!("#{line}"));
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

pub fn title_of(meta: &Meta) -> String {
    meta.title
        .clone()
        .map(|t| scrub_marker(&t))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            let t = trim_to(&meta.first_user, 60);
            if t.is_empty() {
                "Untitled session".into()
            } else {
                t
            }
        })
}

/// Header: frontmatter + summary block, regenerated on every export from
/// cached meta. `prose` is the LLM summary sidecar, when present.
pub fn header(meta: &Meta, prose: &str) -> String {
    let prose = scrub_marker(prose);
    let title = title_of(meta);
    let one_liner = prose
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .unwrap_or_else(|| trim_to(&meta.first_user, 110));

    let fm = [
        ("project", Value::from(project_name(meta.cwd.as_deref()))),
        (
            "session",
            Value::from(meta.session.clone().unwrap_or_else(|| "unknown".into())),
        ),
        ("title", Value::from(title.clone())),
        ("started", Value::from(fmt_time(meta.started.as_deref()))),
        ("updated", Value::from(fmt_time(meta.updated.as_deref()))),
        ("exchanges", Value::from(meta.exchanges)),
        ("files_touched", Value::from(meta.files_touched.clone())),
        ("summary", Value::from(trim_to(&one_liner, 200))),
    ];

    let mut out = vec!["---".to_string()];
    for (k, v) in &fm {
        out.push(format!("{k}: {v}"));
    }
    out.push("---".into());
    out.push(String::new());
    out.push(format!("# {title}"));
    out.push(String::new());
    out.push("## 📋 Summary".into());
    out.push(String::new());
    if !prose.is_empty() {
        out.push(prose.trim().to_string());
        out.push(String::new());
    }
    if !meta.first_user.is_empty() {
        out.push(format!("- **Goal:** {}", trim_to(&meta.first_user, 240)));
    }
    let files = if meta.files_touched.is_empty() {
        "—".to_string()
    } else {
        let mut s = meta
            .files_touched
            .iter()
            .take(10)
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ");
        if meta.files_touched.len() > 10 {
            s.push_str(&format!(" _(+{} more)_", meta.files_touched.len() - 10));
        }
        s
    };
    out.push(format!("- **Files touched:** {files}"));
    let mut activity = format!("{} exchange(s)", meta.exchanges);
    for (tool, label) in [
        ("Bash", "command(s)"),
        ("Edit", "edit(s)"),
        ("Write", "file(s) written"),
    ] {
        if let Some(n) = meta.tools.get(tool) {
            activity.push_str(&format!(" · {n} {label}"));
        }
    }
    out.push(format!("- **Activity:** {activity}"));
    if prose.is_empty() && !meta.last_assistant.is_empty() {
        out.push(format!(
            "- **Last step:** {}",
            trim_to(&meta.last_assistant, 200)
        ));
    }
    out.push(String::new());
    out.push("---".into());
    out.push(String::new());
    out.push(BODY_MARKER.into());
    out.push(String::new());
    out.join("\n")
}

/// Render a batch of events as conversation markdown. Consecutive assistant
/// texts within the batch merge into one block. `detail` ∈
/// conversation|tools|full adds tool calls/results/thinking progressively.
pub fn conversation(events: &[Event], detail: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut pending_ts: Option<String> = None;

    fn flush_into(lines: &mut Vec<String>, pending: &mut Vec<String>, ts: &mut Option<String>) {
        if !pending.is_empty() {
            lines.push(format!("## 🤖 Claude · {}\n", fmt_time(ts.as_deref())));
            lines.push(format!(
                "{}\n",
                demote_headings(pending.join("\n\n").trim())
            ));
            pending.clear();
            *ts = None;
        }
    }
    macro_rules! flush {
        () => {
            flush_into(&mut lines, &mut pending, &mut pending_ts)
        };
    }

    for ev in events {
        if ev.kind != "user" && ev.kind != "assistant" {
            continue;
        }
        let Some(msg) = &ev.message else { continue };

        if msg.role == "user" {
            if ev.is_typed_user() {
                flush!();
                let body = text_of(&msg.content);
                if !body.is_empty() {
                    lines.push(format!(
                        "## 🧑 User · {}\n",
                        fmt_time(ev.timestamp.as_deref())
                    ));
                    lines.push(format!("{}\n", demote_headings(&body)));
                }
            } else if matches!(detail, "tools" | "full")
                && let Some(blocks) = msg.content.as_array()
            {
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let out = match b.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => String::new(),
                    };
                    let out = out.trim();
                    if out.is_empty() {
                        continue;
                    }
                    flush!();
                    let snip = clip(out, 2000);
                    lines.push(format!(
                            "<details><summary>tool result</summary>\n\n```\n{snip}\n```\n\n</details>\n"
                        ));
                }
            }
            continue;
        }

        // assistant
        let said = text_of(&msg.content);
        if !said.is_empty() {
            pending_ts.get_or_insert_with(|| ev.timestamp.clone().unwrap_or_default());
            pending.push(said);
        }
        if let Some(blocks) = msg.content.as_array() {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("thinking") if detail == "full" => {
                        let think = b
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if !think.is_empty() {
                            flush!();
                            lines.push(format!(
                                "<details><summary>💭 thinking</summary>\n\n{think}\n\n</details>\n"
                            ));
                        }
                    }
                    Some("tool_use") if matches!(detail, "tools" | "full") => {
                        flush!();
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                        let inp =
                            serde_json::to_string_pretty(b.get("input").unwrap_or(&Value::Null))
                                .unwrap_or_default();
                        let inp = clip(&inp, 1500);
                        lines.push(format!(
                            "<details><summary>🔧 tool call: <code>{name}</code></summary>\n\n```json\n{inp}\n```\n\n</details>\n"
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    flush!();
    lines.join("\n")
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…(truncated)", &s[..cut])
}

/// One row of the per-project index cache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct IndexRow {
    pub file: String,
    pub started: String,
    pub title: String,
    pub summary: String,
    pub files_touched: Vec<String>,
}

pub fn index_markdown(project: &str, rows: &[IndexRow]) -> String {
    let mut rows: Vec<&IndexRow> = rows.iter().collect();
    rows.sort_by(|a, b| b.started.cmp(&a.started));
    let mut out = vec![
        format!("# 🗂 Session history — {project}"),
        String::new(),
        format!(
            "_{} session(s). Scan this index first; open a transcript only if you need the detail._",
            rows.len()
        ),
        String::new(),
        "| Date | Title | Summary | Files touched |".into(),
        "|------|-------|---------|---------------|".into(),
    ];
    for r in rows {
        let date: String = r.started.chars().take(10).collect();
        let files = if r.files_touched.is_empty() {
            "—".to_string()
        } else {
            esc_cell(
                &r.files_touched
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        out.push(format!(
            "| {date} | [{}]({}) | {} | {files} |",
            esc_cell(&r.title),
            r.file,
            esc_cell(&trim_to(&r.summary, 110)),
        ));
    }
    out.push(String::new());
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demote_skips_fences() {
        let md = "# top\n```\n# not a heading\n```\n## sub";
        let out = demote_headings(md);
        assert!(out.contains("## top"));
        assert!(out.contains("\n# not a heading\n"));
        assert!(out.contains("### sub"));
    }

    #[test]
    fn trim_counts_chars() {
        assert_eq!(trim_to("hola  mundo", 100), "hola mundo");
        assert!(trim_to("aaaaaaaaaa", 5).ends_with('…'));
    }

    #[test]
    fn header_holds_exactly_one_body_marker() {
        // A session *about* the marker puts a copy of it in the first user
        // message; the header must still contain only the real one, or the
        // incremental append splits the file in the wrong place.
        let meta = Meta {
            first_user: format!("explain how {BODY_MARKER} is parsed"),
            last_assistant: format!("the {BODY_MARKER} marker separates them"),
            title: Some(format!("About {BODY_MARKER}")),
            ..Default::default()
        };
        let h = header(&meta, "");
        assert_eq!(h.matches(BODY_MARKER).count(), 1, "header:\n{h}");

        let h = header(&meta, &format!("Summary mentioning {BODY_MARKER} too."));
        assert_eq!(h.matches(BODY_MARKER).count(), 1, "header:\n{h}");
    }
}
