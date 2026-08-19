//! Tolerant parsing of Claude Code transcript `.jsonl` files.
//!
//! The transcript schema is undocumented and drifts between Claude Code
//! releases, so everything here is defaults-and-heuristics: unknown fields are
//! ignored, missing fields degrade gracefully, and "is this a message the user
//! actually typed?" is decided by shape, not by any single unstable field.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub timestamp: Option<String>,
    #[serde(rename = "aiTitle")]
    pub ai_title: Option<String>,
    #[serde(rename = "promptSource")]
    pub prompt_source: Option<Value>,
    #[serde(rename = "isMeta")]
    pub is_meta: bool,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Message {
    pub role: String,
    pub content: Value,
}

/// Read complete lines starting at `offset`; returns events plus the offset
/// after the last complete line (a partial trailing line is left for next time).
pub fn read_events_from(path: &Path, offset: u64) -> anyhow::Result<(Vec<Event>, u64)> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(f);
    let mut events = Vec::new();
    let mut consumed = offset;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if !line.ends_with('\n') {
            break; // partial write in progress — pick it up next run
        }
        consumed += n as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<Event>(trimmed) {
            events.push(ev);
        }
    }
    Ok((events, consumed))
}

/// All events plus the byte offset actually consumed — the offset must come
/// from the reader, never from the file length, or a partially-written trailing
/// line gets skipped and every later incremental read starts mid-line.
pub fn read_all_events_with_offset(path: &Path) -> anyhow::Result<(Vec<Event>, u64)> {
    read_events_from(path, 0)
}

pub fn read_all_events(path: &Path) -> anyhow::Result<Vec<Event>> {
    Ok(read_events_from(path, 0)?.0)
}

/// Concatenated text blocks of a message content value.
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                (b.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| b.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn has_block(content: &Value, kind: &str) -> bool {
    content.as_array().is_some_and(|a| {
        a.iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some(kind))
    })
}

impl Event {
    /// A turn the human actually typed — as opposed to tool results, meta
    /// events, or injected context. `promptSource` is the strong signal when
    /// present; the shape heuristic keeps capture alive if that field goes away.
    pub fn is_typed_user(&self) -> bool {
        if self.kind != "user" || self.is_meta {
            return false;
        }
        let Some(msg) = &self.message else {
            return false;
        };
        if is_command_text(&text_of(&msg.content)) {
            return false; // slash-command bookkeeping (/clear etc.), not a real turn
        }
        if self.prompt_source.is_some() {
            return true;
        }
        match &msg.content {
            Value::String(s) => !s.trim().is_empty(),
            c @ Value::Array(_) => !has_block(c, "tool_result") && !text_of(c).is_empty(),
            _ => false,
        }
    }
}

/// Local-command bookkeeping that Claude Code injects as user messages
/// (`<command-name>/clear</command-name>…`) — noise, not conversation.
pub fn is_command_text(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with("<command-name>")
        || t.starts_with("<local-command")
        || t.starts_with("Caveat: The messages below were generated")
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Meta {
    pub session: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub started: Option<String>,
    pub updated: Option<String>,
    pub exchanges: usize,
    pub files_touched: Vec<String>,
    pub tools: BTreeMap<String, usize>,
    pub first_user: String,
    pub last_assistant: String,
}

impl Meta {
    /// Fold a batch of events into the running meta (incremental-friendly).
    pub fn absorb(&mut self, events: &[Event]) {
        for ev in events {
            if let Some(s) = &ev.session_id {
                self.session = Some(s.clone());
            }
            // First cwd wins. Later events report wherever the agent has since
            // cd'd to, and letting that through would move the session's whole
            // history to another store mid-session, orphaning what was already
            // written there.
            if let Some(c) = &ev.cwd {
                self.cwd.get_or_insert_with(|| c.clone());
            }
            if ev.kind == "ai-title"
                && let Some(t) = &ev.ai_title
            {
                self.title = Some(t.clone());
            }
            if let Some(ts) = &ev.timestamp {
                self.started.get_or_insert_with(|| ts.clone());
                self.updated = Some(ts.clone());
            }
            if ev.is_typed_user() {
                self.exchanges += 1;
                if self.first_user.is_empty()
                    && let Some(m) = &ev.message
                {
                    self.first_user = text_of(&m.content);
                }
            }
            if ev.kind == "assistant" {
                let Some(msg) = &ev.message else { continue };
                let said = text_of(&msg.content);
                if !said.is_empty() {
                    self.last_assistant = said;
                }
                let Some(blocks) = msg.content.as_array() else {
                    continue;
                };
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                    *self.tools.entry(name.to_string()).or_insert(0) += 1;
                    if matches!(name, "Edit" | "Write" | "NotebookEdit")
                        && let Some(fp) = b
                            .get("input")
                            .and_then(|i| i.get("file_path"))
                            .and_then(Value::as_str)
                        && !self.files_touched.iter().any(|f| f == fp)
                    {
                        self.files_touched.push(fp.to_string());
                    }
                }
            }
        }
    }
}

