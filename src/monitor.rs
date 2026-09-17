//! Finds the WSL2 utility VM and measures it.
//!
//! # Why the process list, and why this API
//!
//! WSL2 runs all distributions inside one lightweight Hyper-V VM whose memory
//! and CPU time Windows accounts to a placeholder process, `vmmemWSL`
//! (`vmmem` on Windows 10). Its existence is the most reliable "is WSL2 on"
//! signal: `wsl --list --running` reports no running distributions while the
//! VM is still alive and holding memory (the VM lingers for `vmIdleTimeout`
//! after the last distribution exits).
//!
//! The VM process runs as SYSTEM, so `OpenProcess` on it fails for a normal
//! user with every access mask, even `SYNCHRONIZE`, and `GetProcessTimes` /
//! `GetProcessMemoryInfo` are out. `NtQuerySystemInformation` returns the
//! same numbers for every process without opening anything, which is how
//! Task Manager does it too. It is an undocumented-but-stable ntdll export;
//! the function is resolved with `GetProcAddress` so no import library is
//! needed. Two of its information classes are used:
//!
//! * `SystemSessionProcessInformation`, the process list of one session.
//!   The VM is created by the WSL service, so it is always in session 0,
//!   and a session-0 list is a tenth of the size and cost of the full one
//!   (services only, no user session). `-process` therefore has to name a
//!   session-0 process, which every VM process is.
//! * `SystemProcessIdInformation`, the name behind one pid: a few hundred
//!   nanoseconds, used to confirm a known VM is still there.
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
//! A session-0 snapshot copies the entries of the service processes (about
//! 260 KB, ~0.6 ms; the full list would be 830 KB and ~7 ms). While the VM
//! is off, that is what every `-poll` does: there is no cheaper unprivileged
//! way to notice a new process. Once the VM is known, the polls in between
//! two `-interval` refreshes only check the name behind its pid, and take a
//! snapshot again only when the pid is gone or has changed hands, or when
//! the CPU/memory numbers are due. The snapshot buffer is allocated for each
//! snapshot with `VirtualAlloc` and released right after, sized from the
//! previous one, so it is not part of the process's memory between polls
//! (the allocation costs under a percent of the snapshot).
//!
//! A session-0 snapshot costs about 2 million cycles when nothing ran in
//! between, but about 8 million after a pause of 100 ms or more: the kernel
//! walks some 130 processes and 2 000+ threads whose structures are then no
//! longer in the caches (`snapshot_cold_cost` measures this). There is no
//! unprivileged event that says "a process appeared", and no query that
//! returns names without the threads. So the caller also tells `poll`
//! when nobody has touched the machine for a while (`relaxed`): then the
//! VM is looked for every [`RELAXED_DISCOVERY`] instead of every poll and
//! the numbers are refreshed every [`RELAXED_STATS`], which nobody sees,
//! while the cheap pid check keeps running so a stop is noticed as before.

use std::ffi::c_void;
use std::ptr::null;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, SYSTEMTIME};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
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
/// be turned into a rate, and how large the last process snapshot was.
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
    /// Size to allocate for the next snapshot: what the last one needed plus
    /// some slack for processes started since.
    snapshot_size: usize,
    /// Snapshots taken so far, see [`snapshots`](Self::snapshots).
    snapshots: u32,
    /// When the last snapshot was taken (`None` before the first), for the
    /// relaxed discovery cadence.
    last_snapshot: Option<Instant>,
    /// `ntdll!NtQuerySystemInformation`, or `None` if it could not be found
    /// (then the VM is reported as not running).
    nt_query: Option<NtQuerySystemInformationFn>,
}

/// `NTSTATUS NtQuerySystemInformation(SYSTEM_INFORMATION_CLASS, PVOID, ULONG, PULONG)`.
type NtQuerySystemInformationFn = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;

