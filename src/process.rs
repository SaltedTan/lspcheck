// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Starting the server in a process group lspcheck owns, finding what it left
//! behind, and stopping it.
//!
//! On Unix the server joins the process group of a small *sentinel* process
//! (lspcheck itself, run with [`SENTINEL_ARG`]). The sentinel stays alive until
//! the session ends, so the group id can't be recycled for an unrelated group
//! while lspcheck is still looking for the server's descendants or signalling
//! them.
//!
//! On Linux the lspcheck binary also makes itself a child subreaper, so
//! descendants that leave the group with `setsid` are reparented to it when
//! their parent exits. lspcheck only treats such adopted children as the
//! server's when it is certain: the process enabled subreaping through
//! [`enable_subreaper`], exactly one session is running, and the child isn't
//! a process lspcheck started itself. Otherwise only the group is searched,
//! and [`Coverage`] says so.
//!
//! On Windows only the server process itself is tracked so far.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The argument that makes the lspcheck binary act as a sentinel: it reads
/// stdin until end of file, then exits.
pub const SENTINEL_ARG: &str = "__lspcheck-sentinel";

/// How to run the server.
#[derive(Debug, Clone)]
pub struct ServerCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

impl ServerCommand {
    /// A command from argv. A program given as a path with a directory part
    /// (`./server`, `bin/server`) is made absolute against the current
    /// directory, because the server runs in a temporary workspace; a bare name
    /// is still looked up on `PATH`.
    pub fn new(argv: &[OsString]) -> Self {
        let mut program = argv[0].clone();
        let path = std::path::Path::new(&program);
        if path.components().count() > 1
            && path.is_relative()
            && let Ok(abs) = std::path::absolute(path)
        {
            program = abs.into_os_string();
        }
        Self { program, args: argv[1..].to_vec(), cwd: None, env: Vec::new() }
    }

    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A process found in the server's group or among its escaped descendants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// The command name, decoded lossily for display.
    pub name: String,
}

impl std::fmt::Display for ProcInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid {} ({})", self.pid, self.name)
    }
}

/// What a search for leftover processes could see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The process group, and descendants that left it (Linux subreaper).
    GroupAndEscaped,
    /// The process group only: a descendant that called `setsid` is invisible.
    GroupOnly,
    /// Nothing: this platform isn't supported yet.
    None,
}

static SUBREAPER: AtomicBool = AtomicBool::new(false);
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
static SPAWNED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Makes this process a child subreaper where the OS supports it (Linux).
/// Call once, early, from a binary that starts no children except sessions.
pub fn enable_subreaper() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let ok = nix::sys::prctl::set_child_subreaper(true).is_ok();
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let ok = false;
    SUBREAPER.store(ok, Ordering::SeqCst);
    ok
}

/// What a leftover search can see right now.
pub fn coverage() -> Coverage {
    if !cfg!(unix) {
        Coverage::None
    } else if SUBREAPER.load(Ordering::SeqCst) && ACTIVE_SESSIONS.load(Ordering::SeqCst) == 1 {
        Coverage::GroupAndEscaped
    } else {
        Coverage::GroupOnly
    }
}

/// Registers a running session and the processes it started.
pub(crate) fn session_started(pids: &[i32]) {
    ACTIVE_SESSIONS.fetch_add(1, Ordering::SeqCst);
    SPAWNED.lock().expect("spawned lock").extend_from_slice(pids);
}

pub(crate) fn session_ended(pids: &[i32]) {
    ACTIVE_SESSIONS.fetch_sub(1, Ordering::SeqCst);
    SPAWNED.lock().expect("spawned lock").retain(|p| !pids.contains(p));
}

