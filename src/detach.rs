//! Running slow work outside the hook that asked for it.
//!
//! A hook is not a place to spend ten seconds. `SessionEnd` fires while the host
//! is tearing the session down, and it will stop waiting — observed in practice
//! as `SessionEnd hook failed: Hook cancelled`, with the summary silently never
//! written. Raising the hook's timeout does not fix it, because the deadline is
//! not really the hook's; it belongs to a process that is on its way out.
//!
//! So the hook does not do the work. It re-invokes this same binary in a process
//! that deliberately outlives it — its own process group, so a group-wide kill
//! or a terminal hangup misses it, and no inherited stdio, so nothing upstream
//! waits on a pipe it holds. The hook then returns in milliseconds.

use std::process::{Command, Stdio};

/// Marker for a process we detached ourselves. It is not a lock — it just lets
/// the worker's own logging say where it came from.
pub const WORKER_ENV: &str = "CENV_WORKER";

pub fn is_worker() -> bool {
    std::env::var_os(WORKER_ENV).is_some()
}

/// Re-invoke this binary with `args`, detached from the current process group
/// and from all three standard streams. Returns whether the spawn succeeded;
/// the child's own outcome is deliberately not awaited.
pub fn spawn_worker(args: &[&str]) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env(WORKER_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // A new process group is what actually buys survival: the host can kill the
    // hook's group without touching ours, and SIGHUP on terminal close is sent
    // to the terminal's foreground group, which we are no longer in.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }

    // Deliberately not waited on. The child is reparented when we exit, which is
    // immediately — that is the whole point, not a leak.
    cmd.spawn().is_ok()
}
