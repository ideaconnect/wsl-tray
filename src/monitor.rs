//! Samples the WSL2 utility-VM process ("vmmemWSL") through
//! `NtQuerySystemInformation(SystemProcessInformation)`. That call needs no
//! process handle, so it works for a standard (non-elevated) user even though
//! vmmemWSL runs as SYSTEM (`OpenProcess` on it is denied).

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

/// What the UI shows.
#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    pub running: bool,
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

pub struct Monitor {
    proc_name: String, // lower-cased (Unicode, like Go's strings.ToLower)
    stats_every: Duration,
    ncpu: f64,
    total_mem: f64,

    cur: Status,
    last_t: Instant, // when the CPU baseline was taken
    last_cpu: i64,   // kernel+user time (100 ns units) at last_t
    last_pid: usize,
    buf: Vec<u8>,
    nt_query: Option<NtQuerySystemInformationFn>,
}

type NtQuerySystemInformationFn = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;

const SYSTEM_PROCESS_INFORMATION: u32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004_u32 as i32;

/// SYSTEM_PROCESS_INFORMATION (x64 layout). Only the leading fields are needed.
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

    pub fn current(&self) -> Status {
        self.cur
    }

    /// Checks whether the VM process exists (cheap) and, when it does and the
    /// stats interval has elapsed (or `force` is set), refreshes CPU/memory.
    /// Returns the status and whether anything visible changed.
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

    fn mem_pct(&self, bytes: u64) -> f64 {
        if self.total_mem <= 0.0 {
            return 0.0;
        }
        bytes as f64 / self.total_mem * 100.0
    }

    /// Walks the process list and returns (pid, kernel+user time, working set)
    /// of the first entry whose image name matches (case-insensitive).
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

/// Compares a UTF-16 name against a pre-lowercased one, lowering every
/// character (not just A-Z) the way Go's `strings.ToLower` does.
fn eq_lowercase_utf16(name: &[u16], lower: &str) -> bool {
    char::decode_utf16(name.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .flat_map(char::to_lowercase)
        .eq(lower.chars())
}

fn num_cpus() -> u32 {
    let mut si: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut si) };
    si.dwNumberOfProcessors.max(1)
}

fn total_phys_mem() -> u64 {
    let mut ms: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut ms) } == 0 {
        return 0;
    }
    ms.ullTotalPhys
}

fn local_time() -> (u16, u16, u16) {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    (t.wHour, t.wMinute, t.wSecond)
}

/// Runs `wsl.exe --shutdown` with no console window and waits for it.
/// Direct CreateProcessW instead of std::process::Command: that pulls in
/// ~67 KB of pipe/environment code for a call whose output is not needed.
/// The exe is named explicitly (System32) so, unlike a bare command line,
/// the working directory is never searched for a `wsl.exe`.
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
