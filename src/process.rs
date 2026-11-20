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
//! A descendant that leaves the group with `setsid` is found through its
//! parent while the parent lives. On Linux lspcheck confirms the parent link
//! by listing processes again; elsewhere it can't, and reports such a process
//! as unconfirmed ([`Found`]). On Linux the lspcheck binary also makes
//! itself a child subreaper, so such descendants are reparented to it when
//! their parent exits. lspcheck only treats such adopted children as the
//! server's when it is certain: the process enabled subreaping through
//! [`enable_subreaper`], exactly one session is running, and the child isn't
//! a process lspcheck started itself. Otherwise a descendant that left the
//! group and outlived its parent can't be found, and [`Coverage`] says so.
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

impl AsRef<ProcInfo> for ProcInfo {
    fn as_ref(&self) -> &ProcInfo {
        self
    }
}

/// What a search for the server's leftover processes found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// Processes lspcheck is sure were the server's: members of its group,
    /// processes reparented to lspcheck, and those joined to one of these by
    /// parent links lspcheck confirmed.
    pub confirmed: Vec<ProcInfo>,
    pub unconfirmed: Option<Unconfirmed>,
}

impl Found {
    pub fn is_empty(&self) -> bool {
        self.confirmed.is_empty() && self.unconfirmed.is_none()
    }
}

/// Processes found only through parent links lspcheck couldn't confirm, so
/// they may not have been the server's. There is at least one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconfirmed {
    pub processes: Vec<ProcInfo>,
    /// Why lspcheck couldn't confirm the links.
    pub why: String,
}

/// What a search for leftover processes could see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The process group, and descendants that left it, even once their
    /// parent has exited (Linux subreaper).
    GroupAndEscaped,
    /// The process group and its members' descendants: one that called
    /// `setsid` is invisible once its parent has exited.
    GroupOnly,
    /// Nothing: this platform isn't supported yet.
    None,
}

static SUBREAPER: AtomicBool = AtomicBool::new(false);
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
/// Set once two sessions have run at the same time. Their descendants may then
/// be adopted in either session's lifetime, so neither can tell whose an
/// adopted process is, now or later.
static OVERLAPPED: AtomicBool = AtomicBool::new(false);
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
    } else if SUBREAPER.load(Ordering::SeqCst)
        && !OVERLAPPED.load(Ordering::SeqCst)
        && ACTIVE_SESSIONS.load(Ordering::SeqCst) == 1
    {
        Coverage::GroupAndEscaped
    } else {
        Coverage::GroupOnly
    }
}

