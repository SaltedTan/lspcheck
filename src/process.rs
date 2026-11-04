// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Starting the server in a process group lspcheck owns, finding what it left
//! behind, and stopping it.
//!
//! On Unix the server joins the process group of a small *sentinel* process
//! (lspcheck itself, run with [`SENTINEL_ARG`]). The sentinel stays alive until
//! the session ends, so the group id can't be recycled for an unrelated group
//! while lspcheck is still looking for the server's descendants or signalling
//! them. On Linux lspcheck also makes itself a child subreaper, so descendants
//! that leave the group with `setsid` are reparented to lspcheck when their
//! parent exits, and can still be found.
//!
//! On Windows only the server process itself is tracked so far.

use std::ffi::OsString;
use std::path::PathBuf;

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
    pub fn new(argv: &[OsString]) -> Self {
        Self {
            program: argv[0].clone(),
            args: argv[1..].to_vec(),
            cwd: None,
            env: Vec::new(),
        }
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
    pub name: String,
}

impl std::fmt::Display for ProcInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid {} ({})", self.pid, self.name)
    }
}

/// Makes this process a child subreaper where the OS supports it (Linux).
/// Call once, early, from the binary's `main`.
pub fn enable_subreaper() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        nix::sys::prctl::set_child_subreaper(true).is_ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        false
    }
}

/// Whether this platform can find the server's descendants.
pub fn tracks_descendants() -> bool {
    cfg!(unix)
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    use std::process::{Child, Command, Stdio};

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
            Ok(Some(Group {
                pgid: pid,
                own: vec![pid],
                sentinel: Some(child),
            }))
        }

        /// A group the server leads itself.
        pub fn led_by(pid: i32) -> Group {
            Group {
                pgid: pid,
                own: Vec::new(),
                sentinel: None,
            }
        }

        /// Living members of the group other than lspcheck's own processes.
        pub fn members(&self) -> Vec<ProcInfo> {
            list_processes()
                .into_iter()
                .filter(|p| p.pgid == self.pgid && !self.own.contains(&p.pid))
                .collect()
        }

        pub fn signal(&self, sig: Signal) {
            let _ = killpg(Pid::from_raw(self.pgid), sig);
        }

        /// Ends the sentinel. Call after everything else in the group is gone.
        pub fn close(&mut self) {
            if let Some(mut child) = self.sentinel.take() {
                drop(child.stdin.take());
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

    /// Children of this process that it didn't start: descendants of a server
    /// that were reparented here because lspcheck is a subreaper.
    pub fn adopted_orphans(known: &[i32]) -> Vec<ProcInfo> {
        let me = std::process::id() as i32;
        list_processes()
            .into_iter()
            .filter(|p| p.ppid == me && !known.contains(&p.pid))
            .collect()
    }

    /// Kills a process and, if it is this process's child, reaps it.
    pub fn kill_and_reap(p: &ProcInfo) {
        use nix::sys::wait::{WaitPidFlag, waitpid};
        let pid = Pid::from_raw(p.pid);
        let _ = nix::sys::signal::kill(pid, Signal::SIGKILL);
        if p.ppid == std::process::id() as i32 {
            let _ = waitpid(pid, Some(WaitPidFlag::empty()));
        }
    }

    /// Reaps adopted children that have already exited, so they don't linger as zombies.
    /// Only Linux adopts orphans (lspcheck is a subreaper there).
    pub fn reap_adopted_zombies(known: &[i32]) {
        #[cfg(target_os = "linux")]
        {
            use nix::sys::wait::{WaitPidFlag, waitpid};
            for p in linux::zombies() {
                if p.ppid == std::process::id() as i32 && !known.contains(&p.pid) {
                    let _ = waitpid(Pid::from_raw(p.pid), Some(WaitPidFlag::WNOHANG));
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = known;
    }

    /// Every living process: `/proc` on Linux, `ps` elsewhere.
    pub fn list_processes() -> Vec<ProcInfo> {
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
    fn ps_processes() -> Vec<ProcInfo> {
        let Ok(out) = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,comm="])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut f = line.split_whitespace();
                let pid = f.next()?.parse().ok()?;
                let ppid = f.next()?.parse().ok()?;
                let pgid = f.next()?.parse().ok()?;
                let stat = f.next()?;
                let name = f.collect::<Vec<_>>().join(" ");
                (!stat.starts_with('Z')).then_some(ProcInfo {
                    pid,
                    ppid,
                    pgid,
                    name,
                })
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::ProcInfo;

        pub fn zombies() -> Vec<ProcInfo> {
            processes(true)
        }

        /// Living processes, or with `zombies`, only the dead ones awaiting a reaper.
        pub fn processes(zombies: bool) -> Vec<ProcInfo> {
            let Ok(dir) = std::fs::read_dir("/proc") else {
                return Vec::new();
            };
            dir.filter_map(|e| {
                let name = e.ok()?.file_name();
                let pid: i32 = name.to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let (info, state, threads) = parse_stat(pid, &stat)?;
                // A leader whose main thread exited shows state Z while other
                // threads still run; it is alive.
                let dead = state == 'Z' && threads <= 1 || state == 'X';
                (dead == zombies).then_some(info)
            })
            .collect()
        }

        /// Parses `/proc/<pid>/stat`: `pid (comm) state ppid pgrp ...`, where
        /// `comm` may itself contain spaces and parentheses.
        pub(super) fn parse_stat(pid: i32, stat: &str) -> Option<(ProcInfo, char, i64)> {
            let open = stat.find('(')?;
            let close = stat.rfind(')')?;
            let name = stat[open + 1..close].to_string();
            let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
            let state = rest.first()?.chars().next()?;
            let ppid = rest.get(1)?.parse().ok()?;
            let pgid = rest.get(2)?.parse().ok()?;
            // Field 20 (num_threads) is the 18th after the closing parenthesis.
            let threads = rest.get(17)?.parse().ok()?;
            Some((
                ProcInfo {
                    pid,
                    ppid,
                    pgid,
                    name,
                },
                state,
                threads,
            ))
        }

        #[cfg(test)]
        mod tests {
            #[test]
            fn parses_stat_with_odd_command_names() {
                let stat = "123 (a) b (c)) S 1 120 120 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 3 0 1 2 3";
                let (info, state, threads) = super::parse_stat(123, stat).unwrap();
                assert_eq!(info.name, "a) b (c)");
                assert_eq!((state, info.ppid, info.pgid, threads), ('S', 1, 120, 3));
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
            Group {
                pgid: pid,
                own: Vec::new(),
            }
        }
        pub fn members(&self) -> Vec<ProcInfo> {
            Vec::new()
        }
        pub fn close(&mut self) {}
    }

    pub fn adopted_orphans(_known: &[i32]) -> Vec<ProcInfo> {
        Vec::new()
    }
    pub fn kill_and_reap(_p: &ProcInfo) {}
    pub fn reap_adopted_zombies(_known: &[i32]) {}
}
