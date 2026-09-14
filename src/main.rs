//! WSL Tray: a tiny tray indicator for WSL2 (on/off, CPU and memory share of
//! the host, one-click shutdown). Pure Win32 through `windows-sys`.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod icon;
mod monitor;

use std::cell::{Cell, OnceCell, RefCell};
use std::fs::File;
use std::io::Write as _;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, SYSTEMTIME, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_SZ,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, KillTimer,
    LoadCursorW, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenuEx, TranslateMessage,
    CW_USEDEFAULT, HICON, IDC_ARROW, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION,
    MB_ICONQUESTION, MB_YESNO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_TIMER, WNDCLASSEXW,
};

use icon::Level;
use monitor::{format_bytes, shutdown_wsl, Monitor, Status};

const WM_TRAY_CALLBACK: u32 = WM_APP + 1; // Shell_NotifyIcon callback
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY; // not exported by windows-sys
const WM_REFRESH_NOW: u32 = WM_APP + 2; // posted from the shutdown thread

const TIMER_POLL: usize = 1;

const IDM_STATUS: usize = 1;
const IDM_STATS: usize = 2;
const IDM_SHUTDOWN: usize = 3;
const IDM_REFRESH: usize = 4;
const IDM_AUTOSTART: usize = 5;
const IDM_EXIT: usize = 6;

const CLASS_NAME: &str = "WSLTrayWindow";
const APP_TITLE: &str = "WSL2 Tray";

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WSLTray";

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- command line ----

struct Options {
    poll: Duration,
    stats: Duration,
    process: String,
    log: Option<String>,
    render_test: Option<String>,
}

const USAGE: &str =
    "wsl-tray [--poll 5s] [--interval 30s] [--process vmmemWSL] [--log FILE] [--render-test DIR]

  --poll         how often to check whether WSL2 is running (cheap)
  --interval     how often to refresh CPU/memory while WSL2 is running
  --process      name of the WSL2 VM process
  --log          append diagnostic log lines to this file
  --render-test  write sample icon PNGs to this directory and exit";

/// Go `time.ParseDuration` grammar: "0", or a sequence of `<number><unit>`
/// terms such as "1m30s" or "1.5h" with units ns/us/µs/ms/s/m/h. A unit-less
/// number other than 0 is an error. Negative values (which Go accepts and then
/// feeds into SetTimer, where they only misconfigure the poll timer) are rejected.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.strip_prefix('+').unwrap_or(s);
    if s == "0" {
        return Some(Duration::ZERO);
    }
    if s.is_empty() {
        return None;
    }
    let mut ns = 0.0f64;
    let mut rest = s;
    while !rest.is_empty() {
        let i = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let (num, tail) = rest.split_at(i);
        let j = tail
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(tail.len());
        let (unit, next) = tail.split_at(j);
        let n: f64 = num.parse().ok()?; // "" and "." fail; "1." and ".5" are fine, as in Go
        let scale = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        ns += n * scale;
        rest = next;
    }
    (ns <= i64::MAX as f64).then(|| Duration::from_nanos(ns.round() as u64))
}

/// Parses the command line the way Go's `flag` package does: `-name value`,
/// `-name=value` and `--name...` are all accepted; `--` or the first argument
/// not starting with `-` ends flag parsing and the rest is ignored.
/// `Ok(None)` means `-h`/`-help` was given.
fn parse_args() -> Result<Option<Options>, String> {
    let mut o = Options {
        poll: Duration::from_secs(5),
        stats: Duration::from_secs(30),
        process: "vmmemWSL".into(),
        log: None,
        render_test: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--" || a.len() < 2 || !a.starts_with('-') {
            break;
        }
        let name = a.strip_prefix("--").unwrap_or(&a[1..]);
        if name.starts_with(['-', '=']) {
            return Err(format!("bad flag syntax: {a}\n\n{USAGE}"));
        }
        let (key, mut inline) = match name.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (name, None),
        };
        let mut value = || {
            inline
                .take()
                .or_else(|| it.next())
                .ok_or_else(|| format!("missing value for --{key}\n\n{USAGE}"))
        };
        match key {
            "poll" => o.poll = parse_duration(&value()?).ok_or("bad --poll duration")?,
            "interval" => o.stats = parse_duration(&value()?).ok_or("bad --interval duration")?,
            "process" => o.process = value()?,
            "log" => o.log = Some(value()?),
            "render-test" => o.render_test = Some(value()?),
            "h" | "help" => return Ok(None),
            _ => return Err(format!("unknown flag: {a}\n\n{USAGE}")),
        }
    }
    Ok(Some(o))
}

