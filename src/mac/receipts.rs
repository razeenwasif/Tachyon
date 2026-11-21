//! What a command cost: CPU time, how much of it ran on performance cores,
//! energy, and peak memory.
//!
//! CPU time is exact. The shell is the parent of every command it runs, and the
//! kernel adds a child's CPU time to its parent's "children" totals when the
//! parent reaps it -- so the difference in the shell's child CPU time between
//! the command starting (OSC 133;C) and finishing (OSC 133;D) is the command's
//! CPU time, including everything it spawned and waited for.
//!
//! The rest uses `kqueue` (`EVFILT_PROC` with `NOTE_FORK | NOTE_EXEC |
//! NOTE_EXIT`) combined with `proc_pid_rusage` (`RUSAGE_INFO_V6`). While idle,
//! the sampler thread blocks in `kevent` with no timeout (zero wake-ups). When
//! a command starts, it watches the shell and every newly forked descendant,
//! discovers foreground processes both through `tcgetpgrp` and `PROC_PIDTBSDINFO`
//! (`e_tpgid`, so local `tachyon server` Mux v2 panes work even without a PTY
//! master fd), samples in a short 25 ms burst for the first 500 ms after start
//! or any fork (plus immediately on `NOTE_EXEC` and `NOTE_EXIT`), and backs off
//! to 500 ms for long runs. Per-PID metrics merge monotonically so a late or
//! partial post-exit read never clobbers the high-water peak footprint or
//! energy. On unified memory the footprint is the number that decides whether a
//! model fits.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::term::Receipt;

/// `proc_pid_rusage` flavour with P-core time and energy (macOS 12+).
const RUSAGE_INFO_V6: libc::c_int = 6;
/// Steady-state sampling and GUI refresh cadence for long commands.
const SAMPLE_EVERY: Duration = Duration::from_millis(500);
/// Fast burst cadence while a command or newly forked child is younger than
/// `FAST_BURST_WINDOW`, so sub-500 ms commands get peak footprint & energy.
const FAST_SAMPLE_EVERY: Duration = Duration::from_millis(25);
const FAST_BURST_WINDOW: Duration = Duration::from_millis(500);
/// `EVFILT_USER` identifier used to wake the `kqueue` thread from `start()`,
/// `finish()`, and `Drop`.
const WAKE_IDENT: usize = 1;
/// `PROC_PGRP_ONLY` from `<sys/proc_info.h>`: list the pids of one process group.
const PROC_PGRP_ONLY: u32 = 2;
/// `PROC_PPID_ONLY` from `<sys/proc_info.h>`: list direct children of one ppid.
const PROC_PPID_ONLY: u32 = 6;
/// Upper bound on tracked descendants per command so fork floods stay bounded.
const MAX_DESCENDANTS: usize = 256;

/// `struct rusage_info_v6` from `<sys/resource.h>`: v4, which `libc` defines,
/// followed by the fields added in v5 and v6.
#[repr(C)]
#[derive(Default)]
pub(super) struct RusageV6 {
    pub(super) v4: RusageV4,
    flags: u64,
    user_ptime: u64,
    system_ptime: u64,
    pinstructions: u64,
    pcycles: u64,
    energy_nj: u64,
    penergy_nj: u64,
    secure_time_in_system: u64,
    secure_ptime_in_system: u64,
    neural_footprint: u64,
    lifetime_max_neural_footprint: u64,
    interval_max_neural_footprint: u64,
    reserved: [u64; 9],
}

/// `libc::rusage_info_v4`, wrapped so it can derive `Default`.
#[repr(transparent)]
pub(super) struct RusageV4(pub(super) libc::rusage_info_v4);

impl Default for RusageV4 {
    fn default() -> Self {
        // SAFETY: a plain struct of integers; all-zero is a valid value.
        RusageV4(unsafe { std::mem::zeroed() })
    }
}

pub(super) fn rusage(pid: libc::pid_t) -> Option<RusageV6> {
    let mut r = RusageV6::default();
    // SAFETY: the buffer is at least as large as rusage_info_v6.
    let rc = unsafe {
        libc::proc_pid_rusage(pid, RUSAGE_INFO_V6, &mut r as *mut RusageV6 as *mut libc::rusage_info_t)
    };
    (rc == 0).then_some(r)
}

