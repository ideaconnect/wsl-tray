//! User settings: the load percentages at which the tray icon changes
//! colour, where they are kept, and the dialog that edits them.
//!
//! # Storage
//!
//! The values are two `REG_DWORD`s, `WarnThreshold` and `HighThreshold`,
//! under `HKCU\Software\IDCT\wsl-tray`: the conventional
//! `HKEY_CURRENT_USER\Software\<company>\<product>` place for the per-user
//! settings of a Win32 program. Missing or unusable values (see
//! [`Settings::new`]) fall back to the defaults, 50 and 75.
//!
//! The same code is right for the Microsoft Store. A packaged (MSIX) app has
//! a private registry hive, and Windows redirects everything it writes under
//! `HKEY_CURRENT_USER` into `%LOCALAPPDATA%\Packages\<package family>\
//! SystemAppData\Helium\User.dat`: the settings are then private to the
//! package and are deleted with it, while reads still see a value that an
//! unpackaged copy left in the real key. Nothing here needs package identity,
//! so the plain executables from the GitHub releases work unchanged.
//!
//! # The dialog
//!
//! The build has no resource compiler (icon, manifest and version info are
//! linked from pre-built objects, see `build.rs`), so the dialog is a
//! `DLGTEMPLATEEX` assembled in memory ([`Template`]) and shown with
//! `DialogBoxIndirectParamW`. That is the structure a `.rc` file compiles to,
//! so the dialog manager does all the usual work: Segoe UI 9 pt scaled to the
//! monitor's DPI (the layout is in dialog units), Tab and mnemonic
//! navigation, Enter and Esc for OK and Cancel, the standard frame. Two
//! number fields with up-down spinners (`msctls_updown32` from common
//! controls v6, which the manifest enables) and a "Restore defaults" button
//! are all there is.
//!
//! `DialogBoxIndirectParamW` runs a nested message loop; the main window
//! keeps receiving its timer ticks meanwhile, see the re-entrancy notes in
//! `main.rs`.

use std::ptr::null;

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{DEFAULT_CHARSET, FW_NORMAL};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, KEY_READ};
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, EM_SETSEL, ICC_UPDOWN_CLASS, INITCOMMONCONTROLSEX, UDM_SETPOS32,
    UDM_SETRANGE32, UDS_ALIGNRIGHT, UDS_ARROWKEYS, UDS_AUTOBUDDY, UDS_SETBUDDYINT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DialogBoxIndirectParamW, EndDialog, GetDlgItem, GetDlgItemInt, GetSystemMetrics,
    GetWindowLongPtrW, LoadImageW, SendDlgItemMessageW, SendMessageW, SetWindowLongPtrW,
    BS_DEFPUSHBUTTON, BS_GROUPBOX, BS_PUSHBUTTON, DS_CENTER, DS_SETFONT, ES_AUTOHSCROLL, ES_NUMBER,
    GWLP_USERDATA, ICON_BIG, ICON_SMALL, IDCANCEL, IDOK, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED,
    MB_ICONWARNING, SM_CXSMICON, SM_CYSMICON, WM_COMMAND, WM_INITDIALOG, WM_NEXTDLGCTL, WM_SETICON,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_GROUP, WS_POPUP, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE,
};

use crate::{message_box, wide, RegKey};

/// Registry key holding the settings, relative to `HKEY_CURRENT_USER`.
pub const KEY: &str = r"Software\IDCT\wsl-tray";
/// `REG_DWORD` value names under [`KEY`], percentages.
const WARN_VALUE: &str = "WarnThreshold";
const HIGH_VALUE: &str = "HighThreshold";

/// Validation messages, shown by the dialog.
const RANGE_MSG: &str = "Enter a whole number between 0 and 100.";
const ORDER_MSG: &str = "The orange threshold cannot be higher than the red one.";

/// The load percentages at which the icon changes colour; see
/// [`Level::for_status`](crate::icon::Level::for_status). `Copy` so the UI
/// thread can keep the current values in a `Cell`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Orange from this load (percent) on.
    pub warn: u32,
    /// Red above this load (percent).
    pub high: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { warn: 50, high: 75 }
    }
}

