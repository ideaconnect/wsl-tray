//! wsl-tray: a tray indicator for WSL2 (on/off, CPU and memory share of the
//! host, shutdown from the menu). Pure Win32 through `windows-sys`.
//!
//! # How the program is put together
//!
//! ```text
//!  main()            parse flags, single-instance mutex, create App
//!    |
//!    +-- create_window()      hidden top-level window; owns the tray icon,
//!    |                        receives its callbacks and the poll timer
//!    +-- add_tray_icon()      Shell_NotifyIconW(NIM_ADD), version 4
//!    +-- SetCoalescableTimer  WM_TIMER every -poll (7.5 s), up to 1 s late
//!    +-- message loop         GetMessageW / DispatchMessageW until WM_QUIT
//!
//!  wnd_proc -> App::handle
//!    WM_TIMER          -> tick(): Monitor::poll(), redraw icon/tooltip if changed;
//!                         relaxed (fewer snapshots) after 2 min without input
//!    WM_TRAY_CALLBACK  -> show_menu() on click / keyboard select / context menu
//!                         (after a fresh sample), or bring the open settings
//!                         dialog to the front
//!    WM_REFRESH_NOW    -> tick(true), posted by the shutdown thread when done
//!    TaskbarCreated    -> add_tray_icon() again after an Explorer restart
//!    WM_DESTROY        -> remove the icon, PostQuitMessage
//!
//!  show_menu -> "Settings..." -> settings::edit() (modal dialog), then
//!               Settings::save() and update_icon() with the new thresholds
//! ```
//!
//! The modules split as follows:
//!
//! * [`monitor`] finds the WSL2 VM process and computes CPU / memory numbers.
//!   It knows nothing about the UI.
//! * [`icon`] turns a [`icon::Level`] (off / ok / warn / high) into an `HICON`
//!   from the embedded Tux mask, and has the PNG writer used by `-render-test`.
//! * [`settings`] holds the two colour thresholds, keeps them per user in the
//!   registry (`HKCU\Software\IDCT\wsl-tray`) and owns the settings dialog.
//! * this file: command line, window, tray icon, menu, registry (autostart and
//!   the Windows 11 "show next to the clock" flag), diagnostics log.
//!
//! # Threading
//!
//! Everything runs on the main thread, which is also the UI thread. The one
//! exception is the thread that waits for `wsl --shutdown`; it talks back only
//! through `PostMessageW(WM_REFRESH_NOW)`, which is thread-safe.
//!
//! # Re-entrancy
//!
//! `TrackPopupMenuEx`, `MessageBoxW`, `DialogBoxIndirectParamW` and even
//! `Shell_NotifyIconW` run a nested message loop, so `wnd_proc` can be entered
//! again while one of them is on the stack. Application state therefore lives
//! in [`App`] behind `Cell` / `RefCell`, and no `RefCell` borrow is ever held
//! across a call that can pump messages. [`App::menu_open`] additionally stops
//! a second menu from opening while the first one, or a dialog it launched, is
//! still up; a tray click meanwhile brings that dialog to the front instead.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod icon;
mod monitor;
mod settings;

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
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetLastActivePopup, GetMessageW,
    GetSystemMetrics, KillTimer, LoadCursorW, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SetCoalescableTimer, SetForegroundWindow,
    TrackPopupMenuEx, TranslateMessage, CW_USEDEFAULT, HICON, IDC_ARROW, IDYES, MB_DEFBUTTON2,
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_YESNO, MF_CHECKED, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_NULL,
    WM_TIMER, WNDCLASSEXW,
};

use icon::Level;
use monitor::{format_bytes, shutdown_wsl, Monitor, Status};
use settings::Settings;

/// Private message the shell sends to our window for tray-icon events
/// (`NOTIFYICONDATAW::uCallbackMessage`). With `NOTIFYICON_VERSION_4` the
/// event id is in the low word of `lParam`.
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
/// Keyboard activation of the icon (Enter/Space). `windows-sys` exports
/// `NIN_SELECT` and `NINF_KEY` but not their combination.
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
/// Posted by the shutdown thread once `wsl --shutdown` has returned, so the
/// UI thread re-polls immediately instead of waiting for the next timer tick.
const WM_REFRESH_NOW: u32 = WM_APP + 2;

/// `SetTimer` id of the poll timer.
const TIMER_POLL: usize = 1;