/// rusage times are in Mach absolute-time units, which on Apple silicon are
/// not nanoseconds (the timebase is 125/3).
// libc marks the timebase struct deprecated in favour of the mach2 crate; the
// call itself is the documented API and one more dependency is not worth it.
#[allow(deprecated)]
pub(super) fn to_nanos(ticks: u64) -> u64 {
    static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: valid out-pointer.
        unsafe { libc::mach_timebase_info(&mut tb) };
        if tb.denom == 0 { (1, 1) } else { (tb.numer, tb.denom) }
    });
    (ticks as u128 * numer as u128 / denom as u128) as u64
}

/// CPU time of the shell's reaped children so far.
pub fn children_cpu(shell: libc::pid_t) -> Option<Duration> {
    if shell <= 0 {
        return None;
    }
    let r = rusage(shell)?;
    Some(Duration::from_nanos(to_nanos(r.v4.0.ri_child_user_time + r.v4.0.ri_child_system_time)))
}

/// The high-water reading for one process of a running command.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct ProcSample {
    cpu_ns: u64,
    pcore_ns: u64,
    energy_nj: u64,
    peak: u64,
}

impl ProcSample {
    fn of(r: &RusageV6) -> ProcSample {
        ProcSample {
            cpu_ns: to_nanos(r.v4.0.ri_user_time + r.v4.0.ri_system_time),
            pcore_ns: to_nanos(r.user_ptime + r.system_ptime),
            energy_nj: r.energy_nj,
            peak: r.v4.0.ri_lifetime_max_phys_footprint,
        }
    }

    /// Merge another sample of the same PID monotonically so a late or partial
    /// post-exit read can never reduce peak memory, energy, or CPU counters.
    fn merge(&mut self, other: ProcSample) {
        self.cpu_ns = self.cpu_ns.max(other.cpu_ns);
        self.pcore_ns = self.pcore_ns.max(other.pcore_ns);
        self.energy_nj = self.energy_nj.max(other.energy_nj);
        self.peak = self.peak.max(other.peak);
    }
}

#[derive(Default)]
struct State {
    /// Sampling a command: (shell pid, PTY master fd or -1 for Mux v2 local panes).
    target: Option<(libc::pid_t, libc::c_int)>,
    /// Incremented on every `start` / `finish` so the worker resets watches cleanly.
    generation: u64,
    /// Direct children of `shell` already alive before `start()` (background jobs).
    baseline_children: HashSet<libc::pid_t>,
    procs: HashMap<libc::pid_t, ProcSample>,
    shutdown: bool,
}

/// One `kqueue`-backed sampler thread per pane. It blocks in `kevent` with no
/// timeout while no command runs, so an idle shell costs zero wake-ups.
pub struct Sampler {
    shared: Arc<Mutex<State>>,
    kq: Option<Arc<OwnedFd>>,
}

impl Sampler {
    /// `on_tick` runs at most every `SAMPLE_EVERY` while a command runs, so the
    /// window can refresh its "running 12s" readout.
    pub fn spawn(on_tick: impl Fn() + Send + 'static) -> Sampler {
        let shared = Arc::new(Mutex::new(State::default()));
        let kq = create_kqueue().map(Arc::new);
        if let Some(worker_kq) = kq.as_ref().map(Arc::clone) {
            let worker_state = Arc::clone(&shared);
            let _ = std::thread::Builder::new()
                .name("tachyon-receipts".into())
                .spawn(move || sample_loop(worker_kq, worker_state, on_tick));
        }
        Sampler { shared, kq }
    }

    pub fn start(&self, shell: libc::pid_t, master_fd: libc::c_int) {
        let baseline = if shell > 0 {
            let fg_pids: HashSet<libc::pid_t> = foreground_pgid(shell, master_fd)
                .map(|pg| list_pids(PROC_PGRP_ONLY, pg as u32).into_iter().collect())
                .unwrap_or_default();
            list_pids(PROC_PPID_ONLY, shell as u32)
                .into_iter()
                .filter(|p| *p != shell && !fg_pids.contains(p))
                .collect()
        } else {
            HashSet::new()
        };
        {
            let mut s = self.shared.lock().unwrap();
            s.procs.clear();
            s.baseline_children = baseline;
            s.target = Some((shell, master_fd));
            s.generation = s.generation.wrapping_add(1);
        }
        if let Some(kq) = &self.kq {
            trigger_wake(kq.as_raw_fd());
        }
    }