impl Settings {
    /// Accepts a pair if both are percentages and orange does not start
    /// above red. Equal values are allowed (orange only at exactly that
    /// load), so 100 for both all but switches the colour scale off.
    pub fn new(warn: u32, high: u32) -> Result<Settings, &'static str> {
        if warn > 100 || high > 100 {
            return Err(RANGE_MSG);
        }
        if warn > high {
            return Err(ORDER_MSG);
        }
        Ok(Settings { warn, high })
    }

    /// The stored settings, or the defaults if there are none or they do
    /// not pass [`Settings::new`].
    pub fn load() -> Settings {
        Self::load_from(KEY)
    }

    fn load_from(sub: &str) -> Settings {
        let Ok(k) = RegKey::open(HKEY_CURRENT_USER, sub, KEY_READ) else {
            return Settings::default();
        };
        match (k.read_dword(WARN_VALUE), k.read_dword(HIGH_VALUE)) {
            (Some(w), Some(h)) => Settings::new(w, h).unwrap_or_default(),
            _ => Settings::default(),
        }
    }

    /// Writes the settings, creating the key if needed.
    pub fn save(&self) -> Result<(), String> {
        self.save_to(KEY)
    }

    fn save_to(&self, sub: &str) -> Result<(), String> {
        let k = RegKey::create(HKEY_CURRENT_USER, sub)?;
        k.set_dword(WARN_VALUE, self.warn)
            .and_then(|()| k.set_dword(HIGH_VALUE, self.high))
            .map_err(|r| format!("cannot write HKCU\\{sub} ({r})"))
    }
}

// ---- the dialog ----

// Control ids. IDOK and IDCANCEL are the dialog manager's own.
const IDC_WARN: i32 = 100;
const IDC_WARN_SPIN: i32 = 101;
const IDC_HIGH: i32 = 102;
const IDC_HIGH_SPIN: i32 = 103;
const IDC_DEFAULTS: i32 = 104;
/// Id of the labels and the group box, which are never looked up.
const IDC_STATIC: i32 = -1;

/// Shows the modal settings dialog, owned by `owner` and filled from `s`.
/// `Ok(true)` means the user pressed OK and `s` now holds the new, validated
/// values; `Ok(false)` is Cancel. Pumps messages until the dialog closes.
pub fn edit(owner: HWND, s: &mut Settings) -> Result<bool, String> {
    let tpl = template();
    let icc = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_UPDOWN_CLASS,
    };
    let r = unsafe {
        // Registers msctls_updown32; cheap to repeat.
        InitCommonControlsEx(&icc);
        DialogBoxIndirectParamW(
            GetModuleHandleW(null()),
            tpl.as_ptr().cast(),
            owner,
            Some(dlg_proc),
            s as *mut Settings as LPARAM,
        )
    };
    match r as i32 {
        IDOK => Ok(true),
        IDCANCEL => Ok(false),
        // 0 (bad owner) or -1: the dialog never appeared.
        _ => Err(format!("cannot show the settings dialog ({})", unsafe {
            GetLastError()
        })),
    }
}

/// Dialog procedure. `GWLP_USERDATA` holds the `*mut Settings` given to
/// [`edit`], which outlives the dialog. Returns 1 for handled messages.
extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, lparam);
                set_values(hwnd, *(lparam as *const Settings));
            }
            set_icon(hwnd);
            1 // let the dialog manager focus the first field
        }
        WM_COMMAND => {
            match (wparam & 0xFFFF) as i32 {
                IDOK => match read_values(hwnd) {
                    Ok(s) => unsafe {
                        *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Settings) = s;
                        EndDialog(hwnd, IDOK as isize);
                    },
                    Err((id, why)) => unsafe {
                        message_box(hwnd, why, MB_ICONWARNING);
                        // Back to the offending field, with its text selected.
                        SendMessageW(hwnd, WM_NEXTDLGCTL, GetDlgItem(hwnd, id) as WPARAM, 1);
                        SendDlgItemMessageW(hwnd, id, EM_SETSEL, 0, -1);
                    },
                },
                IDCANCEL => unsafe {
                    EndDialog(hwnd, IDCANCEL as isize);
                },
                IDC_DEFAULTS => set_values(hwnd, Settings::default()),
                _ => return 0,
            }
            1
        }
        _ => 0,
    }
}

/// Puts `s` into the two fields, through the spinners so their positions
/// and the text agree.
fn set_values(hwnd: HWND, s: Settings) {
    for (spin, v) in [(IDC_WARN_SPIN, s.warn), (IDC_HIGH_SPIN, s.high)] {
        unsafe {
            SendDlgItemMessageW(hwnd, spin, UDM_SETRANGE32, 0, 100);
            SendDlgItemMessageW(hwnd, spin, UDM_SETPOS32, 0, v as LPARAM);
        }
    }
}

