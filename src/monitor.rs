//! Finds the WSL2 utility VM and measures it.
//!
//! # Why the process list, and why this API
//!
//! WSL2 runs all distributions inside one lightweight Hyper-V VM whose memory
//! and CPU time Windows accounts to a placeholder process, `vmmemWSL`
//! (`vmmem` on older Windows 10 builds). Its existence is the most reliable
//! "is WSL2 on" signal: `wsl --list --running` reports no running
//! distributions while the VM is still alive and holding memory (the VM
//! lingers for `vmIdleTimeout` after the last distribution exits).
//!
//! The VM process runs as SYSTEM, so `OpenProcess` on it fails for a normal
//! user and `GetProcessTimes` / `GetProcessMemoryInfo` are out.
//! `NtQuerySystemInformation(SystemProcessInformation)` returns the same
//! numbers for every process without opening anything, which is how Task
//! Manager does it too. It is an undocumented-but-stable ntdll export; the
//! function is resolved with `GetProcAddress` so no import library is needed.
//!
//! # What the numbers mean
//!
//! * CPU: the increase of the process's kernel+user time between two
//!   samples, divided by the wall-clock time between them and by the number
//!   of logical cores, in percent. 100 % means every core of the host was busy
//!   with WSL2. It is `None` until a second sample exists.
//! * Memory: the process working set, which for the VM is the memory the VM
//!   has actually touched; this is the "Memory" column Task Manager shows for
//!   `vmmemWSL`. Also given as a percentage of physical RAM.
//!
//! # Cost
//!
//! One snapshot copies every process entry (about 700 KB for 250 processes,
//! ~4 ms). It is taken every `-poll` seconds; the CPU/memory numbers are only
//! recomputed every `-interval` seconds so the tooltip does not flicker.

use std::ffi::c_void;
use std::ptr::null;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, SYSTEMTIME};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemInformation::{
    GetLocalTime, GetSystemDirectoryW, GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX,
    SYSTEM_INFO,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW, INFINITE,
    PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::wide;

/// One observation of the VM, as shown by the UI.
#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    /// The VM process exists.
    pub running: bool,
    /// Process id of the VM (0 when not running). A changed pid means the VM
    /// was restarted, which resets the CPU baseline.
    pub pid: usize,
    /// Percent of all host logical cores; `None` = not measured yet.
    pub cpu: Option<f64>,
    /// Working set of the WSL2 VM process, bytes.
    pub mem: u64,
    /// `mem` as a percentage of the host's physical RAM.
    pub mem_pct: f64,
    /// Local wall-clock time of the last sample (hh, mm, ss), if any.
    pub updated: Option<(u16, u16, u16)>,
}

/// Stateful sampler: remembers the previous CPU reading so the next one can
/// be turned into a rate, and reuses one buffer for the process snapshot.
pub struct Monitor {
    /// Image name to look for, lower-cased with full Unicode rules.
    proc_name: String,
    /// Minimum time between CPU/memory refreshes while the VM runs.
    stats_every: Duration,
    /// Logical cores of the host, the denominator of the CPU percentage.
    ncpu: f64,
    /// Physical RAM in bytes, the denominator of the memory percentage.
    total_mem: f64,

    /// Last published status.
    cur: Status,
    /// When `last_cpu` was read (the CPU baseline).
    last_t: Instant,
    /// Kernel+user time of the VM at `last_t`, in 100 ns units.
    last_cpu: i64,
    /// Pid the baseline belongs to; a different pid invalidates it.
    last_pid: usize,
    /// Snapshot buffer, grown on demand and kept between polls.
    buf: Vec<u8>,
    /// `ntdll!NtQuerySystemInformation`, or `None` if it could not be found
    /// (then the VM is reported as not running).
    nt_query: Option<NtQuerySystemInformationFn>,
}

/// `NTSTATUS NtQuerySystemInformation(SYSTEM_INFORMATION_CLASS, PVOID, ULONG, PULONG)`.
type NtQuerySystemInformationFn = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;

