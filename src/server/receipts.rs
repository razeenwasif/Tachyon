//! Portable server-side per-command resource receipts (`tachyon server`).
//!
//! Tracks wall duration, user+sys CPU time, P-core share & energy (Apple Silicon),
//! and peak memory footprint for every command running inside a Mux v2 block
//! across macOS (`kqueue` + `proc_pid_rusage`), Linux (`/proc`), and Windows
//! (`Toolhelp32` + `GetProcessTimes` + `GetProcessMemoryInfo`).
//!
//! Spawns its background worker lazily on the first command start and blocks on
//! a `Condvar` when idle (zero idle timer wake-ups).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::mux::model::CommandReceipt;
use crate::term::Receipt;

const FAST_BURST_WINDOW: Duration = Duration::from_millis(500);
const FAST_SAMPLE_EVERY: Duration = Duration::from_millis(25);
const SAMPLE_EVERY: Duration = Duration::from_millis(200);
const MAX_DESCENDANTS: usize = 256;

#[derive(Clone, Copy, Debug, Default)]
struct ProcSample {
    total_ns: u64,
    pcore_ns: u64,
    energy_nj: u64,
    peak_bytes: u64,
}

impl ProcSample {
    fn merge(&mut self, other: Self) {
        self.total_ns = self.total_ns.max(other.total_ns);
        self.pcore_ns = self.pcore_ns.max(other.pcore_ns);
        self.energy_nj = self.energy_nj.max(other.energy_nj);
        self.peak_bytes = self.peak_bytes.max(other.peak_bytes);
    }
}

struct ActiveRun {
    cmd_seq: Option<u32>,
    cmdline: Option<String>,
    started: Instant,
    fast_until: Instant,
    baseline_children_ns: u64,
    baseline_pids: HashSet<u32>,
    samples: HashMap<u32, ProcSample>,
    peak_tick_bytes: u64,
}

struct SharedState {
    root_pid: u32,
    master_fd: i32,
    include_root: bool,
    run_gen: u64,
    worker_spawned: bool,
    shutdown: bool,
    #[cfg(target_os = "macos")]
    kq_fd: i32,
    active: Option<ActiveRun>,
}

/// Lazy, zero-idle-wakeup per-block command receipt sampler.
pub struct ServerSampler {
    shared: Arc<(Mutex<SharedState>, Condvar)>,
}