    /// Stop sampling and fold the readings into `receipt`.
    pub fn finish(&self, cpu: Option<Duration>) -> Receipt {
        // Grab active target & known PIDs for one final synchronous sample before
        // shutting down watches, catching any last-millisecond footprint/energy peak.
        let (target, baseline, known_pids) = {
            let s = self.shared.lock().unwrap();
            (
                s.target,
                s.baseline_children.clone(),
                s.procs.keys().copied().collect::<Vec<_>>(),
            )
        };
        let mut final_samples = Vec::new();
        if let Some((shell, fd)) = target {
            if shell > 0 {
                let mut pids: HashSet<libc::pid_t> = known_pids.into_iter().collect();
                for p in collect_command_pids(shell, fd, &baseline) {
                    pids.insert(p);
                }
                for p in pids {
                    if let Some(r) = rusage(p) {
                        final_samples.push((p, ProcSample::of(&r)));
                    }
                }
            }
        }

        let procs: Vec<ProcSample> = {
            let mut s = self.shared.lock().unwrap();
            for (pid, sample) in final_samples {
                s.procs.entry(pid).and_modify(|cur| cur.merge(sample)).or_insert(sample);
            }
            s.target = None;
            s.baseline_children.clear();
            s.generation = s.generation.wrapping_add(1);
            s.procs.drain().map(|(_, v)| v).collect()
        };
        if let Some(kq) = &self.kq {
            trigger_wake(kq.as_raw_fd());
        }

        let mut receipt = Receipt { cpu, ..Receipt::default() };
        if procs.is_empty() {
            return receipt;
        }
        let cpu_ns: u64 = procs.iter().map(|p| p.cpu_ns).sum();
        let p_ns: u64 = procs.iter().map(|p| p.pcore_ns).sum();
        let energy: u64 = procs.iter().map(|p| p.energy_nj).sum();
        if cpu_ns > 0 {
            receipt.pcore_share = Some((p_ns as f64 / cpu_ns as f64).clamp(0.0, 1.0) as f32);
        }
        if energy > 0 {
            receipt.energy_joules = Some(energy as f64 / 1e9);
        }
        receipt.peak_footprint = procs.iter().map(|p| p.peak).max().filter(|&b| b > 0);
        receipt
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.shutdown = true;
            s.target = None;
            s.generation = s.generation.wrapping_add(1);
        }
        if let Some(kq) = &self.kq {
            trigger_wake(kq.as_raw_fd());
        }
    }
}