/// Compact USER/CLAUDE text — the summarizer's input.
pub fn plain_conversation(events: &[Event], cap: usize) -> String {
    let mut out = Vec::new();
    for ev in events {
        let Some(msg) = &ev.message else { continue };
        if ev.is_typed_user() {
            let t = text_of(&msg.content);
            if !t.is_empty() {
                out.push(format!("USER: {t}"));
            }
        } else if ev.kind == "assistant" {
            let t = text_of(&msg.content);
            if !t.is_empty() {
                out.push(format!("CLAUDE: {t}"));
            }
        }
    }
    let mut text = out.join("\n\n");
    if text.len() > cap {
        let mut cut = cap;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(json: &str) -> Event {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn typed_user_detection() {
        assert!(ev(r#"{"type":"user","promptSource":"terminal","message":{"role":"user","content":"hola"}}"#).is_typed_user());
        // no promptSource, plain string content → still a typed turn (schema drift)
        assert!(
            ev(r#"{"type":"user","message":{"role":"user","content":"hola"}}"#).is_typed_user()
        );
        // tool_result payloads are not typed turns
        assert!(!ev(r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#).is_typed_user());
        assert!(
            !ev(r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"ctx"}}"#)
                .is_typed_user()
        );
    }

    #[test]
    fn partial_trailing_line_is_deferred() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"a\"}}\n{\"type\":\"assist").unwrap();
        let (events, off) = read_events_from(&p, 0).unwrap();
        assert_eq!(events.len(), 1);
        // completing the line later resumes exactly where we stopped
        let full = std::fs::read_to_string(&p).unwrap();
        std::fs::write(
            &p,
            format!("{full}ant\",\"message\":{{\"role\":\"assistant\",\"content\":\"b\"}}}}\n"),
        )
        .unwrap();
        let (more, _) = read_events_from(&p, off).unwrap();
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].kind, "assistant");
    }

    #[test]
    fn cwd_stays_at_the_session_origin() {
        // The agent cd'ing during a session must not move its history store.
        let events = vec![
            ev(
                r#"{"type":"user","sessionId":"s","cwd":"/work/origin","promptSource":"t","message":{"role":"user","content":"hi"}}"#,
            ),
            ev(
                r#"{"type":"assistant","cwd":"/work/origin/subdir","message":{"role":"assistant","content":"ok"}}"#,
            ),
        ];
        let mut m = Meta::default();
        m.absorb(&events);
        assert_eq!(m.cwd.as_deref(), Some("/work/origin"));

        // Also across incremental batches.
        let mut m = Meta::default();
        m.absorb(&events[..1]);
        m.absorb(&events[1..]);
        assert_eq!(m.cwd.as_deref(), Some("/work/origin"));
    }

    #[test]
    fn meta_absorb_counts() {
        let events = vec![
            ev(
                r#"{"type":"user","sessionId":"s1","cwd":"/p/demo","timestamp":"2026-01-01T10:00:00Z","promptSource":"t","message":{"role":"user","content":"fix the bug"}}"#,
            ),
            ev(
                r#"{"type":"assistant","timestamp":"2026-01-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"done"},{"type":"tool_use","name":"Edit","input":{"file_path":"/p/demo/a.rs"}}]}}"#,
            ),
        ];
        let mut m = Meta::default();
        m.absorb(&events);
        assert_eq!(m.exchanges, 1);
        assert_eq!(m.files_touched, vec!["/p/demo/a.rs"]);
        assert_eq!(m.tools.get("Edit"), Some(&1));
        assert_eq!(m.first_user, "fix the bug");
    }
}