#[cfg(unix)]
fn spawned_by_lspcheck(pid: i32) -> bool {
    SPAWNED.lock().expect("spawned lock").contains(&pid)
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::Pid;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// The process group the server runs in.
    #[derive(Debug)]
    pub struct Group {
        /// The group id; the sentinel's pid when there is one.
        pub pgid: i32,
        /// Pids that belong to lspcheck's own bookkeeping, not the server.
        pub own: Vec<i32>,
        sentinel: Option<Child>,
    }

    impl Group {
        /// Starts a sentinel from `exe` and returns its group, or `None` to let
        /// the server lead its own group (when no sentinel is available).
        pub fn start(exe: Option<&std::path::Path>) -> std::io::Result<Option<Group>> {
            use std::os::unix::process::CommandExt;
            let Some(exe) = exe else { return Ok(None) };
            let child = Command::new(exe)
                .arg(SENTINEL_ARG)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()?;
            let pid = child.id() as i32;
            Ok(Some(Group { pgid: pid, own: vec![pid], sentinel: Some(child) }))
        }

        /// A group the server leads itself.
        pub fn led_by(pid: i32) -> Group {
            Group { pgid: pid, own: Vec::new(), sentinel: None }
        }

        /// Living members of the group other than lspcheck's own processes.
        pub fn members(&self) -> Result<Vec<ProcInfo>, String> {
            Ok(list_processes()?.into_iter().filter(|p| p.pgid == self.pgid && !self.own.contains(&p.pid)).collect())
        }

        pub fn signal(&self, sig: Signal) {
            let _ = killpg(Pid::from_raw(self.pgid), sig);
        }

        /// Ends the sentinel, waiting at most a second before killing it.
        pub fn close(&mut self) {
            if let Some(mut child) = self.sentinel.take() {
                drop(child.stdin.take());
                let deadline = Instant::now() + Duration::from_secs(1);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            if let Some(mut child) = self.sentinel.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Children of this process that it didn't start: descendants of the
    /// server reparented here. Empty unless [`coverage`] is `GroupAndEscaped`.
    pub fn adopted_orphans() -> Result<Vec<ProcInfo>, String> {
        if coverage() != Coverage::GroupAndEscaped {
            return Ok(Vec::new());
        }
        let me = std::process::id() as i32;
        Ok(list_processes()?.into_iter().filter(|p| p.ppid == me && !spawned_by_lspcheck(p.pid)).collect())
    }

    /// Sends SIGKILL to a leftover process.
    pub fn kill_process(p: &ProcInfo) -> Result<(), String> {
        kill(Pid::from_raw(p.pid), Signal::SIGKILL).map_err(|e| format!("can't stop {p}: {e}"))
    }

    /// Reaps adopted children that have exited, without blocking.
    pub fn reap_adopted() {
        #[cfg(target_os = "linux")]
        if coverage() == Coverage::GroupAndEscaped
            && let Ok(dead) = linux::processes(true)
        {
            use nix::sys::wait::{WaitPidFlag, waitpid};
            let me = std::process::id() as i32;
            for p in dead.into_iter().filter(|p| p.ppid == me && !spawned_by_lspcheck(p.pid)) {
                let _ = waitpid(Pid::from_raw(p.pid), Some(WaitPidFlag::WNOHANG));
            }
        }
    }

    /// Every living process: `/proc` on Linux, `ps` elsewhere.
    pub fn list_processes() -> Result<Vec<ProcInfo>, String> {
        #[cfg(target_os = "linux")]
        {
            linux::processes(false)
        }
        #[cfg(not(target_os = "linux"))]
        {
            ps_processes()
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn ps_processes() -> Result<Vec<ProcInfo>, String> {
        let mut child = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,comm="])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("can't run ps: {e}"))?;
        let mut stdout = child.stdout.take().expect("piped");
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut out);
            out
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ps took more than 5 s".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let out = reader.join().unwrap_or_default();
        if !status.success() {
            return Err(format!("ps failed with {status}"));
        }
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| {
                let mut f = line.split_whitespace();
                let pid = f.next()?.parse().ok()?;
                let ppid = f.next()?.parse().ok()?;
                let pgid = f.next()?.parse().ok()?;
                let stat = f.next()?;
                let name = f.collect::<Vec<_>>().join(" ");
                (!stat.starts_with('Z')).then_some(ProcInfo { pid, ppid, pgid, name })
            })
            .collect())
    }

    #[cfg(target_os = "linux")]
    pub(super) mod linux {
        use super::ProcInfo;

        /// Living processes, or with `zombies`, only the dead ones awaiting a reaper.
        pub fn processes(zombies: bool) -> Result<Vec<ProcInfo>, String> {
            let dir = std::fs::read_dir("/proc").map_err(|e| format!("can't read /proc: {e}"))?;
            Ok(dir
                .filter_map(|e| {
                    let name = e.ok()?.file_name();
                    let pid: i32 = name.to_str()?.parse().ok()?;
                    // Raw bytes: the command name may not be UTF-8.
                    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
                    let (info, state, threads) = parse_stat(pid, &stat)?;
                    // A leader whose main thread exited shows state Z while other
                    // threads still run; it is alive.
                    let dead = state == b'Z' && threads <= 1 || state == b'X';
                    (dead == zombies).then_some(info)
                })
                .collect())
        }

        /// Parses `/proc/<pid>/stat`: `pid (comm) state ppid pgrp ...`, where
        /// `comm` may contain spaces, parentheses and non-UTF-8 bytes.
        pub(super) fn parse_stat(pid: i32, stat: &[u8]) -> Option<(ProcInfo, u8, i64)> {
            let open = stat.iter().position(|&b| b == b'(')?;
            let close = stat.iter().rposition(|&b| b == b')')?;
            let name = String::from_utf8_lossy(&stat[open + 1..close]).into_owned();
            let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let state = *fields.first()?.as_bytes().first()?;
            let ppid = fields.get(1)?.parse().ok()?;
            let pgid = fields.get(2)?.parse().ok()?;
            // Field 20 (num_threads) is the 18th after the closing parenthesis.
            let threads = fields.get(17)?.parse().ok()?;
            Some((ProcInfo { pid, ppid, pgid, name }, state, threads))
        }

        #[cfg(test)]
        mod tests {
            #[test]
            fn parses_stat_with_odd_command_names() {
                let stat = b"123 (a) b (c)) S 1 120 120 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 3 0 1 2 3";
                let (info, state, threads) = super::parse_stat(123, stat).unwrap();
                assert_eq!(info.name, "a) b (c)");
                assert_eq!((state, info.ppid, info.pgid, threads), (b'S', 1, 120, 3));
                let stat = b"9 (\xffx) S 1 9 9 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 1 2 3";
                let (info, _, _) = super::parse_stat(9, stat).unwrap();
                assert_eq!(info.name, "\u{fffd}x");
            }
        }
    }
}

#[cfg(not(unix))]
pub use other::*;

#[cfg(not(unix))]
mod other {
    use super::ProcInfo;

    #[derive(Debug)]
    pub struct Group {
        pub pgid: i32,
        pub own: Vec<i32>,
    }

    impl Group {
        pub fn start(_exe: Option<&std::path::Path>) -> std::io::Result<Option<Group>> {
            Ok(None)
        }
        pub fn led_by(pid: i32) -> Group {
            Group { pgid: pid, own: Vec::new() }
        }
        pub fn members(&self) -> Result<Vec<ProcInfo>, String> {
            Ok(Vec::new())
        }
        pub fn close(&mut self) {}
    }

    pub fn adopted_orphans() -> Result<Vec<ProcInfo>, String> {
        Ok(Vec::new())
    }
    pub fn kill_process(_p: &ProcInfo) -> Result<(), String> {
        Ok(())
    }
    pub fn reap_adopted() {}
}