fn create_kqueue() -> Option<OwnedFd> {
    // SAFETY: kqueue(2) takes no pointers.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return None;
    }
    // SAFETY: `raw` is a fresh fd we now own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let ev = libc::kevent {
        ident: WAKE_IDENT,
        filter: libc::EVFILT_USER,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let rc = unsafe { libc::kevent(fd.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    (rc >= 0).then_some(fd)
}

fn trigger_wake(kq: libc::c_int) {
    let ev = libc::kevent {
        ident: WAKE_IDENT,
        filter: libc::EVFILT_USER,
        flags: 0,
        fflags: libc::NOTE_TRIGGER,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: valid kevent struct; best-effort wake-up.
    unsafe {
        libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null());
    }
}

fn watch_pid(kq: libc::c_int, watched: &mut HashSet<libc::pid_t>, pid: libc::pid_t) -> bool {
    if pid <= 0 || watched.len() >= MAX_DESCENDANTS || !watched.insert(pid) {
        return false;
    }
    let ev = libc::kevent {
        ident: pid as usize,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        fflags: libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: valid kevent struct.
    let rc = unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if rc < 0 {
        watched.remove(&pid);
        return false;
    }
    true
}

fn clear_watches(kq: libc::c_int, watched: &mut HashSet<libc::pid_t>) {
    for pid in watched.drain() {
        let ev = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_DELETE,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: valid kevent struct; ENOENT if the process already exited is benign.
        unsafe {
            libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null());
        }
    }
}

fn merge_samples(shared: &Mutex<State>, gen: u64, samples: &[(libc::pid_t, ProcSample)]) {
    if samples.is_empty() {
        return;
    }
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    if s.generation != gen || s.target.is_none() {
        return;
    }
    for &(pid, sample) in samples {
        s.procs.entry(pid).and_modify(|cur| cur.merge(sample)).or_insert(sample);
    }
}

fn sample_loop(kq: Arc<OwnedFd>, shared: Arc<Mutex<State>>, on_tick: impl Fn()) {
    let kq_fd = kq.as_raw_fd();
    let mut active_gen: u64 = 0;
    let mut active_target: Option<(libc::pid_t, libc::c_int)> = None;
    let mut baseline: HashSet<libc::pid_t> = HashSet::new();
    let mut watched: HashSet<libc::pid_t> = HashSet::new();
    let mut fast_until: Option<Instant> = None;
    let mut last_tick = Instant::now();
    // SAFETY: `kevent` is a plain C struct; zero-init is valid.
    let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };

    loop {
        let (shutdown, gen, target, new_baseline) = {
            let s = shared.lock().unwrap_or_else(|e| e.into_inner());
            let b = if s.generation != active_gen {
                Some(s.baseline_children.clone())
            } else {
                None
            };
            (s.shutdown, s.generation, s.target, b)
        };
        if shutdown {
            clear_watches(kq_fd, &mut watched);
            return;
        }
        if gen != active_gen {
            clear_watches(kq_fd, &mut watched);
            active_gen = gen;
            active_target = target;
            baseline = new_baseline.unwrap_or_default();
            last_tick = Instant::now();
            fast_until = None;
            if let Some((shell, fd)) = active_target {
                if shell > 0 {
                    fast_until = Some(Instant::now() + FAST_BURST_WINDOW);
                    watch_pid(kq_fd, &mut watched, shell);
                    let pids = collect_command_pids(shell, fd, &baseline);
                    let mut initial = Vec::with_capacity(pids.len());
                    for p in pids {
                        watch_pid(kq_fd, &mut watched, p);
                        if let Some(r) = rusage(p) {
                            initial.push((p, ProcSample::of(&r)));
                        }
                    }
                    merge_samples(&shared, active_gen, &initial);
                }
            }
        }

        let Some((shell, fd)) = active_target else {
            // Idle shell: block indefinitely until `start()` or `Drop` triggers EVFILT_USER.
            let n = unsafe {
                libc::kevent(
                    kq_fd,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    std::ptr::null(),
                )
            };
            if n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                std::thread::sleep(SAMPLE_EVERY);
            }
            continue;
        };

        let wait_dur = if shell <= 0 {
            SAMPLE_EVERY
        } else if fast_until.is_some_and(|deadline| Instant::now() < deadline) {
            FAST_SAMPLE_EVERY
        } else {
            SAMPLE_EVERY
        };
        let ts = libc::timespec {
            tv_sec: wait_dur.as_secs() as libc::time_t,
            tv_nsec: wait_dur.subsec_nanos() as libc::c_long,
        };
        let n = unsafe {
            libc::kevent(
                kq_fd,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                &ts,
            )
        };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                std::thread::sleep(SAMPLE_EVERY);
            }
            continue;
        }

        let mut woke_by_user = false;
        let mut samples: Vec<(libc::pid_t, ProcSample)> = Vec::new();
        let mut saw_fork = false;

        if n > 0 {
            for ev in &events[..n as usize] {
                if ev.filter == libc::EVFILT_USER {
                    woke_by_user = true;
                    continue;
                }
                if ev.filter != libc::EVFILT_PROC {
                    continue;
                }
                let pid = ev.ident as libc::pid_t;
                let ff = ev.fflags;
                if (ff & libc::NOTE_EXIT) != 0 {
                    if pid != shell {
                        if let Some(r) = rusage(pid) {
                            samples.push((pid, ProcSample::of(&r)));
                        }
                    }
                    watched.remove(&pid);
                } else if (ff & libc::NOTE_EXEC) != 0 && pid != shell {
                    if let Some(r) = rusage(pid) {
                        samples.push((pid, ProcSample::of(&r)));
                    }
                }
                if (ff & libc::NOTE_FORK) != 0 {
                    saw_fork = true;
                    fast_until = Some(Instant::now() + FAST_BURST_WINDOW);
                    let kids = list_pids(PROC_PPID_ONLY, pid as u32);
                    for kid in kids {
                        if kid == shell || (pid == shell && baseline.contains(&kid)) {
                            continue;
                        }
                        watch_pid(kq_fd, &mut watched, kid);
                        if let Some(r) = rusage(kid) {
                            samples.push((kid, ProcSample::of(&r)));
                        }
                    }
                }
            }
        }

        // On timer ticks or after a fork, discover any newly spawned children or
        // foreground pgrp members and refresh rusage across active command PIDs.
        if shell > 0 && (n == 0 || saw_fork) {
            let discovered = collect_command_pids(shell, fd, &baseline);
            for p in discovered {
                if watch_pid(kq_fd, &mut watched, p) {
                    fast_until = Some(Instant::now() + FAST_BURST_WINDOW);
                }
                if let Some(r) = rusage(p) {
                    samples.push((p, ProcSample::of(&r)));
                }
            }
            for &p in &watched {
                if p != shell {
                    if let Some(r) = rusage(p) {
                        samples.push((p, ProcSample::of(&r)));
                    }
                }
            }
        }

        merge_samples(&shared, active_gen, &samples);

        if woke_by_user {
            continue;
        }
        if last_tick.elapsed() >= SAMPLE_EVERY {
            let still_running = {
                let s = shared.lock().unwrap();
                s.generation == active_gen && s.target.is_some()
            };
            if still_running {
                last_tick = Instant::now();
                on_tick();
            }
        }
    }
}