/// `SystemProcessInformation` member of `SYSTEM_INFORMATION_CLASS`.
const SYSTEM_PROCESS_INFORMATION: u32 = 5;
/// Returned when the buffer is too small; `needed` then holds the size.
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004_u32 as i32;

/// Leading part of `SYSTEM_PROCESS_INFORMATION` (x64 layout, from the Windows
/// SDK's `winternl.h` plus the documented "reserved" fields). Entries are
/// chained by `next_entry_offset`; each is followed by its thread array and
/// the image-name string, which is why only the prefix is declared and
/// entries are read with `read_unaligned` at their offsets.
#[repr(C)]
struct SysProcInfo {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name_length: u16,
    image_name_max_length: u16,
    _pad0: [u8; 4],
    image_name: *const u16,
    base_priority: i32,
    _pad1: [u8; 4],
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    _pad2: [u8; 4],
    peak_working_set_size: usize,
    working_set_size: usize,
}

impl Monitor {
    /// Creates a sampler for the process called `proc_name`. Captures the
    /// host's core count and RAM once; both are constant for the session.
    pub fn new(proc_name: &str, stats_every: Duration) -> Self {
        let nt_query = unsafe {
            let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
            GetProcAddress(ntdll, c"NtQuerySystemInformation".as_ptr().cast())
                .map(|f| std::mem::transmute::<_, NtQuerySystemInformationFn>(f))
        };
        Monitor {
            proc_name: proc_name.chars().flat_map(char::to_lowercase).collect(),
            stats_every,
            ncpu: num_cpus() as f64,
            total_mem: total_phys_mem() as f64,
            cur: Status::default(),
            last_t: Instant::now(),
            last_cpu: 0,
            last_pid: 0,
            buf: vec![0; 512 * 1024],
            nt_query,
        }
    }

    /// The status published by the last [`poll`](Self::poll).
    pub fn current(&self) -> Status {
        self.cur
    }

    /// Takes a process snapshot and updates the status.
    ///
    /// Returns the status and whether anything visible changed, so the caller
    /// can skip redrawing. The state machine:
    ///
    /// * VM not found: report stopped (changed only if it was running).
    /// * VM newly found, or a different pid than last time: publish memory
    ///   immediately, remember the CPU time as a baseline, CPU stays `None`.
    /// * VM known: recompute CPU and memory when `force` is set, when
    ///   `stats_every` has elapsed since the baseline, or when there is no
    ///   CPU reading yet (so the first number appears one poll after the VM
    ///   showed up rather than a full interval later). A window shorter than
    ///   one second is too noisy and is skipped.
    pub fn poll(&mut self, force: bool) -> (Status, bool) {
        let now = Instant::now();
        let found = self.find();

        let Some((pid, cpu_time, ws)) = found else {
            let changed = self.cur.running;
            self.cur = Status {
                updated: Some(local_time()),
                ..Status::default()
            };
            self.last_pid = 0;
            return (self.cur, changed);
        };

        // Newly started (or restarted) VM: establish a CPU baseline, show memory now.
        if !self.cur.running || pid != self.last_pid {
            self.cur = Status {
                running: true,
                pid,
                cpu: None,
                mem: ws,
                mem_pct: self.mem_pct(ws),
                updated: Some(local_time()),
            };
            self.last_pid = pid;
            self.last_t = now;
            self.last_cpu = cpu_time;
            return (self.cur, true);
        }

        // Steady state: refresh every stats_every. Right after a baseline (CPU
        // still unknown) the next poll is allowed through so a first reading
        // appears quickly instead of after a full interval.
        let elapsed = now.duration_since(self.last_t);
        if !force && elapsed < self.stats_every && self.cur.cpu.is_some() {
            return (self.cur, false);
        }
        if elapsed >= Duration::from_secs(1) {
            // too short a window gives meaningless numbers
            let delta_ns = (cpu_time - self.last_cpu) as f64 * 100.0; // 100 ns units -> ns
            let pct = delta_ns / elapsed.as_nanos() as f64 / self.ncpu * 100.0;
            self.cur.cpu = Some(pct.max(0.0));
            self.last_t = now;
            self.last_cpu = cpu_time;
        }
        self.cur.mem = ws;
        self.cur.mem_pct = self.mem_pct(ws);
        self.cur.updated = Some(local_time());
        (self.cur, true)
    }

