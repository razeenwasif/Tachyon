//! The system monitor card: what the Mac is doing, sampled once a second
//! while the card shows. CPU per core (Mach host processor ticks), GPU
//! (IOKit's accelerator statistics), memory as Activity Monitor counts it
//! (app memory, wired and compressed, with pressure and swap), network
//! throughput (interface byte counters), the startup disk, the battery, the
//! thermal state, and the busiest of your processes.

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CStr};
use std::time::Instant;

use objc2_core_foundation::CFString;

use super::receipts::{rusage, to_nanos};

/// One reading of the whole system.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Each core's busy share since the last reading, 0 to 1.
    pub cores: Vec<f64>,
    pub cpu: f64,
    pub load: [f64; 3],
    pub gpu: Option<f64>,
    /// Bytes: used (app + wired + compressed), total, swap used.
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub pressure: Pressure,
    /// Bytes a second, in and out.
    pub net_in: f64,
    pub net_out: f64,
    pub disk_free: u64,
    pub disk_total: u64,
    pub battery: Option<Battery>,
    pub thermal: &'static str,
    /// The busiest processes: name, CPU share (1.0 = one core), footprint.
    pub top: Vec<(String, f64, u64)>,
    /// Recent total CPU, oldest first, for a sparkline.
    pub history: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Pressure {
    #[default]
    Normal,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    pub percent: u32,
    pub charging: bool,
    pub on_ac: bool,
}

/// What a reading needs from the one before it.
pub struct Sampler {
    ticks: Vec<[u64; 4]>,
    net: Option<(u64, u64)>,
    procs: HashMap<i32, u64>,
    at: Option<Instant>,
    history: VecDeque<f64>,
}

impl Default for Sampler {
    fn default() -> Self {
        Sampler { ticks: Vec::new(), net: None, procs: HashMap::new(), at: None, history: VecDeque::new() }
    }
}

const HISTORY: usize = 48;

impl Sampler {
    /// A reading. Rates (CPU, network, processes) cover the time since the
    /// last one; the first reading has none yet.
    pub fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let dt = self.at.map(|a| now.duration_since(a).as_secs_f64()).filter(|d| *d > 0.05);
        self.at = Some(now);
        let mut s = Snapshot::default();

        // CPU, per core.
        let ticks = cpu_ticks();
        if ticks.len() == self.ticks.len() {
            s.cores = ticks
                .iter()
                .zip(&self.ticks)
                .map(|(n, o)| {
                    let d: Vec<u64> = (0..4).map(|i| n[i].saturating_sub(o[i])).collect();
                    let total = d.iter().sum::<u64>();
                    if total == 0 { 0.0 } else { (d[0] + d[1] + d[3]) as f64 / total as f64 }
                })
                .collect();
            s.cpu = if s.cores.is_empty() { 0.0 } else { s.cores.iter().sum::<f64>() / s.cores.len() as f64 };
            self.history.push_back(s.cpu);
            if self.history.len() > HISTORY {
                self.history.pop_front();
            }
        }
        self.ticks = ticks;
        s.history = self.history.iter().copied().collect();
        let mut load = [0.0f64; 3];
        // SAFETY: room for three.
        if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } == 3 {
            s.load = load;
        }

        s.gpu = gpu_utilization();
        (s.mem_used, s.mem_total) = memory();
        s.swap_used = sysctl::<libc::xsw_usage>("vm.swapusage").map_or(0, |x| x.xsu_used);
        s.pressure = match sysctl::<i32>("kern.memorystatus_vm_pressure_level") {
            Some(4) => Pressure::Critical,
            Some(2) => Pressure::Warning,
            _ => Pressure::Normal,
        };

        // Network: bytes in and out over every interface but loopback; the
        // counters are 32-bit and wrap.
        let net = net_bytes();
        if let (Some((pi, po)), Some(dt)) = (self.net, dt) {
            s.net_in = (net.0.wrapping_sub(pi) & 0xFFFF_FFFF) as f64 / dt;
            s.net_out = (net.1.wrapping_sub(po) & 0xFFFF_FFFF) as f64 / dt;
        }
        self.net = Some(net);

        (s.disk_free, s.disk_total) = disk();
        s.battery = battery();
        s.thermal = thermal();
        s.top = self.top_processes(dt);
        s
    }

    /// Your processes using the most CPU since the last reading.
    fn top_processes(&mut self, dt: Option<f64>) -> Vec<(String, f64, u64)> {
        let mut pids = vec![0i32; 4096];
        // SAFETY: the buffer's size in bytes is passed.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
        pids.truncate(n.max(0) as usize);
        let mut seen = HashMap::with_capacity(pids.len());
        let mut out = Vec::new();
        for pid in pids {
            // Only this user's processes can be read; the rest are skipped.
            let Some(r) = rusage(pid) else { continue };
            let cpu = to_nanos(r.v4.0.ri_user_time + r.v4.0.ri_system_time);
            seen.insert(pid, cpu);
            let (Some(dt), Some(&before)) = (dt, self.procs.get(&pid)) else { continue };
            let share = cpu.saturating_sub(before) as f64 / (dt * 1e9);
            if share > 0.001 {
                out.push((pid, share, r.v4.0.ri_phys_footprint));
            }
        }
        self.procs = seen;
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.truncate(6);
        out.into_iter().map(|(pid, share, mem)| (process_name(pid), share, mem)).collect()
    }
}