/// `SystemProcessInformation` member of `SYSTEM_INFORMATION_CLASS`: the
/// full process list, only used by the `poll_cost` benchmark for comparison.
#[cfg(test)]
const SYSTEM_PROCESS_INFORMATION: u32 = 5;
/// `SystemSessionProcessInformation`: the process list of one session, in
/// the same format, requested through [`SysSessionProcInfo`].
const SYSTEM_SESSION_PROCESS_INFORMATION: u32 = 53;
/// `SystemProcessIdInformation`: the image name of one pid, no snapshot.
const SYSTEM_PROCESS_ID_INFORMATION: u32 = 88;
/// Session of the VM process: it is created by the WSL service, which like
/// every service runs in session 0.
const VM_SESSION: u32 = 0;

/// `SYSTEM_SESSION_PROCESS_INFORMATION`: the input of a session-restricted
/// snapshot. `buffer` receives the same chain of entries as
/// `SystemProcessInformation` would produce.
#[repr(C)]
struct SysSessionProcInfo {
    session_id: u32,
    size_of_buf: u32,
    buffer: *mut c_void,
}
/// Returned when the buffer is too small; `needed` then holds the size.
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004_u32 as i32;
/// Slack added to the size the last snapshot needed, for processes started
/// since; a snapshot that still does not fit is retried with the new size.
const SNAPSHOT_SLACK: usize = 64 * 1024;

/// Cadence of a *relaxed* poll (nobody at the machine, see
/// [`Monitor::poll`]): how often to look for the VM while it is off, and
/// how often to refresh the numbers while it runs (or `-interval` if that
/// is longer). Cheap checks are not affected by relaxing.
pub const RELAXED_DISCOVERY: Duration = Duration::from_secs(30);
pub const RELAXED_STATS: Duration = Duration::from_secs(120);

/// Memory for one snapshot: committed with `VirtualAlloc` and released on
/// drop, so it goes back to the OS at once. A heap allocation of this size
/// (under the heap's 512 KB direct-allocation threshold) would stay
/// committed in the heap's free lists between polls.
struct Pages {
    ptr: *mut u8,
    len: usize,
}

impl Pages {
    fn new(len: usize) -> Option<Pages> {
        let ptr = unsafe { VirtualAlloc(null(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        (!ptr.is_null()).then(|| Pages {
            ptr: ptr.cast(),
            len,
        })
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        unsafe { VirtualFree(self.ptr.cast(), 0, MEM_RELEASE) };
    }
}

/// `UNICODE_STRING`: a counted UTF-16 string, lengths in bytes.
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

/// `SYSTEM_PROCESS_ID_INFORMATION`: `process_id` in, `image_name` out. The
/// caller provides the string buffer. For a normal process the name is the
/// NT path of its image (`\Device\HarddiskVolume3\...\explorer.exe`); for a
/// minimal process such as the WSL2 VM, which has no image file, it is the
/// bare process name (`vmmemWSL`).
#[repr(C)]
struct SysProcIdInfo {
    process_id: usize,
    image_name: UnicodeString,
}

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
            snapshot_size: 256 * 1024,
            snapshots: 0,
            last_snapshot: None,
            nt_query,
        }
    }

    /// The status published by the last [`poll`](Self::poll).
    pub fn current(&self) -> Status {
        self.cur
    }

    /// Number of process-list snapshots taken so far (for the `-log` line
    /// and the tests: a poll that only checked the pid does not add one).
    pub fn snapshots(&self) -> u32 {
        self.snapshots
    }

