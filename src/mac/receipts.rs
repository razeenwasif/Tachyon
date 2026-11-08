//! What a command cost: CPU time, how much of it ran on performance cores,
//! energy, and peak memory.
//!
//! CPU time is exact. The shell is the parent of every command it runs, and the
//! kernel adds a child's CPU time to its parent's "children" totals when the
//! parent reaps it -- so the difference in the shell's child CPU time between
//! the command starting (OSC 133;C) and finishing (OSC 133;D) is the command's
//! CPU time, including everything it spawned and waited for.
//!
//! The rest is sampled. While a command runs, a sampler thread reads
//! `proc_pid_rusage` for every process in the terminal's foreground process
//! group twice a second: P-core time, energy and lifetime peak footprint. The
//! last sample of each process is what counts, so a command shorter than the
//! sampling interval gets an exact CPU time and nothing else, and a child that
//! starts and exits between samples is missed. On unified memory the footprint
//! is the number that decides whether a model fits.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::term::Receipt;

/// `proc_pid_rusage` flavour with P-core time and energy (macOS 12+).
const RUSAGE_INFO_V6: libc::c_int = 6;
const SAMPLE_EVERY: Duration = Duration::from_millis(500);
/// `PROC_PGRP_ONLY` from `<libproc.h>`: list the pids of one process group.
const PROC_PGRP_ONLY: u32 = 2;

/// `struct rusage_info_v6` from `<sys/resource.h>`: v4, which `libc` defines,
/// followed by the fields added in v5 and v6.
#[repr(C)]
#[derive(Default)]
struct RusageV6 {
    v4: RusageV4,
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
struct RusageV4(libc::rusage_info_v4);

impl Default for RusageV4 {
    fn default() -> Self {
        // SAFETY: a plain struct of integers; all-zero is a valid value.
        RusageV4(unsafe { std::mem::zeroed() })
    }
}

fn rusage(pid: libc::pid_t) -> Option<RusageV6> {
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
fn to_nanos(ticks: u64) -> u64 {
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
    let r = rusage(shell)?;
    Some(Duration::from_nanos(to_nanos(r.v4.0.ri_child_user_time + r.v4.0.ri_child_system_time)))
}

/// The latest reading for one process of a running command.
#[derive(Clone, Copy, Default)]
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
}

#[derive(Default)]
struct State {
    /// Sampling a command: (shell pid, PTY master fd).
    target: Option<(libc::pid_t, libc::c_int)>,
    procs: HashMap<libc::pid_t, ProcSample>,
    shutdown: bool,
}

/// One sampler thread per window. It sleeps on a condition variable while no
/// command runs, so an idle shell costs no wake-ups.
pub struct Sampler {
    shared: Arc<(Mutex<State>, Condvar)>,
}

impl Sampler {
    /// `on_tick` runs after each sample while a command runs, so the window
    /// can refresh its "running 12s" readout.
    pub fn spawn(on_tick: impl Fn() + Send + 'static) -> Sampler {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("tachyon-receipts".into())
            .spawn(move || sample_loop(worker, on_tick));
        Sampler { shared }
    }

    pub fn start(&self, shell: libc::pid_t, master_fd: libc::c_int) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.procs.clear();
        s.target = Some((shell, master_fd));
        cv.notify_all();
    }

    /// Stop sampling and fold the readings into `receipt`.
    pub fn finish(&self, cpu: Option<Duration>) -> Receipt {
        let (lock, _) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.target = None;
        let procs: Vec<ProcSample> = s.procs.drain().map(|(_, v)| v).collect();
        drop(s);

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
        receipt.peak_footprint = procs.iter().map(|p| p.peak).max();
        receipt
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        let (lock, cv) = &*self.shared;
        lock.lock().unwrap().shutdown = true;
        cv.notify_all();
    }
}

fn sample_loop(shared: Arc<(Mutex<State>, Condvar)>, on_tick: impl Fn()) {
    let (lock, cv) = &*shared;
    let mut s = lock.lock().unwrap();
    loop {
        if s.shutdown {
            return;
        }
        let Some((shell, fd)) = s.target else {
            s = cv.wait(s).unwrap();
            continue;
        };
        drop(s);
        let readings = sample_foreground(shell, fd);
        s = lock.lock().unwrap();
        if s.target.is_some() {
            for (pid, r) in readings {
                s.procs.insert(pid, r);
            }
            drop(s);
            on_tick();
            s = lock.lock().unwrap();
        }
        s = cv.wait_timeout(s, SAMPLE_EVERY).unwrap().0;
    }
}

/// Every process in the terminal's foreground process group, unless that is
/// the shell itself (the command has not started, or has already finished).
fn sample_foreground(shell: libc::pid_t, fd: libc::c_int) -> Vec<(libc::pid_t, ProcSample)> {
    // SAFETY: tcgetpgrp on our own PTY master descriptor.
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    if pgid <= 0 || pgid == shell {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; 256];
    // SAFETY: the buffer holds `pids.len()` pid_t values; the call returns the
    // number of bytes written.
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            pgid as u32,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    let n = (bytes.max(0) as usize / std::mem::size_of::<libc::pid_t>()).min(pids.len());
    pids[..n]
        .iter()
        .filter(|&&p| p > 0)
        .filter_map(|&p| rusage(p).map(|r| (p, ProcSample::of(&r))))
        .collect()
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

fn duration(d: Duration) -> String {
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
}