/// Foreground process group of the terminal, via `tcgetpgrp` when we hold the
/// PTY master fd or `PROC_PIDTBSDINFO` (`e_tpgid`) when the shell is hosted by
/// local `tachyon server` (`fd < 0`).
fn foreground_pgid(shell: libc::pid_t, fd: libc::c_int) -> Option<libc::pid_t> {
    if fd >= 0 {
        // SAFETY: tcgetpgrp on our own PTY master descriptor.
        let pgid = unsafe { libc::tcgetpgrp(fd) };
        if pgid > 0 && pgid != shell {
            return Some(pgid);
        }
    }
    if shell > 0 && unsafe { libc::getsid(shell) } == shell {
        // Only trust `e_tpgid` when `shell` is the session leader of its own PTY
        // (always true for `forkpty` and local `tachyon server` shells; avoids
        // picking up an outer terminal's foreground group for nested subshells).
        // SAFETY: `proc_bsdinfo` is a plain C struct; zero-init is valid.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                shell,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if got == size {
            let pgid = info.e_tpgid as libc::pid_t;
            if pgid > 0 && pgid != shell {
                return Some(pgid);
            }
        }
    }
    None
}

fn list_pids(kind: u32, info: u32) -> Vec<libc::pid_t> {
    let mut pids = vec![0 as libc::pid_t; MAX_DESCENDANTS];
    // SAFETY: the buffer holds `pids.len()` pid_t values; the call returns the
    // number of bytes written.
    let bytes = unsafe {
        libc::proc_listpids(
            kind,
            info,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    let n = (bytes.max(0) as usize / std::mem::size_of::<libc::pid_t>()).min(pids.len());
    pids.truncate(n);
    pids.retain(|&p| p > 0);
    pids
}

/// Every process belonging to the running command: all members of the terminal's
/// foreground process group plus any new descendants forked by `shell` since
/// `start()` (excluding pre-existing background jobs in `baseline`).
fn collect_command_pids(
    shell: libc::pid_t,
    fd: libc::c_int,
    baseline: &HashSet<libc::pid_t>,
) -> Vec<libc::pid_t> {
    let mut found: HashSet<libc::pid_t> = HashSet::new();
    let mut queue: VecDeque<libc::pid_t> = VecDeque::new();

    if let Some(pgid) = foreground_pgid(shell, fd) {
        for p in list_pids(PROC_PGRP_ONLY, pgid as u32) {
            if p != shell && found.insert(p) {
                queue.push_back(p);
            }
        }
    }
    for kid in list_pids(PROC_PPID_ONLY, shell as u32) {
        if kid != shell && !baseline.contains(&kid) && found.insert(kid) {
            queue.push_back(kid);
        }
    }
    while let Some(parent) = queue.pop_front() {
        if found.len() >= MAX_DESCENDANTS {
            break;
        }
        for kid in list_pids(PROC_PPID_ONLY, parent as u32) {
            if kid != shell && found.insert(kid) {
                queue.push_back(kid);
                if found.len() >= MAX_DESCENDANTS {
                    break;
                }
            }
        }
    }
    found.into_iter().collect()
}

/// `✓ 3m52s · cpu 11m40s (P 88%) · 2.3 Wh · 9.4 GB` for a finished command;
/// `running 12s · 9.4 GB` for one in progress. Short successful commands show
/// nothing, so the screen is not littered with `0.0s`.
pub fn describe(elapsed: Duration, exit: Option<i32>, running: bool, r: Option<&Receipt>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if running {
        // Not for the first two seconds, so quick commands do not flash.
        if elapsed < Duration::from_secs(2) {
            return None;
        }
        parts.push(format!("running {}", duration(elapsed)));
    } else {
        let failed = exit.is_some_and(|e| e != 0);
        if !failed && elapsed < Duration::from_secs(2) {
            return None;
        }
        parts.push(match exit {
            Some(0) | None => format!("✓ {}", duration(elapsed)),
            Some(code) => format!("✗ {code} · {}", duration(elapsed)),
        });
    }
    if let Some(r) = r {
        if let Some(cpu) = r.cpu.filter(|c| *c >= Duration::from_millis(100)) {
            match r.pcore_share {
                Some(p) => parts.push(format!("cpu {} (P {:.0}%)", duration(cpu), p * 100.0)),
                None => parts.push(format!("cpu {}", duration(cpu))),
            }
        }
        if let Some(j) = r.energy_joules.filter(|j| *j >= 0.05) {
            parts.push(energy(j));
        }
        // Memory only once it is worth knowing about.
        if let Some(b) = r.peak_footprint.filter(|b| *b >= 10_000_000) {
            parts.push(bytes(b));
        }
    }
    Some(parts.join(" · "))
}

pub fn duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 60.0 {
        format!("{s:.0}s")
    } else if s < 3600.0 {
        format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64)
    } else {
        format!("{}h{:02}m", (s / 3600.0) as u64, ((s % 3600.0) / 60.0) as u64)
    }
}