// libc points Mach calls at the mach2 crate; these are the documented APIs.
#[allow(deprecated)]
fn cpu_ticks() -> Vec<[u64; 4]> {
    let mut count: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut info_count: libc::mach_msg_type_number_t = 0;
    // SAFETY: Mach fills the out-pointers; the array is deallocated below.
    let kr = unsafe {
        libc::host_processor_info(libc::mach_host_self(), libc::PROCESSOR_CPU_LOAD_INFO, &mut count, &mut info, &mut info_count)
    };
    if kr != 0 || info.is_null() {
        return Vec::new();
    }
    // user, system, idle, nice per core.
    let ticks = (0..count as usize)
        .map(|c| std::array::from_fn(|i| unsafe { *info.add(c * 4 + i) } as u32 as u64))
        .collect();
    #[allow(deprecated)]
    unsafe {
        libc::vm_deallocate(libc::mach_task_self(), info as libc::vm_address_t, info_count as usize * 4);
    }
    ticks
}

fn sysctl<T: Copy>(name: &str) -> Option<T> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>();
    // SAFETY: the buffer is sized for T.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), value.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
    (rc == 0 && size == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
}

/// Memory used as Activity Monitor counts it, and the total.
#[allow(deprecated)]
fn memory() -> (u64, u64) {
    let total = sysctl::<u64>("hw.memsize").unwrap_or(0);
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<libc::vm_statistics64>() / std::mem::size_of::<libc::integer_t>()) as libc::mach_msg_type_number_t;
    // SAFETY: the out-buffer is a vm_statistics64 and count says its size.
    let kr = unsafe {
        libc::host_statistics64(libc::mach_host_self(), libc::HOST_VM_INFO64, &mut stats as *mut _ as libc::host_info64_t, &mut count)
    };
    if kr != 0 {
        return (0, total);
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let app = (stats.internal_page_count as u64).saturating_sub(stats.purgeable_count as u64);
    let used = (app + stats.wire_count as u64 + stats.compressor_page_count as u64) * page;
    (used.min(total), total)
}

fn net_bytes() -> (u64, u64) {
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: freed below; each entry is read while the list lives.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return (0, 0);
    }
    let (mut rx, mut tx) = (0u64, 0u64);
    let mut p = ifap;
    while !p.is_null() {
        let ifa = unsafe { &*p };
        let link = !ifa.ifa_addr.is_null() && unsafe { (*ifa.ifa_addr).sa_family } as i32 == libc::AF_LINK;
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_bytes();
        if link && !ifa.ifa_data.is_null() && !name.starts_with(b"lo") {
            let data = unsafe { &*(ifa.ifa_data as *const libc::if_data) };
            rx += data.ifi_ibytes as u64;
            tx += data.ifi_obytes as u64;
        }
        p = ifa.ifa_next;
    }
    unsafe { libc::freeifaddrs(ifap) };
    (rx & 0xFFFF_FFFF_FFFF, tx & 0xFFFF_FFFF_FFFF)
}

/// Free and total bytes on the volume your files are on.
fn disk() -> (u64, u64) {
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let path = c"/System/Volumes/Data";
    // SAFETY: a valid path and out-buffer.
    if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 && unsafe { libc::statfs(c"/".as_ptr(), &mut st) } != 0 {
        return (0, 0);
    }
    let block = st.f_bsize as u64;
    (st.f_bavail * block, st.f_blocks * block)
}

