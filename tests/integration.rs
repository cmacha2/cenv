//! End-to-end flow against the real binary, fully redirected into a temp
//! sandbox via CENV_* env overrides — the user's actual ~/.claude is never
//! touched by the test suite.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for d in ["claude/projects", "data", "state", "config"] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        Self { root }
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.root.path().join(rel)
    }

    fn cenv(&self, args: &[&str], stdin: Option<&str>) -> (bool, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cenv"));
        cmd.args(args)
            .env("CENV_HOME", self.root.path())
            .env("CENV_CLAUDE_DIR", self.path("claude"))
            .env("CENV_PROJECTS_DIR", self.path("claude/projects"))
            .env("CENV_DATA_DIR", self.path("data"))
            .env("CENV_STATE_DIR", self.path("state"))
            .env("CENV_CONFIG_DIR", self.path("config"))
            .env("CENV_REPO", self.path("env-repo"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if let Some(s) = stdin {
            child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
        } else {
            drop(child.stdin.take());
        }
        let out = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }
}

fn line(kind: &str, extra: &str) -> String {
    format!(
        r#"{{"type":"{kind}","sessionId":"itest-session-0001","cwd":"/work/demo-app","timestamp":"2026-08-19T10:00:00Z",{extra}}}"#
    )
}

fn write_transcript(path: &Path, lines: &[String]) {
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn hook_capture_is_incremental_and_scoped() {
    let sb = Sandbox::new();
    let tdir = sb.path("claude/projects/-work-demo-app");
    fs::create_dir_all(&tdir).unwrap();
    let transcript = tdir.join("itest-session-0001.jsonl");

    write_transcript(
        &transcript,
        &[
            line(
                "user",
                r#""promptSource":"terminal","message":{"role":"user","content":"arregla el bug de login"}"#,
            ),
            line(
                "assistant",
                r#""message":{"role":"assistant","content":[{"type":"text","text":"Voy a mirarlo."},{"type":"tool_use","name":"Edit","input":{"file_path":"/work/demo-app/auth.rs"}}]}"#,
            ),
        ],
    );

    let hook_json = format!(
        r#"{{"session_id":"itest-session-0001","transcript_path":"{}","cwd":"/work/demo-app"}}"#,
        transcript.display()
    );

    let (ok, _) = sb.cenv(&["hook", "stop"], Some(&hook_json));
    assert!(ok);

    let store = sb.path("data/history/demo-app");
    let exports: Vec<_> = fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .filter(|p| p.file_name().is_some_and(|n| n != "INDEX.md"))
        .collect();
    assert_eq!(exports.len(), 1, "one export for one session");
    let md = fs::read_to_string(&exports[0]).unwrap();
    assert!(md.contains("arregla el bug de login"));
    assert!(md.contains("Voy a mirarlo."));
    assert!(
        md.contains("`/work/demo-app/auth.rs`"),
        "files touched in header:\n{md}"
    );
    assert!(
        fs::read_to_string(store.join("INDEX.md"))
            .unwrap()
            .contains("demo-app")
    );

    // Second stop with only new lines appended → incremental, no duplication.
    let mut all = fs::read_to_string(&transcript).unwrap();
    all.push_str(&line(
        "user",
        r#""promptSource":"terminal","message":{"role":"user","content":"ahora añade tests"}"#,
    ));
    all.push('\n');
    fs::write(&transcript, all).unwrap();

    let count_before = fs::read_to_string(&exports[0])
        .unwrap()
        .matches("arregla el bug de login")
        .count();
    let (ok, _) = sb.cenv(&["hook", "stop"], Some(&hook_json));
    assert!(ok);
    let md = fs::read_to_string(&exports[0]).unwrap();
    assert_eq!(
        md.matches("## 🧑 User").count(),
        2,
        "both user turns present:\n{md}"
    );
    assert_eq!(
        md.matches("arregla el bug de login").count(),
        count_before,
        "first turn not re-rendered into the body"
    );
    assert!(md.contains("ahora añade tests"));

    // A hook payload with no transcript_path must do nothing (never guess).
    let before = fs::read_to_string(&exports[0]).unwrap();
    let (ok, out) = sb.cenv(&["hook", "stop"], Some(r#"{"session_id":"x"}"#));
    assert!(ok && out.is_empty());
    assert_eq!(before, fs::read_to_string(&exports[0]).unwrap());
}

#[test]
fn enable_hooks_is_idempotent_and_reversible() {
    let sb = Sandbox::new();
    let settings = sb.path("claude/settings.json");
    fs::write(&settings, r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo mine"}]}]}}"#).unwrap();

    let (ok, _) = sb.cenv(&["enable-hooks"], None);
    assert!(ok);
    let (ok, _) = sb.cenv(&["enable-hooks"], None);
    assert!(ok);

    let content = fs::read_to_string(&settings).unwrap();
    assert_eq!(
        content.matches("cenv hook stop").count(),
        1,
        "idempotent:\n{content}"
    );
    assert!(
        content.contains("echo mine"),
        "pre-existing hooks preserved"
    );
    assert!(
        content.contains(r#""theme": "dark""#),
        "other settings preserved"
    );

    // doctor healthy now
    let (ok, out) = sb.cenv(&["doctor", "--quiet"], None);
    assert!(ok, "doctor should pass: {out}");

    let (ok, _) = sb.cenv(&["enable-hooks", "--remove"], None);
    assert!(ok);
    let content = fs::read_to_string(&settings).unwrap();
    assert!(!content.contains("cenv hook"));
    assert!(content.contains("echo mine"));
}

#[test]
fn init_install_doctor_roundtrip() {
    let sb = Sandbox::new();
    let (ok, out) = sb.cenv(&["init"], None);
    assert!(ok, "{out}");
    assert!(sb.path("env-repo/claude/settings.json").exists());

    let (ok, out) = sb.cenv(&["install"], None);
    assert!(ok, "install failed: {out}");
    let settings = sb.path("claude/settings.json");
    assert!(settings.is_symlink());
    let (ok, out) = sb.cenv(&["doctor"], None);
    assert!(ok, "doctor after install: {out}");

    let (ok, out) = sb.cenv(&["uninstall"], None);
    assert!(ok, "{out}");
    assert!(!settings.exists());
}

#[test]
fn install_refuses_temp_repo_outside_home() {
    let sb = Sandbox::new();
    // Simulate the real incident: the repo lives in a scratch location that is
    // NOT under the (fake) home, so the temp-path guard must fire.
    let outside = tempfile::tempdir().unwrap();
    let fake_home = sb.path("realhome");
    fs::create_dir_all(&fake_home).unwrap();

    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_cenv"))
            .args(args)
            .env("CENV_HOME", &fake_home)
            .env("CENV_CLAUDE_DIR", sb.path("claude"))
            .env("CENV_REPO", outside.path().join("repo"))
            .output()
            .unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    let (ok, out) = run(&["init"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["install"]);
    assert!(
        !ok && out.contains("temporary path"),
        "guard should fire: {out}"
    );
}

#[test]
fn manual_export_requires_project_scope() {
    let sb = Sandbox::new();
    // no transcripts at all → export from an unrelated cwd finds nothing and says so
    let (ok, out) = sb.cenv(&["export"], None);
    assert!(ok);
    assert!(out.contains("No transcripts found"), "{out}");
}

#[test]
fn manual_export_of_a_live_transcript_does_not_lose_the_next_turn() {
    // `cenv export` on a session that is mid-write (transcript ends with a
    // half-flushed line) must record only the bytes it actually consumed.
    // Recording the file length instead would make the next hook seek into the
    // middle of that line and drop the turn for good.
    let sb = Sandbox::new();
    let tdir = sb.path("claude/projects/-work-demo-app");
    fs::create_dir_all(&tdir).unwrap();
    let transcript = tdir.join("live-session.jsonl");

    let complete = line(
        "user",
        r#""promptSource":"terminal","message":{"role":"user","content":"primera pregunta"}"#,
    );
    let partial =
        r#"{"type":"assistant","sessionId":"itest-session-0001","message":{"role":"assist"#;
    fs::write(&transcript, format!("{complete}\n{partial}")).unwrap();

    let (ok, out) = sb.cenv(&["export", transcript.to_str().unwrap()], None);
    assert!(ok, "{out}");

    // The session finishes writing that line.
    fs::write(
        &transcript,
        format!(
            "{complete}\n{}\n",
            line(
                "assistant",
                r#""message":{"role":"assistant","content":[{"type":"text","text":"respuesta completa"}]}"#
            )
        ),
    )
    .unwrap();

    let hook_json = format!(
        r#"{{"session_id":"itest-session-0001","transcript_path":"{}","cwd":"/work/demo-app"}}"#,
        transcript.display()
    );
    let (ok, _) = sb.cenv(&["hook", "stop"], Some(&hook_json));
    assert!(ok);

    let store = sb.path("data/history/demo-app");
    let md = fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .filter(|p| p.file_name().is_some_and(|n| n != "INDEX.md"))
        .map(|p| fs::read_to_string(p).unwrap())
        .collect::<String>();
    assert!(
        md.contains("respuesta completa"),
        "the completed turn must appear:\n{md}"
    );
}

#[test]
fn deleting_the_export_rebuilds_it_in_full() {
    let sb = Sandbox::new();
    let tdir = sb.path("claude/projects/-work-demo-app");
    fs::create_dir_all(&tdir).unwrap();
    let transcript = tdir.join("s.jsonl");
    write_transcript(
        &transcript,
        &[line(
            "user",
            r#""promptSource":"terminal","message":{"role":"user","content":"turno uno"}"#,
        )],
    );
    let hook_json = format!(
        r#"{{"session_id":"itest-session-0001","transcript_path":"{}","cwd":"/work/demo-app"}}"#,
        transcript.display()
    );
    sb.cenv(&["hook", "stop"], Some(&hook_json));

    let store = sb.path("data/history/demo-app");
    let export = fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension().is_some_and(|e| e == "md")
                && p.file_name().is_some_and(|n| n != "INDEX.md")
        })
        .unwrap();

    // User deletes the markdown, then the session continues.
    fs::remove_file(&export).unwrap();
    let mut all = fs::read_to_string(&transcript).unwrap();
    all.push_str(&line(
        "user",
        r#""promptSource":"terminal","message":{"role":"user","content":"turno dos"}"#,
    ));
    all.push('\n');
    fs::write(&transcript, all).unwrap();
    sb.cenv(&["hook", "stop"], Some(&hook_json));

    let md = fs::read_to_string(&export).unwrap();
    assert!(md.contains("turno uno"), "earlier turn recovered:\n{md}");
    assert!(md.contains("turno dos"));
}

#[test]
fn same_named_projects_get_separate_stores() {
    let sb = Sandbox::new();
    for (dir, cwd, text) in [
        ("-a-api", "/a/api", "proyecto a"),
        ("-b-api", "/b/api", "proyecto b"),
    ] {
        let tdir = sb.path(&format!("claude/projects/{dir}"));
        fs::create_dir_all(&tdir).unwrap();
        let transcript = tdir.join("t.jsonl");
        fs::write(
            &transcript,
            format!(
                r#"{{"type":"user","sessionId":"sid-{dir}","cwd":"{cwd}","timestamp":"2026-08-19T10:00:00Z","promptSource":"t","message":{{"role":"user","content":"{text}"}}}}"#
            ) + "\n",
        )
        .unwrap();
        let hook = format!(
            r#"{{"session_id":"sid-{dir}","transcript_path":"{}","cwd":"{cwd}"}}"#,
            transcript.display()
        );
        let (ok, _) = sb.cenv(&["hook", "stop"], Some(&hook));
        assert!(ok);
    }

    let stores: Vec<_> = fs::read_dir(sb.path("data/history"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        stores.len(),
        2,
        "two projects named api must not share a store: {stores:?}"
    );
}

#[test]
fn doctor_accepts_hooks_in_settings_local() {
    let sb = Sandbox::new();
    // Hooks configured only in settings.local.json are still live — doctor must
    // not report capture as broken.
    fs::write(sb.path("claude/settings.json"), r#"{"theme":"dark"}"#).unwrap();
    let (ok, out) = sb.cenv(&["doctor", "--quiet"], None);
    assert!(
        !ok && out.contains("no cenv hooks"),
        "should complain: {out}"
    );

    let bin = env!("CARGO_BIN_EXE_cenv");
    fs::write(
        sb.path("claude/settings.local.json"),
        format!(
            r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":"{bin} hook stop"}}]}}]}}}}"#
        ),
    )
    .unwrap();
    let (ok, out) = sb.cenv(&["doctor", "--quiet"], None);
    assert!(ok && out.is_empty(), "should be satisfied: {out}");
}

#[test]
fn hooks_are_wired_with_a_resolvable_absolute_path() {
    // The bug this pins: hooks run under a non-interactive `/bin/sh` that
    // sources no shell profile, so a bare `cenv` is not on PATH and every stop
    // fails with "command not found" — capture silently never happens.
    let sb = Sandbox::new();
    let (ok, _) = sb.cenv(&["enable-hooks"], None);
    assert!(ok);

    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.path("claude/settings.json")).unwrap())
            .unwrap();
    let cmd = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let bin = cmd.trim_end_matches(" hook stop").trim_matches('"');
    assert!(
        Path::new(bin).is_absolute() && Path::new(bin).is_file(),
        "hook must name an existing absolute binary, got: {cmd}"
    );

    // And it must actually run under a bare-PATH `sh`, the way a hook does.
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{cmd} < /dev/null"))
        .env("PATH", "/usr/bin:/bin")
        .env("CENV_DATA_DIR", sb.path("data"))
        .env("CENV_STATE_DIR", sb.path("state"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("not found"),
        "hook command must resolve under a bare PATH: {stderr}"
    );
    assert!(out.status.success(), "hook must exit 0: {stderr}");

    // Doctor flags a bare command that the hook shell could not resolve.
    fs::write(
        sb.path("claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"cenv hook stop"}]}]}}"#,
    )
    .unwrap();
    let (ok, out) = sb.cenv(&["doctor", "--quiet"], None);
    assert!(
        !ok && out.contains("won't resolve in the hook shell"),
        "doctor should catch a bare command: {out}"
    );
}

#[test]
fn enable_hooks_replaces_a_stale_binary_path() {
    let sb = Sandbox::new();
    fs::write(
        sb.path("claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[
             {"type":"command","command":"/old/prefix/cenv hook stop"},
             {"type":"command","command":"cenv hook stop && notify-me"}
           ]}]}}"#,
    )
    .unwrap();
    let (ok, _) = sb.cenv(&["enable-hooks"], None);
    assert!(ok);

    let content = fs::read_to_string(sb.path("claude/settings.json")).unwrap();
    assert!(
        !content.contains("/old/prefix/cenv"),
        "stale path must be replaced, not stacked:\n{content}"
    );
    assert!(
        content.contains("cenv hook stop && notify-me"),
        "a user's own wrapper must survive:\n{content}"
    );
    assert_eq!(
        content.matches("hook stop").count(),
        2,
        "exactly our new entry plus the user's wrapper:\n{content}"
    );
}