    /// `bytes` as a percentage of physical RAM (0 if RAM could not be read).
    fn mem_pct(&self, bytes: u64) -> f64 {
        if self.total_mem <= 0.0 {
            return 0.0;
        }
        bytes as f64 / self.total_mem * 100.0
    }

    /// Takes a `SystemProcessInformation` snapshot into `self.buf` (growing
    /// it if `STATUS_INFO_LENGTH_MISMATCH` says so) and walks the entries.
    /// Returns `(pid, kernel+user time in 100 ns units, working set bytes)`
    /// of the first process whose image name matches, case-insensitively.
    fn find(&mut self) -> Option<(usize, i64, u64)> {
        let query = self.nt_query?;
        loop {
            let mut needed: u32 = 0;
            let st = unsafe {
                query(
                    SYSTEM_PROCESS_INFORMATION,
                    self.buf.as_mut_ptr().cast(),
                    self.buf.len() as u32,
                    &mut needed,
                )
            };
            if st == 0 {
                break;
            }
            if st != STATUS_INFO_LENGTH_MISMATCH {
                return None;
            }
            self.buf = vec![0; needed as usize + 64 * 1024];
        }

        let mut off = 0usize;
        loop {
            if off + std::mem::size_of::<SysProcInfo>() > self.buf.len() {
                return None;
            }
            // SAFETY: the kernel filled buf with a chain of SYSTEM_PROCESS_INFORMATION
            // entries; each entry is at least size_of::<SysProcInfo>() bytes.
            let p = unsafe {
                std::ptr::read_unaligned(self.buf.as_ptr().add(off) as *const SysProcInfo)
            };
            if !p.image_name.is_null() && p.image_name_length > 0 {
                let n = p.image_name_length as usize / 2;
                // SAFETY: image_name points into buf (the kernel stores the string
                // after the fixed part of the entry), n UTF-16 units long.
                let name = unsafe { std::slice::from_raw_parts(p.image_name, n) };
                if eq_lowercase_utf16(name, &self.proc_name) {
                    return Some((
                        p.unique_process_id,
                        p.user_time + p.kernel_time,
                        p.working_set_size as u64,
                    ));
                }
            }
            if p.next_entry_offset == 0 {
                return None;
            }
            off += p.next_entry_offset as usize;
        }
    }
}

/// Compares a UTF-16 image name against a pre-lowercased `&str`, lowering
/// every character (not just A-Z) the way Go's `strings.ToLower` does, and
/// without allocating per process.
fn eq_lowercase_utf16(name: &[u16], lower: &str) -> bool {
    char::decode_utf16(name.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .flat_map(char::to_lowercase)
        .eq(lower.chars())
}

/// Logical processor count from `GetSystemInfo` (at least 1).
fn num_cpus() -> u32 {
    let mut si: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut si) };
    si.dwNumberOfProcessors.max(1)
}

/// Physical RAM in bytes from `GlobalMemoryStatusEx`, or 0 on failure.
fn total_phys_mem() -> u64 {
    let mut ms: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut ms) } == 0 {
        return 0;
    }
    ms.ullTotalPhys
}

/// Local (hour, minute, second) for the "updated" line of the tooltip.
fn local_time() -> (u16, u16, u16) {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    (t.wHour, t.wMinute, t.wSecond)
}