/// Reads the fields back. On failure: the id of the field to correct and
/// the message to show.
fn read_values(hwnd: HWND) -> Result<Settings, (i32, &'static str)> {
    let field = |id: i32| {
        let mut ok = 0;
        let v = unsafe { GetDlgItemInt(hwnd, id, &mut ok, 0) };
        if ok != 0 && v <= 100 {
            Ok(v)
        } else {
            Err((id, RANGE_MSG))
        }
    };
    let (warn, high) = (field(IDC_WARN)?, field(IDC_HIGH)?);
    Settings::new(warn, high).map_err(|why| (IDC_WARN, why))
}

/// Gives the dialog the executable's icon, for its caption, the taskbar
/// button and Alt+Tab. `LR_SHARED` handles belong to the system and are not
/// destroyed.
fn set_icon(hwnd: HWND) {
    let name = wide("APP"); // the RT_GROUP_ICON name in winres/winres.json
    unsafe {
        let hinst = GetModuleHandleW(null());
        let big = LoadImageW(
            hinst,
            name.as_ptr(),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        );
        let small = LoadImageW(
            hinst,
            name.as_ptr(),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        );
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as WPARAM, big as LPARAM);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as WPARAM, small as LPARAM);
    }
}

// ---- the template ----

/// Window classes of a dialog template in `sz_Or_Ord` form: the ordinal
/// atoms of the predefined classes, or a NUL-terminated class name.
const BUTTON: &[u16] = &[0xFFFF, 0x0080];
const EDIT: &[u16] = &[0xFFFF, 0x0081];
const STATIC: &[u16] = &[0xFFFF, 0x0082];
/// `SS_LEFT`, which `windows-sys` does not export.
const SS_LEFT: u32 = 0;

/// The settings dialog, laid out in dialog units (a quarter of the average
/// character width by an eighth of the character height of the dialog
/// font): 240 x 120 with 7-unit margins, 14-unit fields and buttons, labels
/// 3 units below the top of their field so the baselines meet. Tab order is
/// item order; each label's mnemonic activates the field after it, and each
/// spinner attaches to the field before it (`UDS_AUTOBUDDY`).
fn template() -> Vec<u32> {
    let field = WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_NUMBER as u32;
    let spin = UDS_SETBUDDYINT | UDS_ALIGNRIGHT | UDS_AUTOBUDDY | UDS_ARROWKEYS;
    let spin_class = wide("msctls_updown32");
    let mut t = Template::new(
        "WSL2 Tray Settings",
        (240, 120),
        WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_CENTER as u32,
        WS_EX_APPWINDOW,
    );
    t.item(
        BUTTON,
        BS_GROUPBOX as u32,
        (7, 7, 226, 85),
        IDC_STATIC,
        "Icon colour",
    );
    t.item(
        STATIC,
        SS_LEFT | WS_GROUP,
        (16, 22, 60, 8),
        IDC_STATIC,
        "&Orange from:",
    );
    t.item(EDIT, field, (80, 19, 36, 14), IDC_WARN, "");
    t.item(&spin_class, spin, (116, 19, 11, 14), IDC_WARN_SPIN, "");
    t.item(STATIC, SS_LEFT, (120, 22, 20, 8), IDC_STATIC, "%");
    t.item(
        STATIC,
        SS_LEFT | WS_GROUP,
        (16, 40, 60, 8),
        IDC_STATIC,
        "&Red above:",
    );
    t.item(EDIT, field, (80, 37, 36, 14), IDC_HIGH, "");
    t.item(&spin_class, spin, (116, 37, 11, 14), IDC_HIGH_SPIN, "");
    t.item(STATIC, SS_LEFT, (120, 40, 20, 8), IDC_STATIC, "%");
    t.item(
        STATIC,
        SS_LEFT | WS_GROUP,
        (16, 58, 208, 27),
        IDC_STATIC,
        "Load is WSL2's share of the whole machine: CPU or memory, whichever is \
         higher. The icon is green below the orange value and grey while WSL2 is off.",
    );
    t.item(
        BUTTON,
        BS_PUSHBUTTON as u32 | WS_TABSTOP | WS_GROUP,
        (7, 99, 76, 14),
        IDC_DEFAULTS,
        "Restore &defaults",
    );
    t.item(
        BUTTON,
        BS_DEFPUSHBUTTON as u32 | WS_TABSTOP,
        (129, 99, 50, 14),
        IDOK,
        "OK",
    );
    t.item(
        BUTTON,
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        (183, 99, 50, 14),
        IDCANCEL,
        "Cancel",
    );
    t.finish()
}