// Menu command ids returned by TrackPopupMenuEx(TPM_RETURNCMD). Status lines
// are disabled items and are never returned, but still need distinct ids.
const IDM_STATUS: usize = 1;
const IDM_STATS: usize = 2;
const IDM_SHUTDOWN: usize = 3;
const IDM_REFRESH: usize = 4;
const IDM_AUTOSTART: usize = 5;
const IDM_EXIT: usize = 6;
const IDM_SETTINGS: usize = 7;
const IDM_COFFEE: usize = 8;

/// Window class of the hidden window. Also handy for finding the window from
/// outside (`FindWindowW`) when automating tests.
const CLASS_NAME: &str = "WSLTrayWindow";
/// Caption used for message boxes.
const APP_TITLE: &str = "WSL2 Tray";
/// Opened in the browser by "Buy me a coffee".
const COFFEE_URL: &str = "https://buymeacoffee.com/idct";

/// Per-user autostart key and the value name written there by
/// "Start with Windows".
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WSLTray";

/// Encodes a string as NUL-terminated UTF-16 for the `*W` Win32 functions.
///
/// The returned `Vec` must outlive the call it is passed to; for structures
/// that keep the pointer (e.g. `WNDCLASSEXW::lpszClassName`) bind it to a
/// local first.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- command line ----

/// Parsed command line. Defaults are documented in [`usage`].
struct Options {
    /// Interval of the presence check (`-poll`).
    poll: Duration,
    /// Interval of the CPU/memory refresh while the VM runs (`-interval`).
    stats: Duration,
    /// Image name of the VM process (`-process`), matched case-insensitively.
    process: String,
    /// Diagnostics log file (`-log`), appended to.
    log: Option<String>,
    /// Directory for the icon PNG dump (`-render-test`); exits afterwards.
    render_test: Option<String>,
}

/// Default image name of the WSL2 VM process. Windows 11 calls it `vmmemWSL`;
/// the `win10` Cargo feature builds the Windows 10 variant, where it is
/// `vmmem`. Either build can be pointed at the other name with `-process`.
const DEFAULT_PROCESS: &str = if cfg!(feature = "win10") {
    "vmmem"
} else {
    "vmmemWSL"
};

/// Help text for `-h` and flag errors. A function rather than a constant only
/// because the `--process` default is chosen per build.
fn usage() -> String {
    format!(
        "wsl-tray [--poll 7.5s] [--interval 30s] [--process {DEFAULT_PROCESS}] [--log FILE] [--render-test DIR]

  --poll         how often to check whether WSL2 is running (cheap)
  --interval     how often to refresh CPU/memory while WSL2 is running
  --process      name of the WSL2 VM process
  --log          append diagnostic log lines to this file
  --render-test  write sample icon PNGs to this directory and exit"
    )
}

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
        poll: Duration::from_millis(7500),
        stats: Duration::from_secs(30),
        process: DEFAULT_PROCESS.into(),
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
            return Err(format!("bad flag syntax: {a}\n\n{}", usage()));
        }
        let (key, mut inline) = match name.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (name, None),
        };
        let mut value = || {
            inline
                .take()
                .or_else(|| it.next())
                .ok_or_else(|| format!("missing value for --{key}\n\n{}", usage()))
        };
        match key {
            "poll" => o.poll = parse_duration(&value()?).ok_or("bad --poll duration")?,
            "interval" => o.stats = parse_duration(&value()?).ok_or("bad --interval duration")?,
            "process" => o.process = value()?,
            "log" => o.log = Some(value()?),
            "render-test" => o.render_test = Some(value()?),
            "h" | "help" => return Ok(None),
            _ => return Err(format!("unknown flag: {a}\n\n{}", usage())),
        }
    }
    Ok(Some(o))
}

// ---- diagnostics ----

/// The `-log` file, if one was opened. Shared with the shutdown thread.
static LOG: Mutex<Option<File>> = Mutex::new(None);
/// Cheap pre-check so the poll path does no formatting when logging is off.
static LOG_ON: AtomicBool = AtomicBool::new(false);

/// Appends a line to the `-log` file. Without one, the arguments are not
/// even formatted. Same line format as the Go version's `log` package.
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

/// Local wall-clock time via `GetLocalTime` (no chrono dependency needed).
fn local_time() -> SYSTEMTIME {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    t
}

/// Without keyboard or mouse input in this session for this long, nobody is
/// looking at the icon and polling relaxes (see [`Monitor::poll`]).
const IDLE_AFTER_MS: u32 = 120_000;

