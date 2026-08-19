//! One headless `claude -p` call per session producing BOTH the narrative
//! summary and the CLAUDE.md rule candidates (the template paid for two).
//! Best-effort: any failure leaves the deterministic export intact.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::config::Llm;
use crate::paths;

const PROMPT: &str = r#"You are analyzing the Claude Code session transcript below. Produce ONLY a JSON object (no prose, no code fences) with exactly two keys:

"summary": 3-5 sentences of plain prose for a developer browsing their project history weeks later. Cover the goal, what was actually built or changed (name specific files and key decisions), and any open questions or next steps. If the session is trivial, one sentence is fine.

"rules": an array of durable, forward-looking rules a FUTURE Claude session should follow — rules it would NOT already know from reading this project's code, README, or git history. The test for every candidate: "Would a future agent behave differently because of this line, in a way the existing code/docs don't already make obvious?" If not, DROP it.
INCLUDE only: explicit USER preferences or instructions ('I prefer X', 'always/never Y'), especially corrections the user gave; conventions or guardrails the user wants ENFORCED going forward; stable facts a new agent must know that are genuinely NOT obvious from the repo.
EXCLUDE: descriptions of how the code works or what was built this session; one-off task details or debugging narratives; anything already enforced by the code or written in README/CLAUDE.md; anything you are not confident is a deliberate, reusable instruction.
Prefer rules the USER explicitly stated; the evidence quote MUST be the user's own words. Be very conservative — an empty array is far better than a rule that restates the implementation.
Each element: {"text": "imperative one-line rule", "category": "preference|convention|constraint|fact", "confidence": "low|medium|high", "evidence": "short quote in the USER's own words"}."#;

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Analysis {
    pub summary: String,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(default)]
pub struct Rule {
    pub text: String,
    pub category: String,
    pub confidence: String,
    pub evidence: String,
}

pub fn analyze(convo: &str, cfg: &Llm) -> Result<Option<Analysis>> {
    if convo.trim().is_empty() {
        return Ok(None);
    }
    let out = run_claude(&format!("{PROMPT}\n\n=== TRANSCRIPT ===\n{convo}"), cfg)?;
    Ok(out.as_deref().and_then(parse_analysis))
}

/// Spawn `claude -p` and return its stdout, or None on any failure.
///
/// Three properties matter here:
///   - `CENV_LOCK` in the child's environment makes its own hooks short-circuit,
///     so summarizing a session cannot recursively summarize itself.
///   - `--tools ""` gives the child no tools at all. Its stdin is transcript
///     text, which can contain anything a third party once put in front of the
///     agent; a summarizer that cannot act cannot be talked into acting.
///   - Nothing here can block forever: stdout is drained on its own thread and
///     collected through a channel with the same deadline as the child, because
///     killing `claude` does not necessarily close a pipe its own children hold.
fn run_claude(prompt: &str, cfg: &Llm) -> Result<Option<String>> {
    let mut child = match Command::new("claude")
        .args([
            "-p",
            "--no-session-persistence",
            "--tools",
            "",
            "--model",
            &cfg.model,
        ])
        .env(paths::LOCK_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Ok(None), // no claude CLI — degrade silently
    };

    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    // Write after the drain thread is live: a prompt larger than the pipe
    // buffer would otherwise block before anything is reading the other end.
    let mut stdin = child.stdin.take().unwrap();
    if stdin.write_all(prompt.as_bytes()).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    }
    drop(stdin);

    let deadline = std::time::Instant::now() + Duration::from_secs(cfg.timeout_secs);
    let ok = loop {
        match child.try_wait()? {
            Some(status) => break status.success(),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let out = rx
        .recv_timeout(remaining.max(Duration::from_secs(2)))
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok((ok && !out.is_empty()).then_some(out))
}

/// Tolerant parse: find the outermost JSON object even if the model wrapped it
/// in prose or fences.
fn parse_analysis(text: &str) -> Option<Analysis> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let a: Analysis = serde_json::from_str(&text[start..=end]).ok()?;
    if a.summary.trim().is_empty() && a.rules.is_empty() {
        return None;
    }
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n{\"summary\": \"Did things.\", \"rules\": [{\"text\": \"Use uv\", \"category\": \"preference\", \"confidence\": \"high\", \"evidence\": \"always use uv\"}]}\n```";
        let a = parse_analysis(raw).unwrap();
        assert_eq!(a.summary, "Did things.");
        assert_eq!(a.rules.len(), 1);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_analysis("no json here").is_none());
        assert!(parse_analysis("{}").is_none());
    }
}