impl ServerSampler {
    pub fn new(root_pid: u32, master_fd: i32, include_root: bool) -> Self {
        Self {
            shared: Arc::new((
                Mutex::new(SharedState {
                    root_pid,
                    master_fd,
                    include_root,
                    run_gen: 0,
                    worker_spawned: false,
                    shutdown: false,
                    #[cfg(target_os = "macos")]
                    kq_fd: -1,
                    active: None,
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn is_active(&self) -> bool {
        self.shared.0.lock().unwrap().active.is_some()
    }

    /// Begin measuring a command (either shell child `OSC 133;C` or direct command spawn).
    pub fn start(&self, cmd_seq: Option<u32>, cmdline: Option<String>) {
        let (lock, cv) = &*self.shared;
        let (root_pid, master_fd, include_root) = {
            let g = lock.lock().unwrap();
            (g.root_pid, g.master_fd, g.include_root)
        };
        if root_pid == 0 {
            return;
        }
        let baseline_children_ns = children_cpu_ns(root_pid).unwrap_or(0);
        let baseline_pids = if include_root {
            HashSet::new()
        } else {
            direct_children(root_pid).into_iter().collect()
        };
        let initial_pids = collect_command_pids(root_pid, master_fd, include_root, &baseline_pids);
        let mut initial_samples = HashMap::new();
        let mut tick_bytes = 0u64;
        for pid in initial_pids {
            if let Some(s) = sample_pid(pid) {
                tick_bytes = tick_bytes.saturating_add(s.peak_bytes);
                initial_samples.insert(pid, s);
            }
        }
        let now = Instant::now();
        let mut spawn_worker = false;
        #[cfg(target_os = "macos")]
        let wake_kq;
        {
            let mut g = lock.lock().unwrap();
            if g.shutdown {
                return;
            }
            g.run_gen = g.run_gen.wrapping_add(1);
            g.active = Some(ActiveRun {
                cmd_seq,
                cmdline,
                started: now,
                fast_until: now + FAST_BURST_WINDOW,
                baseline_children_ns,
                baseline_pids,
                samples: initial_samples,
                peak_tick_bytes: tick_bytes,
            });
            if !g.worker_spawned {
                g.worker_spawned = true;
                spawn_worker = true;
            }
            #[cfg(target_os = "macos")]
            {
                wake_kq = g.kq_fd;
            }
            cv.notify_all();
        }
        #[cfg(target_os = "macos")]
        if wake_kq >= 0 {
            trigger_kq_user(wake_kq);
        }
        if spawn_worker {
            let shared = Arc::clone(&self.shared);
            let _ = std::thread::Builder::new()
                .name(format!("tachyon-rcpt-{root_pid}"))
                .spawn(move || worker_loop(shared));
        }
    }

    /// Take an immediate synchronous sample of the active process tree (e.g. just
    /// before `wait_exit` reaps a direct child process).
    pub fn sample_now(&self) {
        let (lock, _) = &*self.shared;
        let (root_pid, master_fd, include_root, baseline_pids, is_active) = {
            let g = lock.lock().unwrap();
            match &g.active {
                Some(a) => (
                    g.root_pid,
                    g.master_fd,
                    g.include_root,
                    a.baseline_pids.clone(),
                    true,
                ),
                None => return,
            }
        };
        if !is_active || root_pid == 0 {
            return;
        }
        let pids = collect_command_pids(root_pid, master_fd, include_root, &baseline_pids);
        let mut taken = Vec::with_capacity(pids.len());
        let mut tick_bytes = 0u64;
        for pid in pids {
            if let Some(s) = sample_pid(pid) {
                tick_bytes = tick_bytes.saturating_add(s.peak_bytes);
                taken.push((pid, s));
            }
        }
        let mut g = lock.lock().unwrap();
        if let Some(a) = &mut g.active {
            a.peak_tick_bytes = a.peak_tick_bytes.max(tick_bytes);
            for (pid, s) in taken {
                a.samples.entry(pid).and_modify(|e| e.merge(s)).or_insert(s);
            }
        }
    }

    /// Finish the current active command run (if `expected_seq` is `None` or matches)
    /// and construct its portable `CommandReceipt`.
    pub fn finish(
        &self,
        expected_seq: Option<u32>,
        exit_code: Option<i32>,
        cmdline: Option<String>,
        fallback_wall: Option<Duration>,
    ) -> Option<CommandReceipt> {
        let (lock, cv) = &*self.shared;
        self.sample_now();

        let (root_pid, include_root, run) = {
            let mut g = lock.lock().unwrap();
            let Some(active) = g.active.as_ref() else {
                return fallback_wall.map(|wall| CommandReceipt {
                    cmd_seq: expected_seq,
                    cmdline,
                    exit_code,
                    wall_us: wall.as_micros().min(u64::MAX as u128) as u64,
                    cpu_us: None,
                    pcore_bp: None,
                    energy_mj: None,
                    peak_bytes: None,
                    finished_at: unix_now(),
                });
            };
            if expected_seq.is_some() && active.cmd_seq != expected_seq {
                return fallback_wall.map(|wall| CommandReceipt {
                    cmd_seq: expected_seq,
                    cmdline,
                    exit_code,
                    wall_us: wall.as_micros().min(u64::MAX as u128) as u64,
                    cpu_us: None,
                    pcore_bp: None,
                    energy_mj: None,
                    peak_bytes: None,
                    finished_at: unix_now(),
                });
            }
            let active = g.active.take().unwrap();
            #[cfg(target_os = "macos")]
            let wake_kq = g.kq_fd;
            #[cfg(target_os = "macos")]
            if wake_kq >= 0 {
                trigger_kq_user(wake_kq);
            }
            cv.notify_all();
            (g.root_pid, g.include_root, active)
        };

        let wall = Instant::now().duration_since(run.started);
        let children_delta_ns = if !include_root {
            children_cpu_ns(root_pid)
                .map(|now_ns| now_ns.saturating_sub(run.baseline_children_ns))
                .unwrap_or(0)
        } else {
            0
        };

        let sample_cpu_ns: u64 = run.samples.values().map(|s| s.total_ns).sum();
        let cpu_ns = sample_cpu_ns.max(children_delta_ns);
        let pcore_ns: u64 = run.samples.values().map(|s| s.pcore_ns).sum();
        let energy_nj: u64 = run.samples.values().map(|s| s.energy_nj).sum();
        let max_single_bytes = run.samples.values().map(|s| s.peak_bytes).max().unwrap_or(0);
        let peak_bytes = run.peak_tick_bytes.max(max_single_bytes);

        let receipt = Receipt {
            cpu: (cpu_ns > 0).then(|| Duration::from_nanos(cpu_ns)),
            pcore_share: if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") && sample_cpu_ns > 0 {
                Some((pcore_ns as f64 / sample_cpu_ns as f64).clamp(0.0, 1.0) as f32)
            } else {
                None
            },
            energy_joules: (energy_nj > 0).then(|| energy_nj as f64 / 1e9),
            peak_footprint: (peak_bytes > 0).then_some(peak_bytes),
        };

        Some(CommandReceipt::from_term_receipt(
            run.cmd_seq.or(expected_seq),
            cmdline.or(run.cmdline),
            exit_code,
            wall,
            receipt,
            unix_now(),
        ))
    }
}

impl Drop for ServerSampler {
    fn drop(&mut self) {
        let (lock, cv) = &*self.shared;
        let mut g = lock.lock().unwrap();
        g.shutdown = true;
        g.active = None;
        #[cfg(target_os = "macos")]
        let wake_kq = g.kq_fd;
        cv.notify_all();
        drop(g);
        #[cfg(target_os = "macos")]
        if wake_kq >= 0 {
            trigger_kq_user(wake_kq);
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn worker_loop(shared: Arc<(Mutex<SharedState>, Condvar)>) {
    #[cfg(target_os = "macos")]
    macos::worker(shared);
    #[cfg(not(target_os = "macos"))]
    polling_worker(shared);
}

fn polling_worker(shared: Arc<(Mutex<SharedState>, Condvar)>) {
    let (lock, cv) = &*shared;
    loop {
        let (root_pid, master_fd, include_root, baseline_pids, fast) = {
            let mut g = lock.lock().unwrap();
            while !g.shutdown && g.active.is_none() {
                g = cv.wait(g).unwrap();
            }
            if g.shutdown {
                return;
            }
            let a = g.active.as_ref().unwrap();
            (
                g.root_pid,
                g.master_fd,
                g.include_root,
                a.baseline_pids.clone(),
                Instant::now() < a.fast_until,
            )
        };

        let pids = collect_command_pids(root_pid, master_fd, include_root, &baseline_pids);
        let mut taken = Vec::with_capacity(pids.len());
        let mut tick_bytes = 0u64;
        for pid in pids {
            if let Some(s) = sample_pid(pid) {
                tick_bytes = tick_bytes.saturating_add(s.peak_bytes);
                taken.push((pid, s));
            }
        }

        let mut g = lock.lock().unwrap();
        if let Some(a) = &mut g.active {
            a.peak_tick_bytes = a.peak_tick_bytes.max(tick_bytes);
            for (pid, s) in taken {
                a.samples.entry(pid).and_modify(|e| e.merge(s)).or_insert(s);
            }
            let wait_dur = if fast { FAST_SAMPLE_EVERY } else { SAMPLE_EVERY };
            let _ = cv.wait_timeout(g, wait_dur).unwrap();
        }
    }
}

#[cfg(target_os = "macos")]
fn trigger_kq_user(kq_fd: i32) {
    let ev = libc::kevent {
        ident: 1,
        filter: libc::EVFILT_USER,
        flags: 0,
        fflags: libc::NOTE_TRIGGER,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    unsafe {
        libc::kevent(kq_fd, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null());
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::sync::OnceLock;

    const RUSAGE_INFO_V6: i32 = 6;
    const PROC_PGRP_ONLY: u32 = 2;
    const PROC_PPID_ONLY: u32 = 3;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    #[repr(C)]
    struct RusageInfoV6 {
        ri_uuid: [u8; 16],
        ri_user_time: u64,
        ri_system_time: u64,
        ri_pkg_idle_wkups: u64,
        ri_interrupt_wkups: u64,
        ri_pageins: u64,
        ri_wired_size: u64,
        ri_resident_size: u64,
        ri_phys_footprint: u64,
        ri_proc_start_abstime: u64,
        ri_proc_exit_abstime: u64,
        ri_child_user_time: u64,
        ri_child_system_time: u64,
        ri_child_pkg_idle_wkups: u64,
        ri_child_interrupt_wkups: u64,
        ri_child_pageins: u64,
        ri_child_elapsed_abstime: u64,
        ri_diskio_bytesread: u64,
        ri_diskio_byteswritten: u64,
        ri_cpu_time_qos_default: u64,
        ri_cpu_time_qos_maintenance: u64,
        ri_cpu_time_qos_background: u64,
        ri_cpu_time_qos_utility: u64,
        ri_cpu_time_qos_legacy: u64,
        ri_cpu_time_qos_user_initiated: u64,
        ri_cpu_time_qos_user_interactive: u64,
        ri_billed_system_time: u64,
        ri_serviced_system_time: u64,
        ri_logical_writes: u64,
        ri_lifetime_max_phys_footprint: u64,
        ri_instructions: u64,
        ri_cycles: u64,
        ri_billed_energy: u64,
        ri_serviced_energy: u64,
        ri_interval_max_phys_footprint: u64,
        ri_runnable_time: u64,
        ri_flags: u64,
        ri_user_ptime: u64,
        ri_system_ptime: u64,
        ri_pinstructions: u64,
        ri_pcycles: u64,
        ri_energy_nj: u64,
        ri_penergy_nj: u64,
        ri_secure_time_in_system: u64,
        ri_secure_ptime_in_system: u64,
        ri_neural_footprint: u64,
        ri_lifetime_max_neural_footprint: u64,
        ri_interval_max_neural_footprint: u64,
        ri_conclave_footprint: u64,
        ri_page_wait_time_mach: u64,
        ri_page_cache_hits: u64,
        ri_reserved: [u64; 6],
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
        fn proc_pid_rusage(pid: libc::pid_t, flavor: i32, buffer: *mut RusageInfoV6) -> i32;
    }

    fn timebase() -> (u64, u64) {
        static TB: OnceLock<(u64, u64)> = OnceLock::new();
        *TB.get_or_init(|| {
            let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
            unsafe {
                mach_timebase_info(&mut info);
            }
            (info.numer.max(1) as u64, info.denom.max(1) as u64)
        })
    }

    fn to_nanos(ticks: u64) -> u64 {
        let (n, d) = timebase();
        ((ticks as u128 * n as u128) / d as u128).min(u64::MAX as u128) as u64
    }

    fn rusage(pid: u32) -> Option<RusageInfoV6> {
        if pid == 0 {
            return None;
        }
        let mut info: RusageInfoV6 = unsafe { std::mem::zeroed() };
        let rc = unsafe { proc_pid_rusage(pid as libc::pid_t, RUSAGE_INFO_V6, &mut info) };
        (rc == 0).then_some(info)
    }

    pub fn children_cpu_ns(pid: u32) -> Option<u64> {
        let r = rusage(pid)?;
        Some(to_nanos(r.ri_child_user_time.saturating_add(r.ri_child_system_time)))
    }

    pub fn sample_pid(pid: u32) -> Option<ProcSample> {
        let r = rusage(pid)?;
        let total_ns = to_nanos(r.ri_user_time.saturating_add(r.ri_system_time));
        let pcore_ns = to_nanos(r.ri_user_ptime.saturating_add(r.ri_system_ptime));
        let peak_bytes = r
            .ri_lifetime_max_phys_footprint
            .max(r.ri_phys_footprint)
            .max(r.ri_resident_size);
        Some(ProcSample {
            total_ns,
            pcore_ns,
            energy_nj: r.ri_energy_nj,
            peak_bytes,
        })
    }

    fn list_pids(kind: u32, info: u32) -> Vec<u32> {
        let mut pids = vec![0 as libc::pid_t; MAX_DESCENDANTS];
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
        pids.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect()
    }

    pub fn direct_children(parent_pid: u32) -> Vec<u32> {
        list_pids(PROC_PPID_ONLY, parent_pid)
    }

    fn foreground_pgid(root_pid: u32, fd: i32) -> Option<u32> {
        let shell = root_pid as libc::pid_t;
        if fd >= 0 {
            let pgid = unsafe { libc::tcgetpgrp(fd) };
            if pgid > 0 && pgid != shell {
                return Some(pgid as u32);
            }
        }
        if shell > 0 && unsafe { libc::getsid(shell) } == shell {
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
                    return Some(pgid as u32);
                }
            }
        }
        None
    }

    pub fn collect_command_pids(
        root_pid: u32,
        fd: i32,
        include_root: bool,
        baseline: &HashSet<u32>,
    ) -> Vec<u32> {
        let mut found = HashSet::new();
        let mut queue = VecDeque::new();
        if include_root && root_pid > 0 {
            found.insert(root_pid);
            queue.push_back(root_pid);
        }
        if let Some(pgid) = foreground_pgid(root_pid, fd) {
            for p in list_pids(PROC_PGRP_ONLY, pgid) {
                if (include_root || p != root_pid) && found.insert(p) {
                    queue.push_back(p);
                }
            }
        }
        for kid in list_pids(PROC_PPID_ONLY, root_pid) {
            if kid != root_pid && !baseline.contains(&kid) && found.insert(kid) {
                queue.push_back(kid);
            }
        }
        while let Some(parent) = queue.pop_front() {
            if found.len() >= MAX_DESCENDANTS {
                break;
            }
            for kid in list_pids(PROC_PPID_ONLY, parent) {
                if (include_root || kid != root_pid) && found.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
        found.into_iter().collect()
    }

    fn watch_pid(kq_fd: i32, watched: &mut HashSet<u32>, pid: u32, fflags: u32) -> bool {
        if pid == 0 || !watched.insert(pid) {
            return false;
        }
        let ev = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let rc = unsafe { libc::kevent(kq_fd, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if rc < 0 {
            watched.remove(&pid);
            false
        } else {
            true
        }
    }

    fn unwatch_all(kq_fd: i32, watched: &mut HashSet<u32>) {
        for pid in watched.drain() {
            let ev = libc::kevent {
                ident: pid as usize,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_DELETE,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            unsafe {
                libc::kevent(kq_fd, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null());
            }
        }
    }

    pub fn worker(shared: Arc<(Mutex<SharedState>, Condvar)>) {
        let (lock, cv) = &*shared;
        let kq_fd = unsafe { libc::kqueue() };
        if kq_fd >= 0 {
            let user_ev = libc::kevent {
                ident: 1,
                filter: libc::EVFILT_USER,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            unsafe {
                libc::kevent(kq_fd, &user_ev, 1, std::ptr::null_mut(), 0, std::ptr::null());
            }
            lock.lock().unwrap().kq_fd = kq_fd;
        } else {
            super::polling_worker(shared);
            return;
        }

        let mut watched: HashSet<u32> = HashSet::new();
        let mut active_gen = 0u64;
        let mut events = vec![
            libc::kevent {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            64
        ];

        loop {
            let (root_pid, master_fd, include_root, baseline_pids, wait_dur) = {
                let mut g = lock.lock().unwrap();
                while !g.shutdown && g.active.is_none() {
                    if !watched.is_empty() {
                        unwatch_all(kq_fd, &mut watched);
                    }
                    g = cv.wait(g).unwrap();
                }
                if g.shutdown {
                    break;
                }
                if g.run_gen != active_gen {
                    unwatch_all(kq_fd, &mut watched);
                    active_gen = g.run_gen;
                }
                let a = g.active.as_ref().unwrap();
                let dur = if Instant::now() < a.fast_until {
                    FAST_SAMPLE_EVERY
                } else {
                    SAMPLE_EVERY
                };
                (
                    g.root_pid,
                    g.master_fd,
                    g.include_root,
                    a.baseline_pids.clone(),
                    dur,
                )
            };

            if watched.is_empty() && root_pid > 0 {
                let root_flags = if include_root {
                    libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT
                } else {
                    libc::NOTE_FORK
                };
                watch_pid(kq_fd, &mut watched, root_pid, root_flags);
                for p in collect_command_pids(root_pid, master_fd, include_root, &baseline_pids) {
                    watch_pid(
                        kq_fd,
                        &mut watched,
                        p,
                        libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
                    );
                }
            }

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

            let mut taken: Vec<(u32, ProcSample)> = Vec::new();
            let mut saw_fork = false;
            if n > 0 {
                for ev in &events[..n as usize] {
                    if ev.filter != libc::EVFILT_PROC {
                        continue;
                    }
                    let pid = ev.ident as u32;
                    let ff = ev.fflags;
                    if (ff & libc::NOTE_EXIT) != 0 {
                        if include_root || pid != root_pid {
                            if let Some(s) = sample_pid(pid) {
                                taken.push((pid, s));
                            }
                        }
                        watched.remove(&pid);
                    } else if (include_root || pid != root_pid) && (ff & libc::NOTE_EXEC) != 0 {
                        if let Some(s) = sample_pid(pid) {
                            taken.push((pid, s));
                        }
                    }
                    if (ff & libc::NOTE_FORK) != 0 {
                        saw_fork = true;
                        for kid in direct_children(pid) {
                            if !include_root
                                && (kid == root_pid || (pid == root_pid && baseline_pids.contains(&kid)))
                            {
                                continue;
                            }
                            watch_pid(
                                kq_fd,
                                &mut watched,
                                kid,
                                libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
                            );
                            if let Some(s) = sample_pid(kid) {
                                taken.push((kid, s));
                            }
                        }
                    }
                }
            }

            let mut tick_bytes = 0u64;
            if n == 0 || saw_fork {
                for p in collect_command_pids(root_pid, master_fd, include_root, &baseline_pids) {
                    watch_pid(
                        kq_fd,
                        &mut watched,
                        p,
                        libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
                    );
                    if let Some(s) = sample_pid(p) {
                        tick_bytes = tick_bytes.saturating_add(s.peak_bytes);
                        taken.push((p, s));
                    }
                }
                for &p in &watched {
                    if include_root || p != root_pid {
                        if let Some(s) = sample_pid(p) {
                            taken.push((p, s));
                        }
                    }
                }
            }

            let mut g = lock.lock().unwrap();
            if let Some(a) = &mut g.active {
                if saw_fork {
                    a.fast_until = Instant::now() + FAST_BURST_WINDOW;
                }
                a.peak_tick_bytes = a.peak_tick_bytes.max(tick_bytes);
                for (pid, s) in taken {
                    a.samples.entry(pid).and_modify(|e| e.merge(s)).or_insert(s);
                }
            } else {
                unwatch_all(kq_fd, &mut watched);
            }
        }

        unwatch_all(kq_fd, &mut watched);
        lock.lock().unwrap().kq_fd = -1;
        unsafe {
            libc::close(kq_fd);
        }
    }
}

#[cfg(target_os = "macos")]
use macos::{children_cpu_ns, collect_command_pids, direct_children, sample_pid};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs;
    use std::sync::OnceLock;

    fn clk_tck() -> u64 {
        static TCK: OnceLock<u64> = OnceLock::new();
        *TCK.get_or_init(|| unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64)
    }

    fn ticks_to_ns(ticks: u64) -> u64 {
        let hz = clk_tck();
        ((ticks as u128 * 1_000_000_000u128) / hz as u128).min(u64::MAX as u128) as u64
    }

    pub fn children_cpu_ns(pid: u32) -> Option<u64> {
        let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let parsed = parse_proc_stat(&content)?;
        Some(ticks_to_ns(parsed.cutime.saturating_add(parsed.cstime)))
    }

    pub fn sample_pid(pid: u32) -> Option<ProcSample> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let st = parse_proc_stat(&stat)?;
        let peak_bytes = fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| parse_proc_status_vmhwm(&s))
            .unwrap_or(0);
        Some(ProcSample {
            total_ns: ticks_to_ns(st.utime.saturating_add(st.stime)),
            pcore_ns: 0,
            energy_nj: 0,
            peak_bytes,
        })
    }

    fn scan_proc_table() -> Vec<ProcStat> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for ent in entries.flatten() {
            let name = ent.file_name();
            let Some(s) = name.to_str() else { continue };
            if !s.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if let Ok(stat) = fs::read_to_string(ent.path().join("stat")) {
                if let Some(p) = parse_proc_stat(&stat) {
                    out.push(p);
                }
            }
        }
        out
    }

    pub fn direct_children(parent_pid: u32) -> Vec<u32> {
        scan_proc_table()
            .into_iter()
            .filter(|p| p.ppid == parent_pid && p.pid != parent_pid)
            .map(|p| p.pid)
            .collect()
    }

    pub fn collect_command_pids(
        root_pid: u32,
        fd: i32,
        include_root: bool,
        baseline: &HashSet<u32>,
    ) -> Vec<u32> {
        let table = scan_proc_table();
        let fg_pgid = if fd >= 0 {
            let pg = unsafe { libc::tcgetpgrp(fd) };
            (pg > 0 && pg != root_pid as libc::pid_t).then_some(pg as u32)
        } else {
            None
        };
        let mut by_ppid: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in &table {
            by_ppid.entry(p.ppid).or_default().push(p.pid);
        }
        let mut found = HashSet::new();
        let mut queue = VecDeque::new();
        if include_root && root_pid > 0 {
            found.insert(root_pid);
            queue.push_back(root_pid);
        }
        if let Some(pg) = fg_pgid {
            for p in &table {
                if p.pgrp == pg && (include_root || p.pid != root_pid) && found.insert(p.pid) {
                    queue.push_back(p.pid);
                }
            }
        }
        if let Some(kids) = by_ppid.get(&root_pid) {
            for &kid in kids {
                if kid != root_pid && !baseline.contains(&kid) && found.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
        while let Some(parent) = queue.pop_front() {
            if found.len() >= MAX_DESCENDANTS {
                break;
            }
            if let Some(kids) = by_ppid.get(&parent) {
                for &kid in kids {
                    if (include_root || kid != root_pid) && found.insert(kid) {
                        queue.push_back(kid);
                    }
                }
            }
        }
        found.into_iter().collect()
    }
}

#[cfg(target_os = "linux")]
use linux::{children_cpu_ns, collect_command_pids, direct_children, sample_pid};

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    fn filetime_to_100ns(ft: FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
    }

    pub fn children_cpu_ns(_pid: u32) -> Option<u64> {
        None
    }

    pub fn sample_pid(pid: u32) -> Option<ProcSample> {
        if pid == 0 {
            return None;
        }
        let handle: HANDLE =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let times_ok = unsafe {
            GetProcessTimes(
                handle,
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        }
        .is_ok();
        let mut mem = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        let mem_ok = unsafe {
            GetProcessMemoryInfo(
                handle,
                &mut mem,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        }
        .is_ok();
        unsafe {
            let _ = CloseHandle(handle);
        }
        if !times_ok && !mem_ok {
            return None;
        }
        let total_ns = if times_ok {
            filetime_to_100ns(kernel)
                .saturating_add(filetime_to_100ns(user))
                .saturating_mul(100)
        } else {
            0
        };
        let peak_bytes = if mem_ok { mem.PeakWorkingSetSize as u64 } else { 0 };
        Some(ProcSample {
            total_ns,
            pcore_ns: 0,
            energy_nj: 0,
            peak_bytes,
        })
    }

    fn snapshot_edges() -> Vec<(u32, u32)> {
        let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return Vec::new();
        };
        let mut edges = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if unsafe { Process32FirstW(snap, &mut entry) }.is_ok() {
            loop {
                edges.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if unsafe { Process32NextW(snap, &mut entry) }.is_err() {
                    break;
                }
            }
        }
        unsafe {
            let _ = CloseHandle(snap);
        }
        edges
    }

    pub fn direct_children(parent_pid: u32) -> Vec<u32> {
        snapshot_edges()
            .into_iter()
            .filter(|&(pid, ppid)| ppid == parent_pid && pid != parent_pid)
            .map(|(pid, _)| pid)
            .collect()
    }

    pub fn collect_command_pids(
        root_pid: u32,
        _fd: i32,
        include_root: bool,
        baseline: &HashSet<u32>,
    ) -> Vec<u32> {
        let mut by_ppid: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, ppid) in snapshot_edges() {
            by_ppid.entry(ppid).or_default().push(pid);
        }
        let mut found = HashSet::new();
        let mut queue = VecDeque::new();
        if include_root && root_pid > 0 {
            found.insert(root_pid);
            queue.push_back(root_pid);
        }
        if let Some(kids) = by_ppid.get(&root_pid) {
            for &kid in kids {
                if kid != root_pid && !baseline.contains(&kid) && found.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
        while let Some(parent) = queue.pop_front() {
            if found.len() >= MAX_DESCENDANTS {
                break;
            }
            if let Some(kids) = by_ppid.get(&parent) {
                for &kid in kids {
                    if (include_root || kid != root_pid) && found.insert(kid) {
                        queue.push_back(kid);
                    }
                }
            }
        }
        found.into_iter().collect()
    }
}

#[cfg(target_os = "windows")]
use win::{children_cpu_ns, collect_command_pids, direct_children, sample_pid};

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn children_cpu_ns(_pid: u32) -> Option<u64> {
    None
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn direct_children(_parent_pid: u32) -> Vec<u32> {
    Vec::new()
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn collect_command_pids(_: u32, _: i32, _: bool, _: &HashSet<u32>) -> Vec<u32> {
    Vec::new()
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn sample_pid(_: u32) -> Option<ProcSample> {
    None
}

/// Parsed fields from Linux `/proc/<pid>/stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct ProcStat {
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    pub utime: u64,
    pub stime: u64,
    pub cutime: u64,
    pub cstime: u64,
}

/// Parse `/proc/<pid>/stat`, safely skipping any spaces or parentheses inside `(comm)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_proc_stat(content: &str) -> Option<ProcStat> {
    let rparen = content.rfind(')')?;
    let lparen = content.find('(')?;
    let pid = content[..lparen].trim().parse::<u32>().ok()?;
    let rest: Vec<&str> = content[rparen + 1..].split_whitespace().collect();
    // Fields after ') ': 0=state 1=ppid 2=pgrp ... 11=utime 12=stime 13=cutime 14=cstime
    if rest.len() <= 14 {
        return None;
    }
    Some(ProcStat {
        pid,
        ppid: rest[1].parse().ok()?,
        pgrp: rest[2].parse().ok()?,
        utime: rest[11].parse().ok()?,
        stime: rest[12].parse().ok()?,
        cutime: rest[13].parse::<i64>().ok()?.max(0) as u64,
        cstime: rest[14].parse::<i64>().ok()?.max(0) as u64,
    })
}

/// Parse peak RSS (`VmHWM:` or fallback `VmRSS:`) from Linux `/proc/<pid>/status`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_proc_status_vmhwm(content: &str) -> Option<u64> {
    let mut rss = None;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("VmHWM:") {
            let kb = val.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kb.saturating_mul(1024));
        }
        if let Some(val) = line.strip_prefix("VmRSS:") {
            if let Some(kb) = val.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) {
                rss = Some(kb.saturating_mul(1024));
            }
        }
    }
    rss
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_and_status_parsers_handle_spaces_in_comm_and_vmhwm() {
        let stat = "1234 (Web Content (isolated)) S 100 1234 100 0 -1 4194304 10 0 0 0 42 8 15 5 20 0 1 0 999 0 0";
        let p = parse_proc_stat(stat).expect("valid stat");
        assert_eq!(p.pid, 1234);
        assert_eq!(p.ppid, 100);
        assert_eq!(p.pgrp, 1234);
        assert_eq!(p.utime, 42);
        assert_eq!(p.stime, 8);
        assert_eq!(p.cutime, 15);
        assert_eq!(p.cstime, 5);

        let status = "Name:\tpython3\nVmPeak:\t204800 kB\nVmHWM:\t65536 kB\nVmRSS:\t32768 kB\n";
        assert_eq!(parse_proc_status_vmhwm(status), Some(65536 * 1024));

        let status_no_hwm = "Name:\tsh\nVmRSS:\t4096 kB\n";
        assert_eq!(parse_proc_status_vmhwm(status_no_hwm), Some(4096 * 1024));
    }

    #[cfg(unix)]
    #[test]
    fn server_sampler_captures_short_child_cpu_and_footprint() {
        let mut child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "awk 'BEGIN { s=0; for (i=0; i<900000; i++) s+=i }'",
            ])
            .spawn()
            .expect("spawn sh+awk");
        let pid = child.id();
        let sampler = ServerSampler::new(pid, -1, true);
        sampler.start(Some(1), Some("awk spin".into()));
        let status = loop {
            sampler.sample_now();
            if let Some(st) = child.try_wait().expect("try_wait") {
                break st;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let r = sampler
            .finish(Some(1), status.code(), Some("awk spin".into()), None)
            .expect("receipt");
        assert_eq!(r.exit_code, Some(0));
        assert!(r.wall_us > 0);
        assert!(
            r.cpu_us.is_some_and(|c| c > 0) && r.peak_bytes.is_some_and(|b| b > 0),
            "expected captured CPU and memory footprint, got {r:?}"
        );
    }
}