/// Whether the session has seen no input for [`IDLE_AFTER_MS`]. Tick counts
/// wrap every 49 days; wrapping subtraction keeps the difference right
/// across that. A failed query counts as "someone is there".
fn user_idle() -> bool {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut lii) } == 0 {
        return false;
    }
    unsafe { GetTickCount() }.wrapping_sub(lii.dwTime) >= IDLE_AFTER_MS
}

/// `MessageBoxW` with the application title. Returns the button id (`IDYES`
/// etc.). Blocks and pumps messages until dismissed, see the re-entrancy note
/// in the module docs.
fn message_box(hwnd: HWND, text: &str, flags: u32) -> i32 {
    unsafe { MessageBoxW(hwnd, wide(text).as_ptr(), wide(APP_TITLE).as_ptr(), flags) }
}

// ---- application state ----

/// All mutable state of the program. There is exactly one instance, stored in
/// the [`APP`] thread-local of the UI thread and reached through
/// [`with_app`] from the window procedure.
///
/// Every field is a `Cell` or `RefCell` because the window procedure is
/// re-entered while `TrackPopupMenuEx` / `MessageBoxW` / `Shell_NotifyIconW`
/// pump messages; see the module docs. No `RefCell` borrow is held across
/// such a call.
struct App {
    /// The hidden window that owns the tray icon and the popup menu.
    hwnd: Cell<HWND>,
    /// The `NOTIFYICONDATAW` last passed to the shell. Kept so `NIM_MODIFY`
    /// and `NIM_DELETE` can reuse the same identity (`hWnd` + `uID`).
    nid: RefCell<NOTIFYICONDATAW>,
    /// Current tray icon. Replaced (and the old one destroyed) only when the
    /// colour level changes.
    hicon: Cell<HICON>,
    /// The WSL2 sampler.
    mon: RefCell<Monitor>,
    /// Icon edge length in pixels: `SM_CXSMICON` at the current DPI
    /// (16 at 100 %, 20 at 125 %, 24 at 150 %, ...).
    icon_size: usize,
    /// True between the user confirming "Shut down WSL2" and the shutdown
    /// thread posting `WM_REFRESH_NOW`. Greys out the menu item meanwhile.
    shutting_down: Cell<bool>,
    /// Re-entrancy guard for [`App::show_menu`]; held until the chosen
    /// command (including any dialog it shows) has finished.
    menu_open: Cell<bool>,
    /// Id of the registered `"TaskbarCreated"` message, broadcast by a new
    /// Explorer instance; the icon has to be added again then.
    taskbar_created: Cell<u32>,
    /// Colour level of `hicon`, to skip redundant re-renders.
    last_level: Cell<Option<Level>>,
    /// The colour thresholds in effect: loaded at start, replaced when the
    /// settings dialog is confirmed.
    settings: Cell<Settings>,
    /// Windows 11 promotion (see [`promote_tray_icon`]) is done, or not
    /// applicable on this Windows version.
    promoted: Cell<bool>,
    /// Number of promotion attempts; Explorer creates the registry entry a
    /// little after `NIM_ADD`, so the first attempts may find nothing.
    promote_tries: Cell<u32>,
    /// Canonical executable path in Win32 syntax, used to find our entry
    /// under `NotifyIconSettings` (Explorer stores the resolved path there).
    exe_path: String,
    /// Arguments this instance was started with, replayed into the autostart
    /// Run value so an autostarted copy behaves the same.
    launch_args: Vec<String>,
}

thread_local! {
    /// The single [`App`], owned by the UI thread. `OnceCell` rather than
    /// `RefCell<Option<App>>` so a re-entered `wnd_proc` never hits a borrow
    /// panic just for looking the state up.
    static APP: OnceCell<App> = const { OnceCell::new() };
}

/// Runs `f` with the [`App`] if it has been created (it always has by the
/// time any window message arrives, but `wnd_proc` cannot assume that).
fn with_app<R>(f: impl FnOnce(&App) -> R) -> Option<R> {
    APP.with(|a| a.get().map(f))
}