fn energy(joules: f64) -> String {
    if joules >= 360.0 {
        format!("{:.1} Wh", joules / 3600.0)
    } else {
        format!("{joules:.1} J")
    }
}

fn bytes(b: u64) -> String {
    let gb = b as f64 / 1e9;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", b as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// The hand-written rusage_info_v6 layout and the time units must match
    /// this kernel: a busy loop of known length shows up as that much CPU, and
    /// the P-core and energy fields hold plausible values.
    #[test]
    fn rusage_v6_layout_and_units() {
        let me = unsafe { libc::getpid() };
        let before = ProcSample::of(&rusage(me).expect("proc_pid_rusage v6"));
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(300) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let after = ProcSample::of(&rusage(me).unwrap());

        let cpu_ms = (after.cpu_ns - before.cpu_ns) as f64 / 1e6;
        assert!((200.0..2000.0).contains(&cpu_ms), "300 ms busy loop measured as {cpu_ms:.0} ms of CPU");
        assert!(after.pcore_ns <= after.cpu_ns + 1_000_000, "P-core time exceeds total CPU time");
        assert!(after.energy_nj > before.energy_nj, "energy did not increase during a busy loop");
        assert!((1 << 20..64u64 << 30).contains(&after.peak), "implausible peak footprint {}", after.peak);
    }

    #[test]
    fn receipts_read_naturally() {
        let r = Receipt {
            cpu: Some(Duration::from_secs(700)),
            pcore_share: Some(0.88),
            energy_joules: Some(8280.0),
            peak_footprint: Some(9_400_000_000),
        };
        assert_eq!(
            describe(Duration::from_secs(232), Some(0), false, Some(&r)).unwrap(),
            "✓ 3m52s · cpu 11m40s (P 88%) · 2.3 Wh · 9.4 GB"
        );
        assert_eq!(describe(Duration::from_millis(300), Some(0), false, None), None);
        assert_eq!(describe(Duration::from_millis(300), Some(1), false, None).unwrap(), "✗ 1 · 0.3s");
        assert_eq!(describe(Duration::from_secs(12), None, true, None).unwrap(), "running 12s");
    }

    #[test]
    fn proc_sample_merge_is_monotonic() {
        let mut a = ProcSample {
            cpu_ns: 100,
            pcore_ns: 80,
            energy_nj: 500,
            peak: 32_000_000,
        };
        let late_partial = ProcSample {
            cpu_ns: 120,
            pcore_ns: 0,
            energy_nj: 0,
            peak: 1_000_000,
        };
        a.merge(late_partial);
        assert_eq!(
            a,
            ProcSample {
                cpu_ns: 120,
                pcore_ns: 80,
                energy_nj: 500,
                peak: 32_000_000,
            }
        );
    }

    #[test]
    fn children_cpu_ignores_non_positive_pid() {
        assert_eq!(children_cpu(0), None);
        assert_eq!(children_cpu(-1), None);
    }

    #[test]
    fn kqueue_sampler_captures_short_sub_500ms_child_without_pty_fd() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        let mut sh = Command::new("/bin/sh")
            .arg("-c")
            .arg(
                "echo ready; read _; \
                 /usr/bin/python3 -c 'a = bytearray(24 * 1024 * 1024); s = sum(range(2_500_000)); print(len(a), s)' >/dev/null; \
                 echo done",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn /bin/sh");

        let sh_pid = sh.id() as libc::pid_t;
        let mut stdin = sh.stdin.take().unwrap();
        let mut reader = BufReader::new(sh.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");

        let sampler = Sampler::spawn(|| {});
        let cpu_before = children_cpu(sh_pid);
        // Pass master_fd = -1 to exercise the local Mux v2 code path (no PTY fd).
        sampler.start(sh_pid, -1);
        // Give the kqueue worker a brief beat to arm EVFILT_PROC on `sh_pid`.
        std::thread::sleep(Duration::from_millis(15));

        let t0 = Instant::now();
        writeln!(stdin, "go").unwrap();
        stdin.flush().unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "done");
        let elapsed = t0.elapsed();

        let cpu = cpu_before
            .zip(children_cpu(sh_pid))
            .map(|(a, b)| b.saturating_sub(a));
        let receipt = sampler.finish(cpu);
        let _ = sh.wait();

        let peak = receipt.peak_footprint.expect("sub-500ms child peak footprint must be captured");
        assert!(
            peak >= 16_000_000,
            "expected >= 16 MB peak footprint for 24 MiB child (ran in {elapsed:?}), got {peak}"
        );
        assert!(
            receipt.energy_joules.is_some_and(|j| j > 0.0),
            "expected positive energy reading for short child (ran in {elapsed:?}), got {receipt:?}"
        );
        assert!(
            receipt.pcore_share.is_some(),
            "expected P-core share for short child (ran in {elapsed:?}), got {receipt:?}"
        );
    }

    #[test]
    fn kqueue_sampler_excludes_preexisting_background_children() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        let mut sh = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 & bg=$!; echo $bg; read _; kill $bg 2>/dev/null; wait $bg 2>/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn /bin/sh");

        let sh_pid = sh.id() as libc::pid_t;
        let mut stdin = sh.stdin.take().unwrap();
        let mut reader = BufReader::new(sh.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let bg_pid: libc::pid_t = line.trim().parse().expect("background pid");

        let sampler = Sampler::spawn(|| {});
        sampler.start(sh_pid, -1);
        {
            let s = sampler.shared.lock().unwrap();
            assert!(
                s.baseline_children.contains(&bg_pid),
                "pre-existing background pid {bg_pid} should be in baseline_children: {:?}",
                s.baseline_children
            );
        }
        std::thread::sleep(Duration::from_millis(60));
        {
            let s = sampler.shared.lock().unwrap();
            assert!(
                !s.procs.contains_key(&bg_pid),
                "pre-existing background pid {bg_pid} must not be sampled as part of the new command"
            );
        }
        let _ = sampler.finish(None);
        let _ = writeln!(stdin, "stop");
        let _ = stdin.flush();
        let _ = sh.wait();
    }
}