    /// Looks at the VM and updates the status.
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
    ///   one second is too noisy and is skipped. In between, only the pid is
    ///   checked.
    ///
    /// `relaxed` says nobody is at the machine: then a snapshot is taken at
    /// most every [`RELAXED_DISCOVERY`] while the VM is off and the numbers
    /// are refreshed every [`RELAXED_STATS`] while it runs, so an idle
    /// machine does almost no work for the icon. The pid check is unaffected,
    /// so a stop is still noticed at the next poll. `force` overrides both.
    pub fn poll(&mut self, force: bool, relaxed: bool) -> (Status, bool) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_t);
        let refresh_every = if relaxed {
            self.stats_every.max(RELAXED_STATS)
        } else {
            self.stats_every
        };

        // Fast path: the VM is known and no refresh is due, so a snapshot
        // could only confirm that it still exists. Ask about its pid instead.
        // If that no longer names our process, fall through to the snapshot,
        // which finds a restarted VM or reports this one gone.
        if self.cur.running
            && !force
            && elapsed < refresh_every
            && self.cur.cpu.is_some()
            && self.alive(self.last_pid)
        {
            return (self.cur, false);
        }

        // Relaxed discovery: the VM is off and nobody would see it appear.
        if !self.cur.running && !force && relaxed {
            if let Some(t) = self.last_snapshot {
                if now.duration_since(t) < RELAXED_DISCOVERY {
                    return (self.cur, false);
                }
            }
        }

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
        // appears quickly instead of after a full interval. (Reached with a
        // refresh not due only if the pid check above failed and the snapshot
        // then found the same pid after all.)
        if !force && elapsed < refresh_every && self.cur.cpu.is_some() {
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

    /// Whether `pid` still belongs to a process with the image name being
    /// looked for. `SystemProcessIdInformation` returns that one name without
    /// walking the process list, so this costs a few microseconds against
    /// milliseconds for a snapshot. Also false when the query fails for any
    /// reason (no such pid, or a name longer than the buffer, which no VM
    /// process has); the caller then takes a snapshot, which settles it.
    fn alive(&self, pid: usize) -> bool {
        let Some(query) = self.nt_query else {
            return false;
        };
        let mut name = [0u16; 260];
        let mut info = SysProcIdInfo {
            process_id: pid,
            image_name: UnicodeString {
                length: 0,
                maximum_length: (name.len() * 2) as u16,
                buffer: name.as_mut_ptr(),
            },
        };
        let mut needed: u32 = 0;
        let st = unsafe {
            query(
                SYSTEM_PROCESS_ID_INFORMATION,
                (&mut info as *mut SysProcIdInfo).cast(),
                std::mem::size_of::<SysProcIdInfo>() as u32,
                &mut needed,
            )
        };
        if st != 0 {
            return false; // STATUS_INVALID_CID: no process with that id (any more)
        }
        let n = (info.image_name.length as usize / 2).min(name.len());
        // Compare the last path component only: for a minimal process the
        // name is bare, for others it is the NT path of the image.
        let full = &name[..n];
        let base = full
            .rsplit(|&c| c == u16::from(b'\\'))
            .next()
            .unwrap_or(full);
        eq_lowercase_utf16(base, &self.proc_name)
    }

    /// Takes a snapshot of the session-0 process list and walks the entries.
    /// Returns `(pid, kernel+user time in 100 ns units, working set bytes)`
    /// of the first process whose image name matches, case-insensitively.
    fn find(&mut self) -> Option<(usize, i64, u64)> {
        let buf = self.snapshot()?;
        let mut off = 0usize;
        loop {
            if off + std::mem::size_of::<SysProcInfo>() > buf.len {
                return None;
            }
            // SAFETY: the kernel filled buf with a chain of SYSTEM_PROCESS_INFORMATION
            // entries; each entry is at least size_of::<SysProcInfo>() bytes.
            let p = unsafe { std::ptr::read_unaligned(buf.ptr.add(off) as *const SysProcInfo) };
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

    /// The session-0 process list in a buffer allocated for the call, sized
    /// from the previous one and grown if `STATUS_INFO_LENGTH_MISMATCH` says
    /// so. Also keeps the snapshot bookkeeping.
    fn snapshot(&mut self) -> Option<Pages> {
        let query = self.nt_query?;
        let mut buf = Pages::new(self.snapshot_size)?;
        loop {
            let mut needed: u32 = 0;
            let mut req = SysSessionProcInfo {
                session_id: VM_SESSION,
                size_of_buf: buf.len as u32,
                buffer: buf.ptr.cast(),
            };
            let st = unsafe {
                query(
                    SYSTEM_SESSION_PROCESS_INFORMATION,
                    (&mut req as *mut SysSessionProcInfo).cast(),
                    std::mem::size_of::<SysSessionProcInfo>() as u32,
                    &mut needed,
                )
            };
            if st == 0 {
                break;
            }
            if st != STATUS_INFO_LENGTH_MISMATCH {
                return None;
            }
            // `needed` is the size that would have fitted; doubling as a floor
            // guarantees progress even if it were left at zero.
            let size = (needed as usize + SNAPSHOT_SLACK).max(buf.len * 2);
            buf = Pages::new(size)?;
        }
        self.snapshot_size = buf.len;
        self.snapshots += 1;
        self.last_snapshot = Some(Instant::now());
        Some(buf)
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
    /// Uses an always-present session-0 process when the WSL2 VM is not
    /// running.
    #[test]
    fn poll_live() {
        for name in ["vmmemWSL", "services.exe"] {
            let mut m = Monitor::new(name, Duration::from_secs(30));
            let (st, changed) = m.poll(true, false);
            eprintln!("{name}: first poll -> {st:?} changed={changed}");
            if !st.running {
                continue;
            }
            assert!(st.cpu.is_none(), "CPU must be unknown after baseline");
            std::thread::sleep(Duration::from_millis(1500));
            let (st, changed) = m.poll(true, false);
            eprintln!("{name}: second poll -> {st:?} changed={changed}");
            assert!(
                st.cpu.is_some(),
                "expected a CPU measurement after forced second poll"
            );
            assert!(changed);
            let before = m.snapshots();
            for relaxed in [false, true] {
                let (_, changed) = m.poll(false, relaxed);
                assert!(
                    !changed,
                    "unforced poll within interval should not report a change"
                );
                assert_eq!(
                    m.snapshots(),
                    before,
                    "a poll within the interval must not take a snapshot"
                );
            }
        }
    }

    /// While the VM is off, a relaxed poll looks for it at most every
    /// `RELAXED_DISCOVERY`; a normal or forced poll always does.
    #[test]
    fn relaxed_discovery() {
        let mut m = Monitor::new("no-such-process.exe", Duration::from_secs(30));
        let (st, _) = m.poll(false, true);
        assert!(!st.running);
        assert_eq!(m.snapshots(), 1, "the first poll always looks");
        m.poll(false, true);
        assert_eq!(
            m.snapshots(),
            1,
            "relaxed: no second look within the interval"
        );
        m.poll(false, false);
        assert_eq!(m.snapshots(), 2, "someone is there: every poll looks");
        m.poll(true, true);
        assert_eq!(m.snapshots(), 3, "forced: always");
    }

    /// The pid check behind the fast path: a process that is always there,
    /// a pid that never is, and a pid whose process has another name.
    #[test]
    fn alive_by_pid() {
        let mut m = Monitor::new("services.exe", Duration::from_secs(30));
        let (pid, _, _) = m.find().expect("services.exe is running");
        assert!(m.alive(pid));
        assert!(!m.alive(1 << 40), "no such pid");
        assert!(!m.alive(4), "pid 4 is System, not services.exe");
        assert!(
            !Monitor::new("notepad.exe", Duration::from_secs(30)).alive(pid),
            "right pid, wrong name"
        );
    }

    /// A snapshot that starts with a buffer far too small grows it and
    /// succeeds; the size is remembered for the next one.
    #[test]
    fn snapshot_grows() {
        let mut m = Monitor::new("services.exe", Duration::from_secs(30));
        m.snapshot_size = 64;
        assert!(m.find().is_some());
        assert!(m.snapshot_size > 64);
        let grown = m.snapshot_size;
        assert!(m.find().is_some());
        assert_eq!(m.snapshot_size, grown, "no regrowth once it fits");
        assert_eq!(m.snapshots(), 2);
    }

    /// Cost of a snapshot: what every `-poll` does while the VM is off and
    /// every `-interval` while it runs (session 0, buffer allocated per call
    /// as in `find`), and the full process list for comparison.
    /// `cargo test --release -- --ignored --nocapture poll_cost`.
    #[test]
    #[ignore]
    fn poll_cost() {
        let mut m = Monitor::new("vmmemWSL", Duration::from_secs(30));
        m.poll(true, false); // warm up: sizes the buffer
        let n = 500;
        let t = Instant::now();
        for _ in 0..n {
            m.find();
        }
        let session = t.elapsed() / n;

        let query = m.nt_query.unwrap();
        let mut buf = vec![0u8; 4 << 20];
        let mut needed = 0u32;
        let t = Instant::now();
        for _ in 0..n {
            let st = unsafe {
                query(
                    SYSTEM_PROCESS_INFORMATION,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut needed,
                )
            };
            assert_eq!(st, 0);
        }
        let full = t.elapsed() / n;
        eprintln!(
            "poll_cost: session-0 snapshot {session:?} ({} KB buffer); \
             full process list {full:?} ({} KB) for comparison",
            m.snapshot_size / 1024,
            needed / 1024
        );
    }

    /// Why a snapshot costs what it does: what it contains, and its cost in
    /// CPU cycles depending on how long the thread slept before it (after a
    /// pause the kernel's process and thread structures are no longer in the
    /// caches). `cargo test --release -- --ignored --nocapture snapshot_cold_cost`.
    #[test]
    #[ignore]
    fn snapshot_cold_cost() {
        use windows_sys::Win32::Foundation::{BOOL, HANDLE};
        use windows_sys::Win32::System::Threading::GetCurrentThread;
        type QueryThreadCycleTimeFn = unsafe extern "system" fn(HANDLE, *mut u64) -> BOOL;
        let cycle_time: QueryThreadCycleTimeFn = unsafe {
            let k32 = GetModuleHandleW(wide("kernel32.dll").as_ptr());
            std::mem::transmute(
                GetProcAddress(k32, c"QueryThreadCycleTime".as_ptr().cast()).unwrap(),
            )
        };
        let cycles = || {
            let mut c = 0u64;
            unsafe { cycle_time(GetCurrentThread(), &mut c) };
            c
        };

        let mut m = Monitor::new("vmmemWSL", Duration::from_secs(30));
        let buf = m.snapshot().unwrap();
        let (mut procs, mut threads, mut off) = (0usize, 0usize, 0usize);
        loop {
            let p = unsafe { std::ptr::read_unaligned(buf.ptr.add(off) as *const SysProcInfo) };
            procs += 1;
            threads += p.number_of_threads as usize;
            if p.next_entry_offset == 0 {
                break;
            }
            off += p.next_entry_offset as usize;
        }
        eprintln!(
            "session 0: {procs} processes, {threads} threads, {} KB buffer",
            m.snapshot_size / 1024
        );
        drop(buf);

        for pause_ms in [0u64, 100, 1000, 5000] {
            let n = if pause_ms >= 1000 { 4 } else { 10 };
            let mut total = 0u64;
            for _ in 0..n {
                std::thread::sleep(Duration::from_millis(pause_ms));
                let c0 = cycles();
                m.find();
                total += cycles() - c0;
            }
            eprintln!(
                "after a {pause_ms:>4} ms pause: {:.2} million cycles per snapshot",
                total as f64 / n as f64 / 1e6
            );
        }
    }

    /// Cost of the pid check done by the polls between two refreshes.
    /// `cargo test --release -- --ignored --nocapture check_cost`.
    #[test]
    #[ignore]
    fn check_cost() {
        let mut m = Monitor::new("services.exe", Duration::from_secs(30));
        let (pid, _, _) = m.find().unwrap();
        let n = 20_000;
        let t = Instant::now();
        for _ in 0..n {
            assert!(m.alive(pid));
        }
        eprintln!("check_cost: {n} pid checks, {:?} each", t.elapsed() / n);
    }
}