/// Registers a running session and the processes it started.
pub(crate) fn session_started(pids: &[i32]) {
    if ACTIVE_SESSIONS.fetch_add(1, Ordering::SeqCst) >= 1 {
        OVERLAPPED.store(true, Ordering::SeqCst);
    }
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

        /// The server's living descendants that lspcheck can see: the group's
        /// members other than lspcheck's own processes, children reparented to
        /// lspcheck that it didn't start (only while [`coverage`] is
        /// `GroupAndEscaped`), and every process descended from one of those,
        /// whatever its group. A process found only through its parent is
        /// confirmed by listing processes again on Linux (see
        /// [`link_holds`]), and never elsewhere. After `deadline` no listing
        /// begins but a first read of /proc, which waits on no process, and
        /// `ps` is stopped at it.
        pub fn descendants(&self, deadline: Instant) -> Result<Found, String> {
            let me = std::process::id() as i32;
            let adopting = coverage() == Coverage::GroupAndEscaped;
            let is_root = |p: &ProcInfo| {
                !self.own.contains(&p.pid)
                    && (p.pgid == self.pgid || adopting && p.ppid == me && !spawned_by_lspcheck(p.pid))
            };
            #[cfg(target_os = "linux")]
            {
                confirmed_descendants(|| linux::processes(false), is_root, deadline)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Ok(linked_unconfirmed(&ps_processes(deadline)?, is_root))
            }
        }

        pub fn signal(&self, sig: Signal) {
            let _ = killpg(Pid::from_raw(self.pgid), sig);
        }

        /// Ends the sentinel, waiting at most a second before killing it.
        pub fn close(&mut self) {
            if let Some(mut child) = self.sentinel.take() {
                drop(child.stdin.take());
                if !reap_within(&mut child, Duration::from_secs(1)) {
                    let _ = child.kill();
                    reap_within(&mut child, Duration::from_secs(1));
                }
            }
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            if let Some(mut child) = self.sentinel.take() {
                let _ = child.kill();
                reap_within(&mut child, Duration::from_secs(1));
            }
        }
    }

    /// Waits up to `limit` for a child to exit, reaping it. A child that
    /// doesn't (one stuck in the kernel) is left rather than waited on forever.
    fn reap_within(child: &mut Child, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return true,
                Ok(None) if Instant::now() >= deadline => return false,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// A process as listed from `/proc`, with when it started, in clock ticks
    /// since boot. With the pid it names one process in practice: the wall
    /// clock doesn't move it, and Linux hands out pids in turn, so reusing one
    /// within a tick would take a full turn of the pid space.
    #[cfg(target_os = "linux")]
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Listed {
        pub info: ProcInfo,
        pub start: u64,
    }

    #[cfg(target_os = "linux")]
    impl AsRef<ProcInfo> for Listed {
        fn as_ref(&self) -> &ProcInfo {
            &self.info
        }
    }

    /// The server's descendants in a listing from `list`. Those `is_root`
    /// picks are confirmed as listed; one found only through parent links
    /// counts once the next listing confirms each link ([`link_holds`]).
    /// While links keep changing, lspcheck lists again, a few times at most,
    /// and not once `deadline` has passed; what it confirmed meanwhile stays
    /// confirmed, and only what it couldn't is left unconfirmed.
    #[cfg(target_os = "linux")]
    fn confirmed_descendants(
        mut list: impl FnMut() -> Result<Vec<Listed>, String>,
        is_root: impl Fn(&ProcInfo) -> bool,
        deadline: Instant,
    ) -> Result<Found, String> {
        let mut listed = list()?;
        let mut confirmed: Vec<Listed> = Vec::new();
        let mut round = 0;
        loop {
            round += 1;
            let (found, links) = descend(&listed, &is_root);
            let mut sure = vec![false; listed.len()];
            for &i in &found[..found.len() - links.len()] {
                sure[i] = true;
            }
            let again = if links.is_empty() {
                Ok(Vec::new())
            } else if Instant::now() >= deadline {
                Err("the search ran out of time before lspcheck could list processes again".to_string())
            } else {
                list().map_err(|e| format!("lspcheck couldn't list processes again: {e}"))
            };
            if let Ok(again) = &again {
                // Links come in the order they were followed, so a parent's
                // own link has been judged before its children's.
                for &(child, parent) in &links {
                    sure[child] = sure[parent] && link_holds(&listed[child], &listed[parent], again, &is_root);
                }
            }
            for &i in found.iter().filter(|&&i| sure[i]) {
                if !confirmed.contains(&listed[i]) {
                    confirmed.push(listed[i].clone());
                }
            }
            let doubtful: Vec<ProcInfo> = found
                .iter()
                .filter(|&&i| !sure[i] && !confirmed.contains(&listed[i]))
                .map(|&i| listed[i].info.clone())
                .collect();
            let why = match again {
                Ok(_) if doubtful.is_empty() => None,
                Ok(_) if round == 4 => Some("their parent links kept changing while lspcheck listed processes".into()),
                Ok(again) => {
                    listed = again;
                    continue;
                }
                Err(why) => Some(why),
            };
            let unconfirmed = why.filter(|_| !doubtful.is_empty()).map(|why| Unconfirmed { processes: doubtful, why });
            return Ok(Found { confirmed: confirmed.into_iter().map(|p| p.info).collect(), unconfirmed });
        }
    }

    /// The processes in `listed` that `is_root` picks, and every process
    /// descended from one of them through parent links, as indices into
    /// `listed`, those `is_root` picks first; and each link followed, as the
    /// child's index and its parent's, in the order followed. Each process
    /// after the picked ones was found through exactly one link.
    fn descend<T: AsRef<ProcInfo>>(
        listed: &[T],
        is_root: impl Fn(&ProcInfo) -> bool,
    ) -> (Vec<usize>, Vec<(usize, usize)>) {
        let mut found: Vec<usize> = (0..listed.len()).filter(|&i| is_root(listed[i].as_ref())).collect();
        let mut links = Vec::new();
        let mut next = 0;
        while let Some(&parent) = found.get(next) {
            for (child, p) in listed.iter().enumerate() {
                if p.as_ref().ppid == listed[parent].as_ref().pid && !found.contains(&child) {
                    found.push(child);
                    links.push((child, parent));
                }
            }
            next += 1;
        }
        (found, links)
    }

    /// Whether the link read in a listing from `child` to `parent` joined
    /// them. A listing isn't taken at one instant: the parent may have exited
    /// after it was read and its pid gone to another process, or the child
    /// have been read while the pid still belonged to an earlier one. The
    /// link is real when the child didn't start before the parent, and
    /// `again`, a listing begun after that one ended, shows the same parent
    /// (same pid and start), still picked by `is_root` if it was, still named
    /// by the same child. The parent then held its pid from before the child
    /// was read until after, and the child had no other parent meanwhile.
    #[cfg(target_os = "linux")]
    fn link_holds(child: &Listed, parent: &Listed, again: &[Listed], is_root: impl Fn(&ProcInfo) -> bool) -> bool {
        let find = |p: &Listed| again.iter().find(|q| q.info.pid == p.info.pid && q.start == p.start);
        child.start >= parent.start
            && find(parent).is_some_and(|p| !is_root(&parent.info) || is_root(&p.info))
            && find(child).is_some_and(|c| c.info.ppid == parent.info.pid)
    }

    /// The server's descendants in `listed`, from `ps`. Those `is_root` picks
    /// are confirmed as listed, but one found only through parent links
    /// stays unconfirmed: `ps` gives start times only to the second, from a
    /// wall clock that may be stepped, so they can't show that a pid didn't
    /// pass to another process while `ps` listed them.
    #[cfg(any(test, not(target_os = "linux")))]
    fn linked_unconfirmed(listed: &[ProcInfo], is_root: impl Fn(&ProcInfo) -> bool) -> Found {
        let (found, links) = descend(listed, is_root);
        let (roots, linked) = found.split_at(found.len() - links.len());
        let pick = |indices: &[usize]| indices.iter().map(|&i| listed[i].clone()).collect::<Vec<_>>();
        Found {
            confirmed: pick(roots),
            unconfirmed: (!linked.is_empty()).then(|| Unconfirmed {
                processes: pick(linked),
                why: "on macOS, ps can't show that a pid stayed with one process".into(),
            }),
        }
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
            for p in dead.into_iter().map(|p| p.info).filter(|p| p.ppid == me && !spawned_by_lspcheck(p.pid)) {
                let _ = waitpid(Pid::from_raw(p.pid), Some(WaitPidFlag::WNOHANG));
            }
        }
    }

    /// Every living process, from `ps`, which is given at most 5 s and no
    /// later than `deadline`. Linux reads /proc instead; tests run this there
    /// too.
    #[cfg(any(test, not(target_os = "linux")))]
    fn ps_processes(deadline: Instant) -> Result<Vec<ProcInfo>, String> {
        let limit = Duration::from_secs(5).min(deadline.saturating_duration_since(Instant::now()));
        if limit.is_zero() {
            return Err("the search ran out of time before lspcheck could run ps".into());
        }
        let mut ps = Command::new("ps");
        ps.args(["-A", "-o", "pid=,ppid=,pgid=,stat=,comm="]);
        let out = output_within(&mut ps, limit)?;
        Ok(String::from_utf8_lossy(&out).lines().filter_map(parse_ps_line).collect())
    }

    /// Parses a line of [`ps_processes`]'s output, or `None` for a zombie.
    #[cfg(any(test, not(target_os = "linux")))]
    fn parse_ps_line(line: &str) -> Option<ProcInfo> {
        let mut f = line.split_whitespace();
        let pid = f.next()?.parse().ok()?;
        let ppid = f.next()?.parse().ok()?;
        let pgid = f.next()?.parse().ok()?;
        let stat = f.next()?;
        let name = f.collect::<Vec<_>>().join(" ");
        (!stat.starts_with('Z')).then_some(ProcInfo { pid, ppid, pgid, name })
    }

    /// The most output [`output_within`] reads; `ps` writes well under a
    /// hundred bytes per process.
    #[cfg(any(test, not(target_os = "linux")))]
    const MAX_OUTPUT: u64 = 16 * 1024 * 1024;

    /// Runs `cmd` and returns what it wrote to stdout, stopping it if it
    /// hasn't exited within `limit`. Its output goes to an unnamed temporary
    /// file, not a pipe: a process it leaves behind could hold a pipe open
    /// for ever, so reading one might never end, but the file holds the whole
    /// output once the command has exited, and is closed on return. Output
    /// over [`MAX_OUTPUT`], or read only in part, is an error: what was left
    /// out could have listed anything.
    #[cfg(any(test, not(target_os = "linux")))]
    fn output_within(cmd: &mut Command, limit: Duration) -> Result<Vec<u8>, String> {
        use std::io::{Read, Seek};
        let name = cmd.get_program().to_string_lossy().into_owned();
        let cant_run = |e: std::io::Error| format!("can't run {name}: {e}");
        let mut file = tempfile::tempfile().map_err(cant_run)?;
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(file.try_clone().map_err(cant_run)?)
            .stderr(Stdio::null())
            .spawn()
            .map_err(cant_run)?;
        let deadline = Instant::now() + limit;
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                reap_within(&mut child, Duration::from_secs(1));
                return Err(format!("{name} took more than {} ms", limit.as_millis()));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if !status.success() {
            return Err(format!("{name} failed with {status}"));
        }
        // Only what is there now: a process left behind may still be writing.
        // The file is a regular one, so reading it waits on no process.
        let cant_read = |e: std::io::Error| format!("can't read {name}'s output: {e}");
        let len = file.metadata().map_err(cant_read)?.len();
        if len > MAX_OUTPUT {
            return Err(format!("{name} wrote more than {} MiB", MAX_OUTPUT / (1024 * 1024)));
        }
        file.rewind().map_err(cant_read)?;
        let mut out = Vec::new();
        file.take(len).read_to_end(&mut out).map_err(cant_read)?;
        if out.len() as u64 != len {
            return Err(format!("{name}'s output was cut short: {} of {len} bytes read", out.len()));
        }
        Ok(out)
    }

    #[cfg(target_os = "linux")]
    pub(super) mod linux {
        use super::{Listed, ProcInfo};

        /// Living processes, or with `zombies`, only the dead ones awaiting a
        /// reaper, in the order of their pids.
        pub fn processes(zombies: bool) -> Result<Vec<Listed>, String> {
            let dir = std::fs::read_dir("/proc").map_err(|e| format!("can't read /proc: {e}"))?;
            Ok(dir
                .filter_map(|e| {
                    let name = e.ok()?.file_name();
                    let pid: i32 = name.to_str()?.parse().ok()?;
                    // Raw bytes: the command name may not be UTF-8.
                    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
                    let (listed, state, threads) = parse_stat(pid, &stat)?;
                    // A leader whose main thread exited shows state Z while other
                    // threads still run; it is alive.
                    let dead = state == b'Z' && threads <= 1 || state == b'X';
                    (dead == zombies).then_some(listed)
                })
                .collect())
        }

        /// Parses `/proc/<pid>/stat`: `pid (comm) state ppid pgrp ...`, where
        /// `comm` may contain spaces, parentheses and non-UTF-8 bytes.
        pub(super) fn parse_stat(pid: i32, stat: &[u8]) -> Option<(Listed, u8, i64)> {
            let open = stat.iter().position(|&b| b == b'(')?;
            let close = stat.iter().rposition(|&b| b == b')')?;
            let name = String::from_utf8_lossy(&stat[open + 1..close]).into_owned();
            let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let state = *fields.first()?.as_bytes().first()?;
            let ppid = fields.get(1)?.parse().ok()?;
            let pgid = fields.get(2)?.parse().ok()?;
            // Field n is the (n - 2)th after the closing parenthesis: 20 is
            // num_threads, 22 starttime (clock ticks since boot).
            let threads = fields.get(17)?.parse().ok()?;
            let start = fields.get(19)?.parse().ok()?;
            Some((Listed { info: ProcInfo { pid, ppid, pgid, name }, start }, state, threads))
        }

        #[cfg(test)]
        mod tests {
            #[test]
            fn parses_stat_with_odd_command_names() {
                let stat = b"123 (a) b (c)) S 1 120 120 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 3 0 4567 2 3";
                let (listed, state, threads) = super::parse_stat(123, stat).unwrap();
                assert_eq!(listed.info.name, "a) b (c)");
                assert_eq!((state, listed.info.ppid, listed.info.pgid, threads, listed.start), (b'S', 1, 120, 3, 4567));
                let stat = b"9 (\xffx) S 1 9 9 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 1 2 3";
                let (listed, _, _) = super::parse_stat(9, stat).unwrap();
                assert_eq!(listed.info.name, "\u{fffd}x");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ps_lists_this_process_with_its_parent_and_group() {
            let me = std::process::id() as i32;
            let all = ps_processes(Instant::now() + Duration::from_secs(30)).unwrap();
            let p = all.iter().find(|p| p.pid == me).expect("this process");
            assert_eq!((p.ppid, p.pgid), (nix::unistd::getppid().as_raw(), nix::unistd::getpgrp().as_raw()));
        }

        #[test]
        fn ps_isnt_run_once_the_search_is_out_of_time() {
            let e = ps_processes(Instant::now()).unwrap_err();
            assert_eq!(e, "the search ran out of time before lspcheck could run ps");
        }

        #[test]
        fn ps_lines_are_read_without_zombies() {
            let p = parse_ps_line("  41   1  41 Ss   /usr/bin/my server").unwrap();
            assert_eq!(p, ProcInfo { pid: 41, ppid: 1, pgid: 41, name: "/usr/bin/my server".into() });
            assert_eq!(parse_ps_line("45 41 41 Z gone"), None);
        }

        #[test]
        fn ps_output_over_the_cap_is_an_error_not_a_listing() {
            let mut at_cap = Command::new("head");
            at_cap.args(["-c", &MAX_OUTPUT.to_string(), "/dev/zero"]);
            assert_eq!(output_within(&mut at_cap, Duration::from_secs(30)).unwrap().len() as u64, MAX_OUTPUT);
            let mut over = Command::new("head");
            over.args(["-c", &(MAX_OUTPUT + 1).to_string(), "/dev/zero"]);
            let e = output_within(&mut over, Duration::from_secs(30)).unwrap_err();
            assert_eq!(e, "head wrote more than 16 MiB");
        }

        #[test]
        fn a_process_found_only_through_its_parent_is_unconfirmed_where_ps_lists_them() {
            // 10 leads the server's group; 20 left it with setsid, and 30 is
            // 20's child; 40 is unrelated.
            let p = |pid, ppid, pgid| ProcInfo { pid, ppid, pgid, name: format!("p{pid}") };
            let listed = [p(10, 1, 10), p(20, 10, 20), p(30, 20, 20), p(40, 1, 40)];
            let found = linked_unconfirmed(&listed, |p| p.pgid == 10);
            assert_eq!(found.confirmed, [p(10, 1, 10)]);
            let unconfirmed = found.unconfirmed.unwrap();
            assert_eq!(unconfirmed.processes, [p(20, 10, 20), p(30, 20, 20)]);
            assert_eq!(unconfirmed.why, "on macOS, ps can't show that a pid stayed with one process");
            assert_eq!(linked_unconfirmed(&listed[..1], |p| p.pgid == 10).unconfirmed, None);
        }

        #[cfg(target_os = "linux")]
        fn row(pid: i32, ppid: i32, pgid: i32, start: u64) -> Listed {
            Listed { info: ProcInfo { pid, ppid, pgid, name: format!("p{pid}") }, start }
        }

        #[cfg(target_os = "linux")]
        fn pids(found: &[ProcInfo]) -> Vec<i32> {
            found.iter().map(|p| p.pid).collect()
        }

        #[cfg(target_os = "linux")]
        fn in_group(p: &ProcInfo) -> bool {
            p.pgid == 10
        }

        /// The search for the group led by 10, over `listings` in turn.
        #[cfg(target_os = "linux")]
        fn search(listings: Vec<Vec<Listed>>, deadline: Instant) -> Found {
            let mut listings = listings.into_iter();
            let list = move || listings.next().ok_or_else(|| "listed once too often".to_string());
            confirmed_descendants(list, in_group, deadline).unwrap()
        }

        #[cfg(target_os = "linux")]
        fn later() -> Instant {
            Instant::now() + Duration::from_secs(60)
        }

        /// Whether every link followed in `listed` holds in `again`.
        #[cfg(target_os = "linux")]
        fn all_hold(listed: &[Listed], again: &[Listed]) -> bool {
            let (_, links) = descend(listed, in_group);
            links.iter().all(|&(child, parent)| link_holds(&listed[child], &listed[parent], again, in_group))
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_descendant_found_through_its_parent_counts_once_a_later_listing_confirms_the_link() {
            // 10 leads the server's group; 20 left it with setsid, and 30 is
            // 20's child; 40 is unrelated.
            let listed = vec![row(10, 1, 10, 100), row(20, 10, 20, 105), row(30, 20, 20, 110), row(40, 1, 40, 50)];
            let found = search(vec![listed.clone(), listed], later());
            assert_eq!((pids(&found.confirmed), found.unconfirmed), (vec![10, 20, 30], None));
            // Nothing found through a link needs no second listing.
            let alone = vec![row(10, 1, 10, 100), row(40, 1, 40, 50)];
            assert_eq!(
                search(vec![alone], later()),
                Found { confirmed: vec![row(10, 1, 10, 100).info], unconfirmed: None }
            );
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_reused_pid_doesnt_join_an_unrelated_process_to_the_servers() {
            // 20, the server's, was read; then it exited and its pid went to
            // an unrelated process (started at 120), whose child 30 was read
            // later in the same listing. The next listing shows another 20.
            let reused = vec![row(10, 1, 10, 100), row(20, 10, 20, 105), row(30, 20, 30, 125)];
            let after = vec![row(10, 1, 10, 100), row(20, 1, 20, 120), row(30, 20, 30, 125)];
            assert!(!all_hold(&reused, &after));
            // Read while 15 still belonged to an earlier process, 25 started
            // before the 15 that is the server's: the link can't be real.
            let early = vec![row(10, 1, 10, 100), row(15, 10, 15, 102), row(25, 15, 25, 90)];
            assert!(!all_hold(&early, &early));
            // Started in the same tick as the 15 read after it, 25 was really
            // an earlier 15's child: by the next listing it has been reparented.
            let same_tick = vec![row(10, 1, 10, 100), row(15, 10, 15, 102), row(25, 15, 25, 102)];
            let reparented = vec![row(10, 1, 10, 100), row(15, 10, 15, 102), row(25, 1, 25, 102)];
            assert!(!all_hold(&same_tick, &reparented));
            // The process a link leads from must still be the server's.
            let left_group = vec![row(10, 1, 11, 100), row(15, 10, 15, 102), row(25, 15, 25, 102)];
            assert!(all_hold(&same_tick, &same_tick) && !all_hold(&same_tick, &left_group));
            // A link that didn't hold makes the search list again, from the
            // listing that showed it.
            let found = search(vec![reused.clone(), after], later());
            assert_eq!((pids(&found.confirmed), found.unconfirmed), (vec![10], None));
            let kept = vec![row(10, 1, 10, 100), row(20, 10, 20, 105)];
            let found = search(vec![reused, kept.clone(), kept], later());
            assert_eq!((pids(&found.confirmed), found.unconfirmed), (vec![10, 20], None));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn links_that_keep_changing_leave_only_their_processes_unconfirmed() {
            // 50 is a member of the server's group that outlived it; the
            // process found through it changes in every listing.
            let churn: Vec<_> = (0..5).map(|n| vec![row(50, 1, 10, 90), row(20, 50, 20, 105 + n)]).collect();
            let found = search(churn, later());
            assert_eq!(pids(&found.confirmed), [50]);
            let unconfirmed = found.unconfirmed.unwrap();
            assert_eq!(pids(&unconfirmed.processes), [20]);
            assert_eq!(unconfirmed.why, "their parent links kept changing while lspcheck listed processes");
            // What a listing confirmed stays confirmed, though it has gone by
            // the last listing.
            let gone = vec![row(50, 1, 10, 90), row(21, 50, 21, 106)];
            let listings = vec![
                vec![row(50, 1, 10, 90), row(20, 50, 20, 105), row(30, 50, 30, 105)],
                vec![row(50, 1, 10, 90), row(20, 50, 20, 105), row(30, 50, 30, 106)],
                gone.clone(),
                gone.clone(),
            ];
            let found = search(listings, later());
            assert_eq!((pids(&found.confirmed), found.unconfirmed), (vec![50, 20, 21], None));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_search_out_of_time_doesnt_list_again_to_confirm_links() {
            let listed = vec![row(10, 1, 10, 100), row(20, 10, 20, 105), row(30, 20, 20, 110)];
            let found = search(vec![listed], Instant::now());
            assert_eq!(pids(&found.confirmed), [10]);
            let unconfirmed = found.unconfirmed.unwrap();
            assert_eq!(pids(&unconfirmed.processes), [20, 30]);
            assert_eq!(unconfirmed.why, "the search ran out of time before lspcheck could list processes again");
        }

        #[test]
        fn a_commands_output_is_read_though_a_process_it_left_holds_it_open() {
            // The shell exits at once; the sleep it started keeps its stdout.
            let mut sh = Command::new("sh");
            sh.args(["-c", "sleep 60 & echo $!"]);
            let out = output_within(&mut sh, Duration::from_secs(30)).unwrap();
            let pid: i32 = String::from_utf8(out).unwrap().trim().parse().unwrap();
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub use other::*;

#[cfg(not(unix))]
mod other {
    use super::{Found, ProcInfo};

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
        pub fn descendants(&self, _deadline: std::time::Instant) -> Result<Found, String> {
            Ok(Found::default())
        }
        pub fn close(&mut self) {}
    }

    pub fn kill_process(_p: &ProcInfo) -> Result<(), String> {
        Ok(())
    }
    pub fn reap_adopted() {}
}