// ---- diagnostics ----

static LOG: Mutex<Option<File>> = Mutex::new(None);
static LOG_ON: AtomicBool = AtomicBool::new(false);

/// Appends a line to the `--log` file. Without one, the arguments are not
/// even formatted.
macro_rules! log {
    ($($a:tt)*) => {
        if LOG_ON.load(Ordering::Relaxed) {
            log_write(format_args!($($a)*));
        }
    };
}

fn log_write(args: std::fmt::Arguments) {
    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            let t = local_time();
            // Same prefix as Go's default logger (LstdFlags).
            let _ = writeln!(
                f,
                "{:04}/{:02}/{:02} {:02}:{:02}:{:02} {args}",
                t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
            );
        }
    }
}

fn local_time() -> SYSTEMTIME {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    t
}

fn message_box(hwnd: HWND, text: &str, flags: u32) -> i32 {
    unsafe { MessageBoxW(hwnd, wide(text).as_ptr(), wide(APP_TITLE).as_ptr(), flags) }
}

// ---- application state ----
//
// The window procedure is re-entered while TrackPopupMenu/MessageBox pump
// messages, so state lives in Cell/RefCell fields and no borrow is held across
// a Win32 call that can dispatch messages.

struct App {
    hwnd: Cell<HWND>,
    nid: RefCell<NOTIFYICONDATAW>,
    hicon: Cell<HICON>,
    mon: RefCell<Monitor>,
    icon_size: usize,
    shutting_down: Cell<bool>,
    menu_open: Cell<bool>,
    taskbar_created: Cell<u32>,
    last_level: Cell<Option<Level>>,
    promoted: Cell<bool>,
    promote_tries: Cell<u32>,
    exe_path: String,
    launch_args: Vec<String>,
}

thread_local! {
    static APP: OnceCell<App> = const { OnceCell::new() };
}

fn with_app<R>(f: impl FnOnce(&App) -> R) -> Option<R> {
    APP.with(|a| a.get().map(f))
}