/// Builder for an in-memory `DLGTEMPLATEEX` ("DLGTEMPLATEEX" and
/// "DLGITEMTEMPLATEEX" in the Windows SDK documentation): a header followed
/// by one item per control, everything in WORDs, items DWORD-aligned.
struct Template {
    buf: Vec<u16>,
    items: u16,
}

impl Template {
    /// The header: no menu, the default dialog class, Segoe UI 9 pt.
    fn new(title: &str, (w, h): (i16, i16), style: u32, ex_style: u32) -> Template {
        let mut buf = Vec::with_capacity(1024);
        buf.extend([1u16, 0xFFFF]); // dlgVer, signature: the extended format
        push_u32(&mut buf, 0); // helpID
        push_u32(&mut buf, ex_style);
        push_u32(&mut buf, style | DS_SETFONT as u32);
        buf.push(0); // cDlgItems, filled in by finish()
        buf.extend([0, 0, w as u16, h as u16]); // x, y, cx, cy
        buf.extend([0u16, 0]); // menu: none; windowClass: the default
        push_str(&mut buf, title);
        buf.extend([9, FW_NORMAL as u16]); // pointsize, weight
        buf.push(u16::from_le_bytes([0, DEFAULT_CHARSET])); // italic, charset
        push_str(&mut buf, "Segoe UI");
        Template { buf, items: 0 }
    }

    /// Appends a control. `WS_CHILD | WS_VISIBLE` are added to `style`; there
    /// is no extended style and no creation data.
    fn item(
        &mut self,
        class: &[u16],
        style: u32,
        (x, y, w, h): (i16, i16, i16, i16),
        id: i32,
        text: &str,
    ) {
        if self.buf.len() % 2 == 1 {
            self.buf.push(0); // items start on a DWORD boundary
        }
        push_u32(&mut self.buf, 0); // helpID
        push_u32(&mut self.buf, 0); // exStyle
        push_u32(&mut self.buf, style | WS_CHILD | WS_VISIBLE);
        self.buf.extend([x, y, w, h].map(|v| v as u16));
        push_u32(&mut self.buf, id as u32);
        self.buf.extend_from_slice(class);
        push_str(&mut self.buf, text);
        self.buf.push(0); // extraCount
        self.items += 1;
    }

    /// Fills in the item count and returns the template as DWORD-aligned
    /// memory, which `DialogBoxIndirectParamW` requires.
    fn finish(mut self) -> Vec<u32> {
        self.buf[8] = self.items;
        if self.buf.len() % 2 == 1 {
            self.buf.push(0);
        }
        self.buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[lo, hi]| lo as u32 | (hi as u32) << 16)
            .collect()
    }
}

fn push_u32(buf: &mut Vec<u16>, v: u32) {
    buf.extend([v as u16, (v >> 16) as u16]);
}