fn process_name(pid: i32) -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer's size is passed.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n > 0 {
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    } else {
        format!("pid {pid}")
    }
}

fn thermal() -> &'static str {
    use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};
    match NSProcessInfo::processInfo().thermalState() {
        NSProcessInfoThermalState::Nominal => "nominal",
        NSProcessInfoThermalState::Fair => "fair",
        NSProcessInfoThermalState::Serious => "serious",
        NSProcessInfoThermalState::Critical => "critical",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// IOKit: the GPU's busy share, and the battery
// ---------------------------------------------------------------------------

type CFTypeRef = *const c_void;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, existing: *mut u32) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: CFTypeRef, allocator: CFTypeRef, options: u32) -> CFTypeRef;
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFTypeRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRef, source: CFTypeRef) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, out: *mut c_void) -> bool;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> bool;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
    fn CFGetTypeID(object: CFTypeRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
}

const NUMBER_SINT64: isize = 4;

fn key(name: &str) -> objc2_core_foundation::CFRetained<CFString> {
    CFString::from_str(name)
}

/// A number in a CF dictionary.
unsafe fn dict_number(dict: CFTypeRef, name: &str) -> Option<i64> {
    let k = key(name);
    let v = CFDictionaryGetValue(dict, (&*k as *const CFString).cast());
    if v.is_null() || CFGetTypeID(v) != CFNumberGetTypeID() {
        return None;
    }
    let mut out = 0i64;
    CFNumberGetValue(v, NUMBER_SINT64, (&mut out as *mut i64).cast()).then_some(out)
}

/// The GPU's busy share, from its accelerator's performance statistics.
fn gpu_utilization() -> Option<f64> {
    // SAFETY: IOKit and CF calls with their documented ownership: matching
    // dictionaries are consumed, properties are released, objects freed.
    unsafe {
        let matching = IOServiceMatching(c"IOAccelerator".as_ptr());
        if matching.is_null() {
            return None;
        }
        let mut iter = 0u32;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return None;
        }
        let mut best = None;
        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 {
                break;
            }
            let k = key("PerformanceStatistics");
            let stats = IORegistryEntryCreateCFProperty(entry, (&*k as *const CFString).cast(), std::ptr::null(), 0);
            if !stats.is_null() {
                if let Some(u) = dict_number(stats, "Device Utilization %") {
                    best = Some(best.map_or(u, |b: i64| b.max(u)));
                }
                CFRelease(stats);
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iter);
        best.map(|u| (u as f64 / 100.0).clamp(0.0, 1.0))
    }
}

/// The internal battery, if there is one.
fn battery() -> Option<Battery> {
    // SAFETY: the info blob and list are copies, released here; descriptions
    // are borrowed from the blob.
    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return None;
        }
        let list = IOPSCopyPowerSourcesList(blob);
        let mut found = None;
        if !list.is_null() {
            for i in 0..CFArrayGetCount(list) {
                let desc = IOPSGetPowerSourceDescription(blob, CFArrayGetValueAtIndex(list, i));
                if desc.is_null() {
                    continue;
                }
                let (Some(cur), Some(max)) = (dict_number(desc, "Current Capacity"), dict_number(desc, "Max Capacity")) else { continue };
                let charging = {
                    let k = key("Is Charging");
                    let v = CFDictionaryGetValue(desc, (&*k as *const CFString).cast());
                    !v.is_null() && CFBooleanGetValue(v)
                };
                let on_ac = {
                    let k = key("Power Source State");
                    let v = CFDictionaryGetValue(desc, (&*k as *const CFString).cast());
                    !v.is_null() && CFGetTypeID(v) == CFStringGetTypeID() && (*(v as *const CFString)).to_string() == "AC Power"
                };
                found = Some(Battery { percent: ((cur * 100) / max.max(1)).clamp(0, 100) as u32, charging, on_ac });
                break;
            }
            CFRelease(list);
        }
        CFRelease(blob);
        found
    }
}

// ---------------------------------------------------------------------------
// The card's text
// ---------------------------------------------------------------------------

