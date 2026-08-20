# cenv

A single-binary companion for [Claude Code](https://claude.com/claude-code):

- **Automatic session capture** — every conversation is exported to readable
  Markdown as you work, incrementally (only new transcript lines are parsed on
  each hook), with a per-project `INDEX.md` that future sessions and agents can
  scan cheaply.
- **One-call session intelligence** — on session end, a single headless
  `claude -p` call produces both a narrative summary *and* candidate CLAUDE.md
  rules distilled from what you told the agent. Candidates go to a staging file
  you review with checkboxes; nothing is applied unattended.
- **Portable environment sync (optional)** — keep `settings.json`, your global
  `CLAUDE.md`, commands, and whitelisted project memory in a **private** git
  repo, synced across machines with a fail-closed [gitleaks](https://github.com/gitleaks/gitleaks)
  gate: no scanner, no push; flagged secret, no push.

No Python, no runtime dependencies. Hooks run a compiled binary that starts in
milliseconds, so capture costs nothing you can feel — an 8.5 MB transcript
exports in ~0.2 s, and the incremental path does far less than that per turn.

## Quickstart (capture only)

```bash
cargo install --git https://github.com/cmacha2/cenv
cenv enable-hooks     # merges capture hooks into ~/.claude/settings.json
cenv doctor           # verify the wiring
```

Open a new Claude Code session — from now on every session is exported to
`~/.local/share/cenv/history/<project>/`, and each new session is automatically
pointed at that project's history index via hook context injection.

`cenv enable-hooks` **merges** into your existing `settings.json` (a timestamped
backup is written first) and is idempotent; `cenv enable-hooks --remove` takes
the hooks back out, leaving everything else untouched.

## Quickstart (synced environment)

```bash
cenv init             # scaffolds a private env repo at ~/.claude-env
cenv install          # symlinks its claude/ config into ~/.claude
# create a PRIVATE remote for ~/.claude-env, then:
cenv sync             # pull → mirror memory → gitleaks scan → commit → push
```

On another machine: clone your repo to `~/.claude-env`, run `cenv install`,
then `claude login` (credentials never sync, by design).

## Commands

| Command | What it does |
|---|---|
| `cenv enable-hooks [--remove]` | Merge (or strip) capture hooks in `~/.claude/settings.json` |
| `cenv doctor [--quiet]` | Verify capture wiring; `--quiet` prints only problems (runs at every session start) |
| `cenv export [FILE...]` | Export the current project's sessions (or specific `.jsonl` files) to Markdown |
| `cenv export --reindex` | Rebuild the project index from export frontmatter |
| `cenv analyze [--all] [--limit N]` | Run the summary + distillation pass for any session still pending |
| `cenv adopt [--central]` | Keep this project's history in `<project>/history/` instead of the central store |
| `cenv distill apply [--path DIR] [--global --confirm]` | Apply checked `[x]` rule candidates into the matching CLAUDE.md |
| `cenv distill scan-global` | Cluster candidates across projects; recurring ones become global candidates |
| `cenv sync [--full-scan]` | Env-repo sync with the gitleaks gate |
| `cenv backfill [--export] [--analyze]` | Export ALL existing transcripts into a browsable archive (dry-run by default) |
| `cenv init` / `install` / `uninstall` | Env-repo scaffolding and symlink management |

## Where things live

| Path | Contents |
|---|---|
| `~/.local/share/cenv/history/<project>/` | Markdown exports + `INDEX.md` + rule-candidate staging |
| `~/.local/state/cenv/sessions/` | Per-session incremental state (byte offsets, cached metadata) |
| `~/.claude-env/` | Your private env repo (optional) |
| `~/.local/share/cenv/archive/` | Backfill output |

Every location is overridable via `CENV_HOME`, `CENV_CLAUDE_DIR`,
`CENV_PROJECTS_DIR`, `CENV_DATA_DIR`, `CENV_STATE_DIR`, `CENV_CONFIG_DIR`,
`CENV_REPO` — which is also how the test suite sandboxes itself.

## Design notes

cenv is a ground-up Rust rewrite of the ideas in
[pugas-fm/claude-env-template](https://github.com/pugas-fm/claude-env-template)
(bash + Python), keeping what that design got right — fail-closed secret
scanning, install guards against silent capture loss, human-reviewed rule
staging — and fixing what a code review of it surfaced:

- **Your project files are never touched.** The template appended pointers to
  each project's `CLAUDE.md` and `.gitignore` on every capture — dirty diffs in
  shared repos. cenv writes history to a central store and injects the pointer
  through hook output instead. In-repo history is available per project via
  explicit `cenv adopt`.
- **Capture is incremental.** The template re-parsed and re-rendered the whole
  transcript, and re-read every past export, on every Stop hook. cenv stores a
  byte offset per session and appends.
- **No wrong-session fallback.** The template fell back to "the newest
  transcript of *any* project" when hook input was missing — with two sessions
  open in parallel it could export the wrong project's conversation. cenv never
  guesses: no transcript path, no export. Manual exports are scoped to the
  current project.
- **One LLM call, not two.** Summary and rule extraction share a single
  `claude -p` call, and trivial sessions (fewer than 2 exchanges / under 400
  chars) skip it entirely.
- **Memory sync is a mirror, carefully.** Deleted memory files are deleted from
  the repo too, instead of resurrecting on every sync — but the delete pass
  never follows a symlink (so it cannot reach outside the repo), never runs on a
  listing that hit a read error, and refuses a mass delete when the source
  directory has gone empty.
- **The summarizer has no tools.** The headless child runs with `--tools ""`.
  Its input is transcript text, which can contain anything a third party once
  put in front of the agent; a summarizer that cannot act cannot be talked into
  acting.
- **The secret scan doesn't brick.** By default gitleaks scans only commits not
  yet on the upstream, so one historical leak doesn't fail every future sync;
  `cenv sync --full-scan` audits everything.
- **Pulling config doesn't execute code.** Hooks invoke this locally-compiled
  binary, not scripts from the synced repo — a `git pull` in your env repo
  changes data, and code only changes when you rebuild.
- **Schema drift fails loudly, not silently.** Transcript parsing is
  defaults-and-heuristics (no single undocumented field decides what a "typed
  user turn" is), and the behavior is pinned by tests.
- **Hooks are wired with an absolute path.** Hook commands run under a
  non-interactive shell that sources no profile, so `~/.cargo/bin` is not on its
  PATH and a bare `cenv` would fail with "command not found" on every stop.
  `enable-hooks` writes the resolved path of the binary you ran, re-running
  repairs a stale path in place, and `doctor` reports a hook command the hook
  shell could not resolve.
- **No slow work inside a hook.** SessionEnd is the one hook that may call a
  model, and it fires while the host is tearing the session down — so the host
  stops waiting and cancels it, and inline the summary only ever landed when the
  hook happened to win that race. Raising the timeout doesn't fix a deadline that
  isn't yours. So the hook doesn't do the work: it re-invokes cenv in its own
  process group with no inherited stdio, returns in milliseconds, and the
  analysis completes after the session is gone. If that worker is killed too, the
  next session you start sweeps the backlog in the background, and `cenv analyze`
  is there to run it by hand.

### Debugging

Hooks stay silent so they can never break a session, which also hides genuine
problems. `CENV_DEBUG=1` makes them narrate what they decided and why:

```bash
CENV_DEBUG=1 claude    # hook decisions appear on stderr
cenv doctor            # or check the wiring directly
```
- **Your own config survives.** `enable-hooks` merges into `settings.json`
  instead of replacing it, backs it up first, writes atomically, removes only
  the exact commands cenv itself wrote, and — if `install` ever displaces a
  symlink you created — records where it pointed so `uninstall` puts it back.

## Configuration

`config.toml` (synced, in the env repo — or `~/.config/cenv/` without one):

```toml
[capture]
detail = "conversation"   # conversation | tools | full

[llm]
model = "haiku"           # model for summaries/distillation
timeout_secs = 120
min_exchanges = 2         # skip the LLM pass below these thresholds
min_chars = 400

[sync]
scan = "range"            # range | full
```

`config.local.toml` (machine-local, gitignored):

```toml
[[memory]]                # project memory mirrored into the env repo on sync
name = "my-project"
path = "/abs/path/to/.claude/projects/<enc>/memory"
```

## Security model

- **Credentials never sync.** `claude login` per machine.
- **Transcripts never sync.** They are plaintext and can contain secrets; only
  the curated, whitelisted memory directories you list in `config.local.toml`
  leave the machine, and only through the gitleaks gate.
- **The gate is fail-closed** but not omniscient: gitleaks catches secret
  *patterns*, not sensitive *prose*. Whitelist accordingly, and keep the env
  repo private.

## License

MIT