/// Runs `wsl.exe --shutdown` with no console window, waits for it, and
/// fails with the exit code if it is non-zero.
///
/// Uses `CreateProcessW` directly instead of `std::process::Command`: the
/// latter pulls in about 67 KB of pipe and environment handling for a call
/// whose output is not needed. The executable is given by full path
/// (`%SystemRoot%\System32\wsl.exe`) so that, unlike a bare command line,
/// the current directory is never searched for a `wsl.exe`.
///
/// This is called from a helper thread, not the UI thread, because it blocks
/// for as long as the shutdown takes (a few seconds).
pub fn shutdown_wsl() -> Result<(), String> {
    let mut dir = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(dir.as_mut_ptr(), dir.len() as u32) } as usize;
    if n == 0 || n >= dir.len() {
        return Err(format!(
            "wsl --shutdown: GetSystemDirectory failed ({})",
            unsafe { GetLastError() }
        ));
    }
    let mut exe = dir[..n].to_vec();
    exe.extend(wide(r"\wsl.exe"));
    let mut cmdline = wide("wsl.exe --shutdown"); // CreateProcessW may modify it
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    unsafe {
        let ok = CreateProcessW(
            exe.as_ptr(),
            cmdline.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW,
            null(),
            null(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(format!(
                "wsl --shutdown: CreateProcess failed ({})",
                GetLastError()
            ));
        }
        CloseHandle(pi.hThread);
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        if code != 0 {
            return Err(format!("wsl --shutdown: exit status {code}"));
        }
    }
    Ok(())
}

/// `768 MB` below 1 GiB, otherwise `5.40 GB` (binary units, two decimals).
pub fn format_bytes(b: u64) -> String {
    const MB: u64 = 1 << 20;
    if b < 1024 * MB {
        format!("{} MB", b / MB)
    } else {
        format!("{:.2} GB", b as f64 / (1u64 << 30) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(600 << 20), "600 MB");
        assert_eq!(format_bytes(1024 << 20), "1.00 GB");
        assert_eq!(format_bytes((5 << 30) + (1 << 29)), "5.50 GB");
    }

    #[test]
    fn utf16_compare() {
        let u16s = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert!(eq_lowercase_utf16(&u16s("vmmemWSL"), "vmmemwsl"));
        assert!(!eq_lowercase_utf16(&u16s("vmmem"), "vmmemwsl"));
        assert!(!eq_lowercase_utf16(&u16s("vmmemWSLx"), "vmmemwsl"));
        // Non-ASCII letters fold too, as with Go's strings.ToLower.
        assert!(eq_lowercase_utf16(&u16s("ÄBC.exe"), "äbc.exe"));
        assert!(eq_lowercase_utf16(&u16s("ZOË.EXE"), "zoë.exe"));
        assert!(!eq_lowercase_utf16(&u16s("ABC.exe"), "äbc.exe"));
    }

    /// Exercises the live sampler against whatever is running on this machine.
    /// Uses an always-present process when the WSL2 VM is not running.
    #[test]
    fn poll_live() {
        for name in ["vmmemWSL", "explorer.exe"] {
            let mut m = Monitor::new(name, Duration::from_secs(30));
            let (st, changed) = m.poll(true);
            eprintln!("{name}: first poll -> {st:?} changed={changed}");
            if !st.running {
                continue;
            }
            assert!(st.cpu.is_none(), "CPU must be unknown after baseline");
            std::thread::sleep(Duration::from_millis(1500));
            let (st, changed) = m.poll(true);
            eprintln!("{name}: second poll -> {st:?} changed={changed}");
            assert!(
                st.cpu.is_some(),
                "expected a CPU measurement after forced second poll"
            );
            assert!(changed);
            let (_, changed) = m.poll(false);
            assert!(
                !changed,
                "unforced poll within interval should not report a change"
            );
        }
    }

    /// Cost of one presence check (the work done every `-poll` seconds).
    /// Run with `cargo test --release -- --ignored --nocapture poll_cost`.
    #[test]
    #[ignore]
    fn poll_cost() {
        let mut m = Monitor::new("vmmemWSL", Duration::from_secs(30));
        m.poll(true); // warm up: sizes the buffer
        let n = 500;
        let t = Instant::now();
        for _ in 0..n {
            m.find();
        }
        let per = t.elapsed() / n;
        eprintln!(
            "poll_cost: {} process-list snapshots, {:?} each, buffer {} KB",
            n,
            per,
            m.buf.len() / 1024
        );
    }
}