/// A bar `width` cells wide, `frac` of it filled.
fn bar(frac: f64, width: usize) -> String {
    let filled = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Values 0 to 1 as block heights.
fn spark(values: &[f64]) -> String {
    const B: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    values.iter().map(|v| B[((v.clamp(0.0, 1.0) * 7.0).round()) as usize]).collect()
}

fn bytes(n: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n;
    let mut u = 0;
    while v >= 1000.0 && u + 1 < units.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{v:.0} {}", units[u]) } else if v < 10.0 { format!("{v:.1} {}", units[u]) } else { format!("{v:.0} {}", units[u]) }
}

/// The card's text for a reading.
pub fn render(s: &Snapshot) -> String {
    let pct = |f: f64| format!("{:>3.0}%", f * 100.0);
    let mut out = Vec::new();
    out.push(format!("CPU     {}  {}   load {:.2} {:.2} {:.2}", pct(s.cpu), bar(s.cpu, 14), s.load[0], s.load[1], s.load[2]));
    if !s.cores.is_empty() {
        out.push(format!("  cores {}", spark(&s.cores)));
    }
    if s.history.len() > 1 {
        out.push(format!("  last  {}", spark(&s.history)));
    }
    if let Some(g) = s.gpu {
        out.push(format!("GPU     {}  {}", pct(g), bar(g, 14)));
    }
    let mem = if s.mem_total > 0 { s.mem_used as f64 / s.mem_total as f64 } else { 0.0 };
    out.push(format!(
        "Memory  {}  {}   {} of {}",
        pct(mem),
        bar(mem, 14),
        bytes(s.mem_used as f64),
        bytes(s.mem_total as f64)
    ));
    let pressure = match s.pressure {
        Pressure::Normal => "normal",
        Pressure::Warning => "warning",
        Pressure::Critical => "critical",
    };
    out.push(format!("  pressure {pressure} · swap {}", bytes(s.swap_used as f64)));
    out.push(format!("Network ↓ {}/s   ↑ {}/s", bytes(s.net_in), bytes(s.net_out)));
    if s.disk_total > 0 {
        out.push(format!("Disk    {} free of {}", bytes(s.disk_free as f64), bytes(s.disk_total as f64)));
    }
    let power = match s.battery {
        Some(b) => format!("Battery {}%{}", b.percent, if b.charging { ", charging" } else if b.on_ac { ", on power" } else { "" }),
        None => "Power   on AC".to_string(),
    };
    out.push(format!("{power} · thermal {}", s.thermal));
    if !s.top.is_empty() {
        out.push(String::new());
        out.push("Busiest of yours        CPU   memory".into());
        for (name, share, mem) in &s.top {
            let name: String = name.chars().take(20).collect();
            out.push(format!("  {name:<20} {:>5.0}%  {:>7}", share * 100.0, bytes(*mem as f64)));
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_mac_reads_sensibly() {
        let mut s = Sampler::default();
        let first = s.sample();
        assert!(first.mem_total > 1 << 30, "a gigabyte at least: {}", first.mem_total);
        assert!(first.mem_used > 0 && first.mem_used <= first.mem_total);
        assert!(first.disk_total > 0 && first.disk_free <= first.disk_total);
        // Busy for a moment, so the second reading has rates.
        let t = Instant::now();
        let mut x = 0u64;
        while t.elapsed().as_millis() < 300 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let second = s.sample();
        assert!(!second.cores.is_empty(), "cores");
        assert!(second.cores.iter().all(|c| (0.0..=1.0).contains(c)));
        assert!(second.cpu > 0.0);
        assert!(second.top.iter().any(|(_, share, _)| *share > 0.2), "this test spun a core: {:?}", second.top);
        let text = render(&second);
        assert!(text.starts_with("CPU") && text.contains("Memory") && text.contains("Network"), "{text}");
    }

    #[test]
    fn bars_sparks_and_sizes() {
        assert_eq!(bar(0.5, 4), "██░░");
        assert_eq!(bar(1.5, 2), "██");
        assert_eq!(spark(&[0.0, 0.5, 1.0]), "▁▅█");
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KB");
        assert_eq!(bytes(36.0 * 1024.0 * 1024.0 * 1024.0), "36 GB");
    }

    /// Prints the card's text from two readings a second apart.
    #[test]
    #[ignore]
    fn show() {
        let mut s = Sampler::default();
        s.sample();
        std::thread::sleep(std::time::Duration::from_secs(1));
        println!("{}", render(&s.sample()));
    }
}