fn main() {
    let opts = match parse_args() {
        Ok(Some(o)) => o,
        Ok(None) => {
            message_box(null_mut(), USAGE, MB_ICONINFORMATION);
            return;
        }
        Err(e) => {
            message_box(null_mut(), &e, MB_ICONERROR);
            std::process::exit(2);
        }
    };

    if let Some(dir) = &opts.render_test {
        if let Err(e) = render_test(dir) {
            message_box(null_mut(), &e, MB_ICONERROR);
            std::process::exit(1);
        }
        return;
    }

    if let Some(path) = &opts.log {
        if let Ok(f) = File::options().create(true).append(true).open(path) {
            *LOG.lock().unwrap() = Some(f);
            LOG_ON.store(true, Ordering::Relaxed);
        }
    }

    // Single instance. The mutex is intentionally leaked for the process lifetime.
    unsafe {
        CreateMutexW(null(), 0, wide(r"Local\WSLTray.SingleInstance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
    }

    // DPI awareness comes from the embedded manifest (per-monitor v2), so
    // SM_CXSMICON already reflects the taskbar's scale.
    let icon_size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as usize;

    // Explorer records the icon's ExecutablePath in resolved form (junctions,
    // subst drives and on-disk case), so the promotion match uses the
    // canonical path mapped back to Win32 syntax. The Run value written by
    // "Start with Windows" uses the launch path as-is (see set_autostart).
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok().or(Some(p)))
        .map(|p| win32_path(&p.to_string_lossy()))
        .unwrap_or_default();

    let app = App {
        hwnd: Cell::new(null_mut()),
        nid: RefCell::new(unsafe { std::mem::zeroed() }),
        hicon: Cell::new(null_mut()),
        mon: RefCell::new(Monitor::new(&opts.process, opts.stats)),
        icon_size,
        shutting_down: Cell::new(false),
        menu_open: Cell::new(false),
        taskbar_created: Cell::new(0),
        last_level: Cell::new(None),
        promoted: Cell::new(false),
        promote_tries: Cell::new(0),
        exe_path,
        launch_args: std::env::args().skip(1).collect(),
    };
    APP.with(|slot| {
        if slot.set(app).is_err() {
            unreachable!("app initialised twice");
        }
    });

    if let Err(e) = with_app(|a| a.create_window()).unwrap() {
        message_box(null_mut(), &e, MB_ICONERROR);
        std::process::exit(1);
    }
    with_app(|a| {
        a.tick(true);
        a.add_tray_icon();
        a.promote_once();
        unsafe { SetTimer(a.hwnd.get(), TIMER_POLL, opts.poll.as_millis() as u32, None) };
    });

    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl App {
    fn create_window(&self) -> Result<(), String> {
        unsafe {
            let hinst = GetModuleHandleW(null());
            self.taskbar_created
                .set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));

            let class_name = wide(CLASS_NAME);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: null_mut(),
            };
            if RegisterClassExW(&wc) == 0 {
                return Err(format!("RegisterClassEx failed ({})", GetLastError()));
            }
            // A hidden top-level window: needed to receive tray callbacks and to
            // own the popup menu (message-only windows cannot be brought to
            // the foreground).
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                wide(APP_TITLE).as_ptr(),
                0,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                null_mut(),
                null_mut(),
                hinst,
                null(),
            );
            if hwnd.is_null() {
                return Err(format!("CreateWindowEx failed ({})", GetLastError()));
            }
            self.hwnd.set(hwnd);
            Ok(())
        }
    }

    fn handle(&self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TRAY_CALLBACK => {
                // NOTIFYICON_VERSION_4 semantics: left click -> NIN_SELECT,
                // keyboard -> NIN_KEYSELECT, right click -> WM_CONTEXTMENU. The
                // raw mouse messages also arrive and are deliberately ignored
                // to avoid opening the menu twice.
                match (lparam & 0xFFFF) as u32 {
                    WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT => self.show_menu(),
                    _ => {}
                }
                Some(0)
            }
            WM_TIMER => {
                if wparam == TIMER_POLL {
                    self.tick(false);
                    self.promote_once();
                }
                Some(0)
            }
            WM_REFRESH_NOW => {
                self.shutting_down.set(false);
                self.tick(true);
                Some(0)
            }
            WM_CLOSE => {
                unsafe { DestroyWindow(hwnd) };
                Some(0)
            }
            WM_DESTROY => {
                unsafe {
                    KillTimer(hwnd, TIMER_POLL);
                    Shell_NotifyIconW(NIM_DELETE, &*self.nid.borrow());
                    let h = self.hicon.replace(null_mut());
                    if !h.is_null() {
                        DestroyIcon(h);
                    }
                    PostQuitMessage(0);
                }
                Some(0)
            }
            m if m != 0 && m == self.taskbar_created.get() => {
                self.add_tray_icon(); // Explorer restarted
                Some(0)
            }
            _ => None,
        }
    }

    /// Polls WSL2 and refreshes icon + tooltip when something changed.
    fn tick(&self, force: bool) {
        let (st, changed) = self.mon.borrow_mut().poll(force);
        log!(
            "poll force={force} -> running={} pid={} cpu={:.2} mem={} changed={changed}",
            st.running,
            st.pid,
            st.cpu.unwrap_or(-1.0),
            format_bytes(st.mem)
        );
        if !changed && !self.hicon.get().is_null() {
            return;
        }
        self.update_icon(&st);
    }

    fn update_icon(&self, st: &Status) {
        // Re-render the HICON only when its colour changes.
        let lv = Level::for_status(st);
        if self.last_level.get() != Some(lv) || self.hicon.get().is_null() {
            if let Ok((h, _)) = icon::render_icon(self.icon_size, lv, false) {
                let old = self.hicon.replace(h);
                if !old.is_null() {
                    unsafe { DestroyIcon(old) };
                }
                self.last_level.set(Some(lv));
            }
        }
        let mut nid = self.nid.borrow_mut();
        set_tip(&mut nid, &tooltip(st));
        nid.hIcon = self.hicon.get();
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &*nid) };
    }

    fn add_tray_icon(&self) {
        let mut nid = self.nid.borrow_mut();
        *nid = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd.get();
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY_CALLBACK;
        nid.hIcon = self.hicon.get();
        let st = self.mon.borrow().current();
        set_tip(&mut nid, &tooltip(&st));
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &*nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &*nid);
        }
    }

    fn show_menu(&self) {
        // TrackPopupMenu pumps messages, so guard against re-entry.
        if self.menu_open.replace(true) {
            return;
        }
        let st = self.mon.borrow().current();
        let cmd = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                self.menu_open.set(false);
                return;
            }
            let add = |flags: u32, id: usize, text: &str| {
                AppendMenuW(menu, flags, id, wide(text).as_ptr());
            };
            if st.running {
                add(MF_STRING | MF_GRAYED, IDM_STATUS, "WSL2 is running");
                add(MF_STRING | MF_GRAYED, IDM_STATS, &stats_line(&st));
            } else {
                add(MF_STRING | MF_GRAYED, IDM_STATUS, "WSL2 is stopped");
            }
            add(MF_SEPARATOR, 0, "");
            if self.shutting_down.get() {
                add(MF_STRING | MF_GRAYED, IDM_SHUTDOWN, "Shutting down...");
            } else if st.running {
                add(MF_STRING, IDM_SHUTDOWN, "&Shut down WSL2");
            } else {
                add(MF_STRING | MF_GRAYED, IDM_SHUTDOWN, "&Shut down WSL2");
            }
            add(MF_STRING, IDM_REFRESH, "&Refresh now");
            add(MF_SEPARATOR, 0, "");
            let auto = MF_STRING | if autostart_enabled() { MF_CHECKED } else { 0 };
            add(auto, IDM_AUTOSTART, "Start with &Windows");
            add(MF_SEPARATOR, 0, "");
            add(MF_STRING, IDM_EXIT, "E&xit");

            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let hwnd = self.hwnd.get();
            // Required, otherwise the menu won't dismiss when focus moves away.
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenuEx(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
                pt.x,
                pt.y,
                hwnd,
                null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            cmd as usize
        };

        log!("menu command {cmd}");
        match cmd {
            IDM_SHUTDOWN => self.shutdown(),
            IDM_REFRESH => self.tick(true),
            IDM_AUTOSTART => {
                if let Err(e) = self.set_autostart(!autostart_enabled()) {
                    message_box(self.hwnd.get(), &e, MB_ICONERROR);
                }
            }
            IDM_EXIT => unsafe {
                DestroyWindow(self.hwnd.get());
            },
            _ => {}
        }
        // Released only now: the message boxes shown by the commands above pump
        // messages too, and a tray click during them must not open another menu.
        self.menu_open.set(false);
    }

    fn shutdown(&self) {
        if self.shutting_down.get() {
            return;
        }
        let r = message_box(
            self.hwnd.get(),
            "Shut down WSL2?\n\nAll running distributions will be terminated.",
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        );
        if r != IDYES {
            return;
        }
        self.shutting_down.set(true);
        let hwnd = self.hwnd.get() as isize; // HWND is not Send; it is only used with PostMessageW
        std::thread::spawn(move || {
            if let Err(e) = shutdown_wsl() {
                message_box(null_mut(), &e, MB_ICONERROR);
            }
            unsafe { PostMessageW(hwnd as HWND, WM_REFRESH_NOW, 0, 0) };
        });
    }

    // ---- Windows 11 tray promotion ----

    /// Asks Windows 11 to show the icon next to the clock instead of in the
    /// overflow flyout. Explorer keeps per-icon settings under
    /// `HKCU\Control Panel\NotifyIconSettings\<id>` with ExecutablePath and
    /// IsPromoted. The entry appears shortly after the icon is added, so this
    /// is retried a few times. IsPromoted is only written when absent, so a
    /// later manual choice in Settings > Taskbar is respected.
    fn promote_once(&self) {
        if self.promoted.get() || self.promote_tries.get() > 6 || self.exe_path.is_empty() {
            return;
        }
        self.promote_tries.set(self.promote_tries.get() + 1);
        self.promoted.set(promote_tray_icon(&self.exe_path));
    }

    // ---- autostart (HKCU\...\Run) ----

    fn set_autostart(&self, enable: bool) -> Result<(), String> {
        let key = RegKey::open(HKEY_CURRENT_USER, RUN_KEY, KEY_READ | KEY_WRITE)?;
        if !enable {
            unsafe { RegDeleteValueW(key.0, wide(RUN_VALUE).as_ptr()) };
            return Ok(());
        }
        // The launch path (GetModuleFileName) as-is, like the Go version: a
        // subst drive or junction the user launched through stays in the value.
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut val = format!("\"{}\"", exe.display());
        if !self.launch_args.is_empty() {
            val.push(' ');
            val.push_str(&self.launch_args.join(" "));
        }
        key.set_string(RUN_VALUE, &val)
            .map_err(|r| format!("cannot write Run value ({r})"))
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(Some(r)) = with_app(|a| a.handle(hwnd, msg, wparam, lparam)) {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn tooltip(st: &Status) -> String {
    if !st.running {
        return "WSL2: stopped".into();
    }
    let mut s = format!("WSL2: running\n{}", stats_line(st));
    if let Some((h, m, sec)) = st.updated {
        s.push_str(&format!("\nupdated {h:02}:{m:02}:{sec:02}"));
    }
    s
}

fn stats_line(st: &Status) -> String {
    let cpu = match st.cpu {
        Some(c) => format!("{c:.1} %"),
        None => "...".into(),
    };
    format!(
        "CPU {cpu}   MEM {} ({:.1} %)",
        format_bytes(st.mem),
        st.mem_pct
    )
}

fn set_tip(nid: &mut NOTIFYICONDATAW, s: &str) {
    let mut u: Vec<u16> = s.encode_utf16().collect();
    u.truncate(nid.szTip.len() - 1);
    nid.szTip.fill(0);
    nid.szTip[..u.len()].copy_from_slice(&u);
}

// ---- registry helpers ----

struct RegKey(HKEY);

impl RegKey {
    fn open(root: HKEY, sub: &str, access: u32) -> Result<RegKey, String> {
        let mut h: HKEY = null_mut();
        let r = unsafe { RegOpenKeyExW(root, wide(sub).as_ptr(), 0, access, &mut h) };
        if r != 0 {
            return Err(format!("cannot open {sub} ({r})"));
        }
        Ok(RegKey(h))
    }

    fn read_string(&self, name: &str) -> Option<String> {
        let mut typ = 0u32;
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                null_mut(),
                &mut typ,
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if r != 0 || (typ != REG_SZ && typ != REG_EXPAND_SZ) {
            return None;
        }
        let n = (len as usize / 2).min(buf.len());
        let end = buf[..n].iter().position(|&c| c == 0).unwrap_or(n);
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    fn read_dword(&self, name: &str) -> Option<u32> {
        let mut typ = 0u32;
        let mut v = 0u32;
        let mut len = 4u32;
        let r = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                null_mut(),
                &mut typ,
                (&mut v as *mut u32).cast(),
                &mut len,
            )
        };
        (r == 0 && typ == REG_DWORD).then_some(v)
    }

    fn value_exists(&self, name: &str) -> bool {
        let mut typ = 0u32;
        let mut len = 0u32;
        unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                null_mut(),
                &mut typ,
                null_mut(),
                &mut len,
            ) == 0
        }
    }

    /// Fails with the Win32 error code.
    fn set_string(&self, name: &str, value: &str) -> Result<(), u32> {
        let u = wide(value);
        let r = unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                u.as_ptr().cast(),
                (u.len() * 2) as u32,
            )
        };
        if r != 0 {
            return Err(r);
        }
        Ok(())
    }

    fn set_dword(&self, name: &str, value: u32) {
        unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                REG_DWORD,
                (&value as *const u32).cast(),
                4,
            );
        }
    }

    fn subkey_name(&self, index: u32) -> Option<String> {
        let mut name = [0u16; 256];
        let mut n = name.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                self.0,
                index,
                name.as_mut_ptr(),
                &mut n,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        (r == 0).then(|| String::from_utf16_lossy(&name[..n as usize]))
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn autostart_enabled() -> bool {
    RegKey::open(HKEY_CURRENT_USER, RUN_KEY, KEY_READ)
        .map(|k| k.value_exists(RUN_VALUE))
        .unwrap_or(false)
}

/// Maps the extended-length syntax returned by `canonicalize` back to the
/// Win32 form Explorer records: `\\?\C:\x` -> `C:\x`,
/// `\\?\UNC\server\share\x` -> `\\server\share\x`. Anything else is kept.
fn win32_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match p.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => p.to_string(),
    }
}