/// A NUL-terminated UTF-16 string.
fn push_str(buf: &mut Vec<u16>, s: &str) {
    buf.extend(s.encode_utf16());
    buf.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::RegDeleteKeyW;

    #[test]
    fn validation() {
        assert_eq!(Settings::new(50, 75), Ok(Settings::default()));
        assert!(Settings::new(0, 0).is_ok());
        assert!(Settings::new(30, 30).is_ok());
        assert!(Settings::new(100, 100).is_ok());
        assert_eq!(Settings::new(101, 100), Err(RANGE_MSG));
        assert_eq!(Settings::new(10, 101), Err(RANGE_MSG));
        assert_eq!(Settings::new(76, 75), Err(ORDER_MSG));
    }

    #[test]
    fn missing_key_gives_defaults() {
        assert_eq!(
            Settings::load_from(r"Software\IDCT\wsl-tray\no-such-key"),
            Settings::default()
        );
    }

    /// Round trip through a scratch key next to the real one, deleted again
    /// before the assertions so a failure leaves nothing behind.
    #[test]
    fn registry_roundtrip() {
        let sub = format!(r"Software\IDCT\wsl-tray-test-{}", std::process::id());
        let s = Settings { warn: 12, high: 34 };
        s.save_to(&sub).unwrap();
        let back = Settings::load_from(&sub);
        // Values that do not validate (here: red below orange) are ignored.
        RegKey::create(HKEY_CURRENT_USER, &sub)
            .unwrap()
            .set_dword(HIGH_VALUE, 5)
            .unwrap();
        let bad = Settings::load_from(&sub);
        unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(&sub).as_ptr()) };
        assert_eq!(back, s);
        assert_eq!(bad, Settings::default());
    }

    #[test]
    fn template_header() {
        let t = template();
        assert_eq!(t[0], 0xFFFF_0001, "dlgVer 1, signature 0xFFFF");
        assert_eq!(t[1], 0, "helpID");
        assert_eq!(t[2], WS_EX_APPWINDOW);
        assert_ne!(t[3] & DS_SETFONT as u32, 0, "font block present");
        assert_eq!(t[4] & 0xFFFF, 13, "cDlgItems");
        assert_eq!(t[4] >> 16, 0, "x");
        assert_eq!(t[5], 240 << 16, "y, cx");
        assert_eq!(t[6] & 0xFFFF, 120, "cy");
    }

    // The two tests below open the real dialog, so they are not part of the
    // default run: `cargo test --release -- --ignored dialog`.

    use std::ptr::null_mut;
    use std::time::Duration;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsWindow, PostMessageW, SetDlgItemInt, WM_CLOSE,
    };

    /// Caption of the dialog, and of the message boxes it shows.
    const DIALOG: &str = "WSL2 Tray Settings";
    const BOX: &str = crate::APP_TITLE;

    /// Polls for a `#32770` (dialog class) window with this caption, for up
    /// to five seconds. The class filter keeps a running tray instance's
    /// hidden main window, which has the same caption as the message boxes,
    /// out of the way.
    fn wait_for(title: &str) -> HWND {
        for _ in 0..100 {
            let h = unsafe { FindWindowW(wide("#32770").as_ptr(), wide(title).as_ptr()) };
            if !h.is_null() {
                return h;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{title} did not appear");
    }

    /// Drives the dialog from a helper thread the way a user would: reads the
    /// initial values, enters an invalid pair (orange above red) and gets the
    /// message box, restores the defaults, enters a valid pair and confirms;
    /// then opens it again and cancels.
    #[test]
    #[ignore]
    fn dialog_roundtrip() {
        let driver = std::thread::spawn(|| unsafe {
            let dlg = wait_for(DIALOG);
            let get = |id| GetDlgItemInt(dlg, id, null_mut(), 0);
            assert_eq!((get(IDC_WARN), get(IDC_HIGH)), (50, 75), "initial values");

            SetDlgItemInt(dlg, IDC_WARN, 80, 0);
            SetDlgItemInt(dlg, IDC_HIGH, 70, 0);
            PostMessageW(dlg, WM_COMMAND, IDOK as WPARAM, 0);
            let mb = wait_for(BOX);
            PostMessageW(mb, WM_CLOSE, 0, 0);
            while IsWindow(mb) != 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                (get(IDC_WARN), get(IDC_HIGH)),
                (80, 70),
                "kept for correction"
            );

            SendMessageW(dlg, WM_COMMAND, IDC_DEFAULTS as WPARAM, 0);
            assert_eq!(
                (get(IDC_WARN), get(IDC_HIGH)),
                (50, 75),
                "restored defaults"
            );

            SetDlgItemInt(dlg, IDC_WARN, 8, 0);
            SetDlgItemInt(dlg, IDC_HIGH, 12, 0);
            PostMessageW(dlg, WM_COMMAND, IDOK as WPARAM, 0);
        });
        let mut s = Settings::default();
        let r = edit(null_mut(), &mut s);
        driver.join().unwrap();
        assert_eq!(r, Ok(true));
        assert_eq!(s, Settings { warn: 8, high: 12 });

        let driver = std::thread::spawn(|| unsafe {
            let dlg = wait_for(DIALOG);
            assert_eq!(GetDlgItemInt(dlg, IDC_HIGH, null_mut(), 0), 12);
            PostMessageW(dlg, WM_COMMAND, IDCANCEL as WPARAM, 0);
        });
        let r = edit(null_mut(), &mut s);
        driver.join().unwrap();
        assert_eq!(r, Ok(false));
        assert_eq!(s, Settings { warn: 8, high: 12 }, "unchanged by Cancel");
    }

    /// Opens the dialog with the defaults and leaves it open until it is
    /// closed by hand (or by a screenshot script), for checking the layout.
    #[test]
    #[ignore]
    fn dialog_show() {
        let mut s = Settings::default();
        let r = edit(null_mut(), &mut s);
        eprintln!("dialog closed with {r:?}, values {s:?}");
    }
}