/// Entry point: parses flags, handles the `-render-test` and single-instance
/// early exits, creates the window and icon, then runs the message loop until
/// `WM_QUIT`.
fn main() {
    let opts = match parse_args() {
        Ok(Some(o)) => o,
        Ok(None) => {
            message_box(null_mut(), &usage(), MB_ICONINFORMATION);
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

    // Single instance per session: a named mutex in the Local\ namespace. If
    // it already exists another copy is running and this one exits quietly.
    // The handle is intentionally leaked; the OS releases it with the process.
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
        settings: Cell::new(Settings::load()),
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
        // A coalescable timer: Windows may fire it up to `tolerance` late so
        // the wake-up can be batched with other timers on the machine, which
        // is how a status icon should behave on battery. 20 % of the
        // interval, at most a second.
        let ms = opts.poll.as_millis() as u32;
        let tolerance = (ms / 5).clamp(1, 1000);
        unsafe { SetCoalescableTimer(a.hwnd.get(), TIMER_POLL, ms, None, tolerance) };
    });

    // Standard message loop. GetMessageW returns 0 on WM_QUIT and -1 on error;
    // both end the program.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl App {
    /// Registers the window class and creates the hidden window.
    ///
    /// It is a normal top-level window with no style bits and no `ShowWindow`
    /// call, so it never appears. A message-only window (`HWND_MESSAGE`)
    /// would not do: `TrackPopupMenuEx` needs an owner that can be brought to
    /// the foreground, otherwise the menu does not close when the user clicks
    /// elsewhere.
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

    /// Message handler. Returns `Some(result)` for messages it consumed and
    /// `None` to let `DefWindowProcW` handle everything else.
    fn handle(&self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TRAY_CALLBACK => {
                // NOTIFYICON_VERSION_4 semantics: left click -> NIN_SELECT,
                // keyboard -> NIN_KEYSELECT, right click -> WM_CONTEXTMENU. The
                // raw mouse messages also arrive and are deliberately ignored
                // to avoid opening the menu twice.
                match (lparam & 0xFFFF) as u32 {
                    WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT if self.menu_open.get() => {
                        // A command from the previous click is still running
                        // with the settings dialog or a message box up: bring
                        // that to the front rather than swallowing the click.
                        // (The menu itself is never "active", so while it is
                        // open this finds nothing and the click is ignored.)
                        let popup = unsafe { GetLastActivePopup(hwnd) };
                        if popup != hwnd {
                            unsafe { SetForegroundWindow(popup) };
                        }
                    }
                    WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT => self.show_menu(),
                    // (No refresh on hover: with the standard tooltip,
                    // NIF_SHOWTIP, the shell sends no NIN_POPUPOPEN.)
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
                // The shutdown thread is done; re-enable the menu item and
                // show the new state right away.
                self.shutting_down.set(false);
                self.tick(true);
                Some(0)
            }
            WM_CLOSE => {
                unsafe { DestroyWindow(hwnd) };
                Some(0)
            }
            WM_DESTROY => {
                // Tear down in reverse order of creation, then end the loop.
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
                // Explorer was restarted (or crashed): its tray forgot us.
                self.add_tray_icon();
                Some(0)
            }
            _ => None,
        }
    }

    /// One poll cycle: asks the [`Monitor`] for the current state and, if
    /// anything visible changed (or the icon does not exist yet), updates
    /// the icon and tooltip. `force` bypasses the stats interval.
    fn tick(&self, force: bool) {
        let relaxed = !force && user_idle();
        let (st, changed) = self.mon.borrow_mut().poll(force, relaxed);
        log!(
            "poll force={force} relaxed={relaxed} -> running={} pid={} cpu={:.2} mem={} changed={changed} snapshots={}",
            st.running,
            st.pid,
            st.cpu.unwrap_or(-1.0),
            format_bytes(st.mem),
            self.mon.borrow().snapshots()
        );
        if !changed && !self.hicon.get().is_null() {
            return;
        }
        self.update_icon(&st);
    }

    /// Pushes `st` to the shell: re-renders the icon if its colour level
    /// changed, rewrites the tooltip, and calls `NIM_MODIFY`.
    ///
    /// The previous icon is destroyed only after the new one has been
    /// created; the shell copies the icon during `NIM_MODIFY`, so destroying
    /// the old handle afterwards is safe.
    fn update_icon(&self, st: &Status) {
        let lv = Level::for_status(st, &self.settings.get());
        if self.last_level.get() != Some(lv) || self.hicon.get().is_null() {
            log!("icon -> {lv:?}");
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

    /// Adds the icon to the notification area and switches it to
    /// `NOTIFYICON_VERSION_4` behaviour, under which the shell sends
    /// `NIN_SELECT` / `NIN_KEYSELECT` / `WM_CONTEXTMENU` in `lParam`'s low
    /// word instead of raw mouse messages only. Also used after an Explorer
    /// restart, hence the full re-initialisation of `nid`.
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

    /// Builds and shows the popup menu at the cursor, then runs the chosen
    /// command. Used for left click, right click and keyboard activation.
    ///
    /// The menu is rebuilt on every click because its contents (the stats
    /// line, the enabled/checked states) depend on the current status.
    fn show_menu(&self) {
        // TrackPopupMenuEx pumps messages, so a second tray click would land
        // here again while the first menu is still open.
        if self.menu_open.replace(true) {
            return;
        }
        // Sample now, so the status lines show the present rather than the
        // last interval (one snapshot per click).
        self.tick(true);
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
            add(MF_STRING, IDM_SETTINGS, "S&ettings...");
            add(MF_STRING, IDM_COFFEE, "Buy me a &coffee");
            add(MF_SEPARATOR, 0, "");
            add(MF_STRING, IDM_EXIT, "E&xit");

            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let hwnd = self.hwnd.get();
            // The documented tray-menu dance (KB 135788): the owner must be the
            // foreground window or the menu will not close when the user
            // clicks elsewhere, and posting a no-op message afterwards makes
            // the menu go away promptly once the next click lands.
            SetForegroundWindow(hwnd);
            // TPM_BOTTOMALIGN: the menu opens upwards from the taskbar.
            // TPM_RETURNCMD: the chosen id is returned instead of a WM_COMMAND.
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
            cmd as usize // 0 = dismissed without a choice
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
            IDM_SETTINGS => self.show_settings(),
            IDM_COFFEE => {
                if let Err(e) = open_url(self.hwnd.get(), COFFEE_URL) {
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

    /// "Shut down WSL2": asks for confirmation (default button is No), then
    /// runs `wsl --shutdown` on a helper thread so the UI keeps responding.
    /// The thread reports back with `WM_REFRESH_NOW`.
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
        // HWND is a raw pointer and therefore not Send; carry it as an integer.
        // The thread only ever hands it to PostMessageW, which is thread-safe.
        let hwnd = self.hwnd.get() as isize;
        std::thread::spawn(move || {
            if let Err(e) = shutdown_wsl() {
                message_box(null_mut(), &e, MB_ICONERROR);
            }
            unsafe { PostMessageW(hwnd as HWND, WM_REFRESH_NOW, 0, 0) };
        });
    }

    /// "Settings...": edits the colour thresholds in the modal dialog, then
    /// stores them and re-colours the icon straight away. If storing fails
    /// the new values still apply until the program exits.
    fn show_settings(&self) {
        let mut s = self.settings.get();
        match settings::edit(self.hwnd.get(), &mut s) {
            Ok(true) => {
                self.settings.set(s);
                if let Err(e) = s.save() {
                    message_box(
                        self.hwnd.get(),
                        &format!("{e}\n\nThe new thresholds apply until the program exits."),
                        MB_ICONERROR,
                    );
                }
                // Bound first: update_icon can pump messages, and a tick
                // arriving then must not find the Monitor still borrowed.
                let st = self.mon.borrow().current();
                self.update_icon(&st);
            }
            Ok(false) => {}
            Err(e) => {
                message_box(self.hwnd.get(), &e, MB_ICONERROR);
            }
        }
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

    /// Writes or deletes the `Run` value. The value is the quoted launch path
    /// followed by this instance's own arguments, so `-poll`/`-log` settings
    /// survive into the autostarted copy.
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

/// The window procedure registered in [`App::create_window`]. Forwards to
/// [`App::handle`]; anything not handled there goes to `DefWindowProcW`.
extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(Some(r)) = with_app(|a| a.handle(hwnd, msg, wparam, lparam)) {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Tooltip text: state, the stats line, and when it was last sampled.
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

/// `CPU 12.4 %   MEM 5.40 GB (11.2 %)`; CPU shows `...` until the second
/// sample after the VM appeared.
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

/// Copies `s` into `szTip` (128 UTF-16 units including the terminator),
/// truncating if needed.
fn set_tip(nid: &mut NOTIFYICONDATAW, s: &str) {
    let mut u: Vec<u16> = s.encode_utf16().collect();
    u.truncate(nid.szTip.len() - 1);
    nid.szTip.fill(0);
    nid.szTip[..u.len()].copy_from_slice(&u);
}

/// Opens `url` in the user's default browser through the shell's "open"
/// verb, which resolves the `https` protocol association. Returns without
/// waiting; `ShellExecuteW` reports failure with a value of 32 or less.
fn open_url(hwnd: HWND, url: &str) -> Result<(), String> {
    let r = unsafe {
        ShellExecuteW(
            hwnd,
            wide("open").as_ptr(),
            wide(url).as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if r as usize <= 32 {
        return Err(format!("cannot open {url} (error {})", r as usize));
    }
    Ok(())
}

// ---- registry helpers ----

/// An open registry key, closed on drop. Only the handful of operations the
/// program needs are wrapped.
struct RegKey(HKEY);

impl RegKey {
    /// Opens `root\sub` with the given `KEY_*` access mask.
    fn open(root: HKEY, sub: &str, access: u32) -> Result<RegKey, String> {
        let mut h: HKEY = null_mut();
        let r = unsafe { RegOpenKeyExW(root, wide(sub).as_ptr(), 0, access, &mut h) };
        if r != 0 {
            return Err(format!("cannot open {sub} ({r})"));
        }
        Ok(RegKey(h))
    }

    /// Opens `root\sub` for reading and writing, creating it (and any
    /// missing parents) first if needed.
    fn create(root: HKEY, sub: &str) -> Result<RegKey, String> {
        let mut h: HKEY = null_mut();
        let r = unsafe {
            RegCreateKeyExW(
                root,
                wide(sub).as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                null(),
                &mut h,
                null_mut(),
            )
        };
        if r != 0 {
            return Err(format!("cannot create {sub} ({r})"));
        }
        Ok(RegKey(h))
    }

    /// Reads a `REG_SZ` / `REG_EXPAND_SZ` value (unexpanded, up to 1023
    /// characters). `None` if absent or of another type.
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

    /// Reads a `REG_DWORD` value; `None` if absent or of another type.
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

    /// True if the value exists, whatever its type (a size-only query).
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

    /// Writes a `REG_SZ` value. Fails with the Win32 error code.
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

    /// Writes a `REG_DWORD` value. Fails with the Win32 error code.
    fn set_dword(&self, name: &str, value: u32) -> Result<(), u32> {
        let r = unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                REG_DWORD,
                (&value as *const u32).cast(),
                4,
            )
        };
        if r != 0 {
            return Err(r);
        }
        Ok(())
    }

    /// Name of the `index`-th subkey, or `None` past the end (key names are
    /// at most 255 characters, so the fixed buffer always suffices).
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

/// Whether the "Start with Windows" Run value currently exists.
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

/// Windows 11 hides new tray icons in the overflow flyout by default. The
/// per-icon choice lives under `HKCU\Control Panel\NotifyIconSettings\<id>`,
/// where Explorer records `ExecutablePath` and (once the user has decided)
/// `IsPromoted`. Setting `IsPromoted = 1` ourselves shows the icon next to
/// the clock, and Explorer picks the change up live.
///
/// Only an *absent* value is written: if the user has already hidden or shown
/// the icon in Settings › Taskbar, that choice stands.
///
/// Returns true once the entry was found and handled, or when this Windows
/// version has no such key (Windows 10). Explorer creates the entry a little
/// after `NIM_ADD`, so the caller retries on the next timer ticks.
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
                // Best effort: if this fails the icon merely stays in the overflow.
                let _ = k.set_dword("IsPromoted", 1);
            }
            return true;
        }
    }
    false // not found (yet)
}

// ---- -render-test: dump icons as PNG for a visual check ----

/// Writes `icon-<size>-<state>.png` for every state at the tray sizes
/// (16–32 px), 48 px and 256 px. The 256 px files are also the source of the
/// exe icon in `winres/`.
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

    /// Opens the sponsoring page in the browser, so it is not part of the
    /// default run: `cargo test --release -- --ignored coffee_link`.
    #[test]
    #[ignore]
    fn coffee_link() {
        open_url(null_mut(), COFFEE_URL).unwrap();
    }

    /// `GetLastInputInfo` answers (the test session had input at some point
    /// in the last two days), and the idle decision is consistent with it.
    #[test]
    fn idle_query_works() {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        assert_ne!(unsafe { GetLastInputInfo(&mut lii) }, 0);
        let idle_ms = unsafe { GetTickCount() }.wrapping_sub(lii.dwTime);
        assert!(idle_ms < 2 * 24 * 3600 * 1000, "idle for {idle_ms} ms?");
        assert_eq!(user_idle(), idle_ms >= IDLE_AFTER_MS);
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