/// Whether memory a command allocates on the GPU (PyTorch's MPS backend,
/// MLX, anything through Metal) is in the footprint receipts report. Run with
/// `TACHYON_TORCH_PYTHON=<python with torch> cargo test --release
/// gpu_memory_in_footprint -- --ignored --nocapture`.
#[cfg(test)]
mod gpu_memory {
    use super::*;
    use std::io::{BufRead, BufReader};

    const GIB: u64 = 1 << 30;

    /// macOS's own `footprint` tool's figure for `pid`, in bytes.
    fn footprint_tool(pid: libc::pid_t) -> Option<u64> {
        let out = std::process::Command::new("/usr/bin/footprint").arg(pid.to_string()).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // "...: 2148 MB (phys_footprint)..." style summary line.
        let line = text.lines().find(|l| l.contains("phys_footprint") || l.trim_start().starts_with("Footprint:"))?;
        let mut words = line.split_whitespace().peekable();
        while let Some(w) = words.next() {
            if let Ok(n) = w.replace(',', "").parse::<f64>() {
                let unit = words.peek().copied().unwrap_or("");
                let mult = match unit.to_ascii_uppercase().as_str() {
                    "KB" => 1 << 10,
                    "MB" => 1 << 20,
                    "GB" => 1 << 30,
                    _ => 1,
                };
                return Some((n * mult as f64) as u64);
            }
        }
        None
    }