/// Returns true once the NotifyIconSettings entry was found and handled.
fn promote_tray_icon(exe: &str) -> bool {
    const BASE: &str = r"Control Panel\NotifyIconSettings";
    let Ok(root) = RegKey::open(HKEY_CURRENT_USER, BASE, KEY_READ) else {
        return true; // no per-icon settings on this Windows version; nothing to do
    };
    let mut i = 0;
    while let Some(sub) = root.subkey_name(i) {
        i += 1;
        let Ok(k) = RegKey::open(
            HKEY_CURRENT_USER,
            &format!(r"{BASE}\{sub}"),
            KEY_READ | KEY_WRITE,
        ) else {
            continue;
        };
        if k.read_string("ExecutablePath")
            .is_some_and(|p| p.eq_ignore_ascii_case(exe))
        {
            if k.read_dword("IsPromoted").is_none() {
                k.set_dword("IsPromoted", 1);
            }
            return true;
        }
    }
    false // not found (yet)
}

// ---- --render-test: dump icons as PNG for a visual check ----

fn render_test(dir: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for sz in [16usize, 20, 24, 32, 48, 256] {
        for (name, lv) in [
            ("off", Level::Off),
            ("ok", Level::Ok),
            ("warn", Level::Warn),
            ("high", Level::High),
        ] {
            let (hicon, pix) = icon::render_icon(sz, lv, true)?;
            unsafe { DestroyIcon(hicon) };
            let png = icon::encode_png(sz, sz, &icon::to_rgba(sz, &pix.unwrap_or_default()));
            let path = std::path::Path::new(dir).join(format!("icon-{sz}-{name}.png"));
            std::fs::write(&path, png).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("1h5m"), Some(Duration::from_secs(3900)));
        assert_eq!(parse_duration("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration(".5s"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("500us"), Some(Duration::from_micros(500)));
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
        assert_eq!(parse_duration("30"), None); // missing unit
        assert_eq!(parse_duration("1m30"), None);
        assert_eq!(parse_duration("-5s"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration(".s"), None);
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("99999999999999999999h"), None); // overflow
    }

    #[test]
    fn win32_paths() {
        assert_eq!(
            win32_path(r"\\?\C:\Tools\wsl-tray.exe"),
            r"C:\Tools\wsl-tray.exe"
        );
        assert_eq!(
            win32_path(r"\\?\UNC\server\share\wsl-tray.exe"),
            r"\\server\share\wsl-tray.exe"
        );
        assert_eq!(
            win32_path(r"C:\Tools\wsl-tray.exe"),
            r"C:\Tools\wsl-tray.exe"
        );
        assert_eq!(win32_path(r"\\?\Volume{1}\x.exe"), r"\\?\Volume{1}\x.exe");
    }

    #[test]
    fn tooltip_fits() {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        set_tip(&mut nid, &"x".repeat(500));
        assert_eq!(nid.szTip[126], 'x' as u16);
        assert_eq!(nid.szTip[127], 0);
        set_tip(&mut nid, "short");
        assert_eq!(nid.szTip[5], 0);
    }

    #[test]
    fn stats_text() {
        let st = Status {
            running: true,
            cpu: Some(12.44),
            mem: 5 << 30,
            mem_pct: 8.44,
            ..Default::default()
        };
        assert_eq!(stats_line(&st), "CPU 12.4 %   MEM 5.00 GB (8.4 %)");
        assert_eq!(
            stats_line(&Status { cpu: None, ..st }),
            "CPU ...   MEM 5.00 GB (8.4 %)"
        );
        assert_eq!(tooltip(&Status::default()), "WSL2: stopped");
    }
}