    #[test]
    #[ignore]
    fn gpu_memory_in_footprint() {
        // A Metal buffer in this process, the way MLX and MPS allocate.
        use objc2_metal::{MTLBuffer, MTLCreateSystemDefaultDevice, MTLDevice, MTLResourceOptions};
        let me = unsafe { libc::getpid() };
        let before = rusage(me).unwrap().v4.0.ri_phys_footprint;
        let device = MTLCreateSystemDefaultDevice().expect("a Metal device");
        let buf = device.newBufferWithLength_options(2 * GIB as usize, MTLResourceOptions::StorageModeShared).expect("2 GiB buffer");
        // Touch every page, as a computation would.
        let p = buf.contents().as_ptr() as *mut u8;
        for i in (0..2 * GIB as usize).step_by(16 * 1024) {
            unsafe { p.add(i).write(1) };
        }
        let after = rusage(me).unwrap().v4.0;
        println!(
            "Metal buffer, 2 GiB: footprint {:.2} -> {:.2} GiB (peak {:.2}); footprint tool {:.2} GiB",
            before as f64 / GIB as f64,
            after.ri_phys_footprint as f64 / GIB as f64,
            after.ri_lifetime_max_phys_footprint as f64 / GIB as f64,
            footprint_tool(me).unwrap_or(0) as f64 / GIB as f64
        );
        assert!(after.ri_lifetime_max_phys_footprint >= before + GIB * 19 / 10, "a touched 2 GiB Metal buffer is in the footprint");
        drop(buf);

        // PyTorch on MPS, in a process of its own like a real run.
        let Some(python) = std::env::var_os("TACHYON_TORCH_PYTHON") else {
            println!("TACHYON_TORCH_PYTHON not set: skipping PyTorch MPS");
            return;
        };
        let script = "import torch,time,sys\nx=torch.ones(512*1024*1024,device='mps')\ny=x*2\ntorch.mps.synchronize()\nprint('ready',torch.mps.current_allocated_memory(),flush=True)\ntime.sleep(4)\n";
        let mut child = std::process::Command::new(python).arg("-c").arg(script).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let allocated: u64 = line.split_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
        let r = rusage(pid).unwrap().v4.0;
        let tool = footprint_tool(pid).unwrap_or(0);
        println!(
            "PyTorch MPS, two 2 GiB tensors ({:.2} GiB allocated by MPS): receipt peak {:.2} GiB, footprint now {:.2} GiB; footprint tool {:.2} GiB",
            allocated as f64 / GIB as f64,
            ProcSample::of(&rusage(pid).unwrap()).peak as f64 / GIB as f64,
            r.ri_phys_footprint as f64 / GIB as f64,
            tool as f64 / GIB as f64
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(r.ri_lifetime_max_phys_footprint >= allocated * 9 / 10, "MPS memory is in the footprint");
    }
}
