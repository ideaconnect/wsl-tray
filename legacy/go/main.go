//go:build windows

package main

import (
	"flag"
	"fmt"
	"image/png"
	"io"
	"log"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

const (
	wmTrayCallback = wmApp + 1 // Shell_NotifyIcon callback
	wmRefreshNow   = wmApp + 2 // posted from the shutdown goroutine

	timerPoll = 1

	idmStatus    = 1
	idmStats     = 2
	idmShutdown  = 3
	idmRefresh   = 4
	idmAutostart = 5
	idmExit      = 6

	className = "WSLTrayWindow"
	appTitle  = "WSL2 Tray"
)

var (
	flagPoll     = flag.Duration("poll", 5*time.Second, "how often to check whether WSL2 is running (cheap)")
	flagStats    = flag.Duration("interval", 30*time.Second, "how often to refresh CPU/memory while WSL2 is running")
	flagProcess  = flag.String("process", "vmmemWSL", "name of the WSL2 VM process")
	flagRenderTo = flag.String("render-test", "", "write sample icon PNGs to this directory and exit")
	flagLog      = flag.String("log", "", "append diagnostic log lines to this file")
)

type app struct {
	hwnd          uintptr
	nid           notifyIconDataW
	hicon         uintptr
	mon           *Monitor
	iconSize      int
	shuttingDown  bool
	menuOpen      bool
	taskbarCreate uint32
	lastLevel     level
	promoted      bool // NotifyIconSettings IsPromoted handled
	promoteTries  int
}

func init() { runtime.LockOSThread() }

func main() {
	flag.Parse()

	if *flagRenderTo != "" {
		if err := renderTest(*flagRenderTo); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}

	log.SetOutput(io.Discard)
	if *flagLog != "" {
		if f, err := os.OpenFile(*flagLog, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644); err == nil {
			log.SetOutput(f)
		}
	}

	// Single instance.
	pCreateMutexW.Call(0, 0, uintptr(unsafe.Pointer(utf16("Local\\WSLTray.SingleInstance"))))
	if lastError() == errorAlreadyExists {
		return
	}

	// Per-monitor DPI awareness so SM_CXSMICON gives the right icon size.
	if pSetProcessDpiAwarenessContext.Find() == nil {
		pSetProcessDpiAwarenessContext.Call(^uintptr(3)) // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 (-4)
	} else {
		pSetProcessDPIAware.Call()
	}

	a := &app{
		mon: NewMonitor(*flagProcess, *flagStats),
	}
	sz, _, _ := pGetSystemMetrics.Call(smCxSmIcon)
	a.iconSize = int(sz)
	if a.iconSize < 16 {
		a.iconSize = 16
	}

	if err := a.createWindow(); err != nil {
		messageBox(0, err.Error(), appTitle, mbIconError)
		os.Exit(1)
	}
	a.tick(true)
	a.addTrayIcon()
	a.promoteOnce()
	pSetTimer.Call(a.hwnd, timerPoll, uintptr((*flagPoll).Milliseconds()), 0)

	var m msg
	for {
		r, _, _ := pGetMessageW.Call(uintptr(unsafe.Pointer(&m)), 0, 0, 0)
		if int32(r) <= 0 {
			break
		}
		pTranslateMessage.Call(uintptr(unsafe.Pointer(&m)))
		pDispatchMessageW.Call(uintptr(unsafe.Pointer(&m)))
	}
}

func (a *app) createWindow() error {
	hinst, _, _ := pGetModuleHandleW.Call(0)
	r, _, _ := pRegisterWindowMessageW.Call(uintptr(unsafe.Pointer(utf16("TaskbarCreated"))))
	a.taskbarCreate = uint32(r)

	cursor, _, _ := pLoadCursorW.Call(0, idcArrow)
	wc := wndClassExW{
		CbSize:        uint32(unsafe.Sizeof(wndClassExW{})),
		LpfnWndProc:   syscall.NewCallback(a.wndProc),
		HInstance:     hinst,
		HCursor:       cursor,
		LpszClassName: utf16(className),
	}
	if atom, _, _ := pRegisterClassExW.Call(uintptr(unsafe.Pointer(&wc))); atom == 0 {
		return fmt.Errorf("RegisterClassEx failed (%d)", lastError())
	}
	// A hidden top-level window: needed to receive tray callbacks and to own
	// the popup menu (message-only windows cannot be brought to foreground).
	hwnd, _, _ := pCreateWindowExW.Call(wsExOverlapped,
		uintptr(unsafe.Pointer(utf16(className))), uintptr(unsafe.Pointer(utf16(appTitle))),
		0, cwUseDefault, cwUseDefault, 0, 0, 0, 0, hinst, 0)
	if hwnd == 0 {
		return fmt.Errorf("CreateWindowEx failed (%d)", lastError())
	}
	a.hwnd = hwnd
	return nil
}

func (a *app) wndProc(hwnd, umsg, wparam, lparam uintptr) uintptr {
	switch uint32(umsg) {
	case wmTrayCallback:
		// NOTIFYICON_VERSION_4 semantics: left click -> NIN_SELECT, keyboard
		// -> NIN_KEYSELECT, right click -> WM_CONTEXTMENU. The raw mouse
		// messages also arrive and are deliberately ignored to avoid opening
		// the menu twice.
		switch loWord(lparam) {
		case wmContextMenu, ninSelect, ninKeySelect:
			a.showMenu()
		}
		return 0
	case wmTimer:
		if wparam == timerPoll {
			a.tick(false)
			a.promoteOnce()
		}
		return 0
	case wmRefreshNow:
		a.shuttingDown = false
		a.tick(true)
		return 0
	case wmClose:
		pDestroyWindow.Call(hwnd)
		return 0
	case wmDestroy:
		pKillTimer.Call(hwnd, timerPoll)
		pShellNotifyIconW.Call(nimDelete, uintptr(unsafe.Pointer(&a.nid)))
		if a.hicon != 0 {
			pDestroyIcon.Call(a.hicon)
		}
		pPostQuitMessage.Call(0)
		return 0
	}
	if a.taskbarCreate != 0 && uint32(umsg) == a.taskbarCreate {
		a.addTrayIcon() // Explorer restarted
		return 0
	}
	r, _, _ := pDefWindowProcW.Call(hwnd, umsg, wparam, lparam)
	return r
}

// tick polls WSL2 and refreshes icon + tooltip when something changed.
func (a *app) tick(force bool) {
	st, changed := a.mon.Poll(force)
	log.Printf("poll force=%v -> running=%v pid=%d cpu=%.2f mem=%s changed=%v", force, st.Running, st.PID, st.CPU, formatBytes(st.Mem), changed)
	if !changed && a.hicon != 0 {
		return
	}
	a.updateIcon(st)
}

func (a *app) updateIcon(st Status) {
	// Re-render the HICON only when its colour changes.
	lv := levelFor(st)
	if lv != a.lastLevel || a.hicon == 0 {
		hicon, _, err := renderIcon(a.iconSize, lv, false)
		if err == nil {
			if a.hicon != 0 {
				pDestroyIcon.Call(a.hicon)
			}
			a.hicon = hicon
			a.lastLevel = lv
		}
	}
	setTip(&a.nid, tooltip(st))
	a.nid.HIcon = a.hicon
	a.nid.UFlags = nifMessage | nifIcon | nifTip | nifShowTip
	pShellNotifyIconW.Call(nimModify, uintptr(unsafe.Pointer(&a.nid)))
}

func (a *app) addTrayIcon() {
	a.nid = notifyIconDataW{
		CbSize:           uint32(unsafe.Sizeof(notifyIconDataW{})),
		HWnd:             a.hwnd,
		UID:              1,
		UFlags:           nifMessage | nifIcon | nifTip | nifShowTip,
		UCallbackMessage: wmTrayCallback,
		HIcon:            a.hicon,
	}
	setTip(&a.nid, tooltip(a.mon.Current()))
	pShellNotifyIconW.Call(nimAdd, uintptr(unsafe.Pointer(&a.nid)))
	a.nid.UVersion = notifyIconVersion4
	pShellNotifyIconW.Call(nimSetVersion, uintptr(unsafe.Pointer(&a.nid)))
}

func tooltip(st Status) string {
	if !st.Running {
		return "WSL2: stopped"
	}
	s := "WSL2: running\n" + statsLine(st)
	if !st.Updated.IsZero() {
		s += "\nupdated " + st.Updated.Format("15:04:05")
	}
	return s
}

func statsLine(st Status) string {
	cpu := "..."
	if st.CPU >= 0 {
		cpu = fmt.Sprintf("%.1f %%", st.CPU)
	}
	return fmt.Sprintf("CPU %s   MEM %s (%.1f %%)", cpu, formatBytes(st.Mem), st.MemPct)
}

func setTip(nid *notifyIconDataW, s string) {
	u, _ := syscall.UTF16FromString(s)
	if len(u) > len(nid.SzTip) {
		u = u[:len(nid.SzTip)-1]
		u = append(u, 0)
	}
	clear(nid.SzTip[:])
	copy(nid.SzTip[:], u)
}

func (a *app) showMenu() {
	if a.menuOpen { // TrackPopupMenu pumps messages, so guard against re-entry
		return
	}
	a.menuOpen = true
	defer func() { a.menuOpen = false }()

	st := a.mon.Current()
	menu, _, _ := pCreatePopupMenu.Call()
	if menu == 0 {
		return
	}
	defer pDestroyMenu.Call(menu)
	add := func(flags uint32, id uintptr, text string) {
		pAppendMenuW.Call(menu, uintptr(flags), id, uintptr(unsafe.Pointer(utf16(text))))
	}

	if st.Running {
		add(mfString|mfGrayed, idmStatus, "WSL2 is running")
		add(mfString|mfGrayed, idmStats, statsLine(st))
	} else {
		add(mfString|mfGrayed, idmStatus, "WSL2 is stopped")
	}
	add(mfSeparator, 0, "")
	switch {
	case a.shuttingDown:
		add(mfString|mfGrayed, idmShutdown, "Shutting down...")
	case st.Running:
		add(mfString, idmShutdown, "&Shut down WSL2")
	default:
		add(mfString|mfGrayed, idmShutdown, "&Shut down WSL2")
	}
	add(mfString, idmRefresh, "&Refresh now")
	add(mfSeparator, 0, "")
	auto := uint32(mfString)
	if autostartEnabled() {
		auto |= mfChecked
	}
	add(auto, idmAutostart, "Start with &Windows")
	add(mfSeparator, 0, "")
	add(mfString, idmExit, "E&xit")

	var pt point
	pGetCursorPos.Call(uintptr(unsafe.Pointer(&pt)))
	pSetForegroundWindow.Call(a.hwnd) // required, otherwise the menu won't dismiss on focus loss
	cmd, _, _ := pTrackPopupMenuEx.Call(menu, tpmLeftAlign|tpmBottomAlign|tpmRightButton|tpmReturnCmd,
		uintptr(pt.X), uintptr(pt.Y), a.hwnd, 0)
	pPostMessageW.Call(a.hwnd, wmNull, 0, 0)

	log.Printf("menu command %d", cmd)
	switch cmd {
	case idmShutdown:
		a.shutdown()
	case idmRefresh:
		a.tick(true)
	case idmAutostart:
		if err := setAutostart(!autostartEnabled()); err != nil {
			messageBox(a.hwnd, err.Error(), appTitle, mbIconError)
		}
	case idmExit:
		pDestroyWindow.Call(a.hwnd)
	}
}

func (a *app) shutdown() {
	if a.shuttingDown {
		return
	}
	r := messageBox(a.hwnd, "Shut down WSL2?\n\nAll running distributions will be terminated.",
		appTitle, mbYesNo|mbIconQuestion|mbDefButton2)
	if r != idYes {
		return
	}
	a.shuttingDown = true
	hwnd := a.hwnd
	go func() {
		err := ShutdownWSL()
		if err != nil {
			messageBox(0, err.Error(), appTitle, mbIconError)
		}
		pPostMessageW.Call(hwnd, wmRefreshNow, 0, 0)
	}()
}

// promoteOnce asks Windows 11 to show the icon next to the clock instead of
// in the overflow flyout. Explorer keeps per-icon settings under
// HKCU\Control Panel\NotifyIconSettings\<id> with ExecutablePath + IsPromoted.
// The entry appears shortly after the icon is added, so this is retried a few
// times. IsPromoted is only written when absent, so a later manual choice in
// Settings > Taskbar is respected.
func (a *app) promoteOnce() {
	if a.promoted || a.promoteTries > 6 {
		return
	}
	a.promoteTries++
	exe, err := os.Executable()
	if err != nil {
		return
	}
	exe, _ = filepath.Abs(exe)
	a.promoted = promoteTrayIcon(exe)
}

func promoteTrayIcon(exe string) bool {
	const base = `Control Panel\NotifyIconSettings`
	root, err := regOpen(hkeyCurrentUser, base, keyRead)
	if err != nil {
		return true // no per-icon settings on this Windows version; nothing to do
	}
	defer pRegCloseKey.Call(root)
	for i := 0; ; i++ {
		var name [256]uint16
		n := uint32(len(name))
		r, _, _ := pRegEnumKeyExW.Call(root, uintptr(i), uintptr(unsafe.Pointer(&name[0])), uintptr(unsafe.Pointer(&n)), 0, 0, 0, 0)
		if r != 0 {
			return false // not found (yet)
		}
		sub := syscall.UTF16ToString(name[:n])
		k, err := regOpen(hkeyCurrentUser, base+`\`+sub, keyRead|keyWrite)
		if err != nil {
			continue
		}
		path, ok := regReadString(k, "ExecutablePath")
		if ok && strings.EqualFold(path, exe) {
			if _, has := regReadDword(k, "IsPromoted"); !has {
				v := uint32(1)
				pRegSetValueExW.Call(k, uintptr(unsafe.Pointer(utf16("IsPromoted"))), 0, regDWORD,
					uintptr(unsafe.Pointer(&v)), 4)
			}
			pRegCloseKey.Call(k)
			return true
		}
		pRegCloseKey.Call(k)
	}
}

// ---- registry helpers ----

func regOpen(hkey uintptr, sub string, access uint32) (uintptr, error) {
	var h uintptr
	r, _, _ := pRegOpenKeyExW.Call(hkey, uintptr(unsafe.Pointer(utf16(sub))), 0, uintptr(access), uintptr(unsafe.Pointer(&h)))
	if r != 0 {
		return 0, fmt.Errorf("cannot open %s (%d)", sub, r)
	}
	return h, nil
}

func regReadString(k uintptr, name string) (string, bool) {
	var typ uint32
	buf := make([]uint16, 1024)
	n := uint32(len(buf) * 2)
	r, _, _ := pRegQueryValueExW.Call(k, uintptr(unsafe.Pointer(utf16(name))), 0, uintptr(unsafe.Pointer(&typ)),
		uintptr(unsafe.Pointer(&buf[0])), uintptr(unsafe.Pointer(&n)))
	if r != 0 || (typ != regSZ && typ != regExpandSZ) {
		return "", false
	}
	return syscall.UTF16ToString(buf[:n/2]), true
}

func regReadDword(k uintptr, name string) (uint32, bool) {
	var typ, v uint32
	n := uint32(4)
	r, _, _ := pRegQueryValueExW.Call(k, uintptr(unsafe.Pointer(utf16(name))), 0, uintptr(unsafe.Pointer(&typ)),
		uintptr(unsafe.Pointer(&v)), uintptr(unsafe.Pointer(&n)))
	return v, r == 0 && typ == regDWORD
}

// ---- autostart (HKCU\...\Run) ----

const runKey = `Software\Microsoft\Windows\CurrentVersion\Run`
const runValue = "WSLTray"

func openRunKey(access uint32) (uintptr, error) { return regOpen(hkeyCurrentUser, runKey, access) }

func autostartEnabled() bool {
	h, err := openRunKey(keyRead)
	if err != nil {
		return false
	}
	defer pRegCloseKey.Call(h)
	var typ, n uint32
	r, _, _ := pRegQueryValueExW.Call(h, uintptr(unsafe.Pointer(utf16(runValue))), 0, uintptr(unsafe.Pointer(&typ)), 0, uintptr(unsafe.Pointer(&n)))
	return r == 0
}

func setAutostart(enable bool) error {
	h, err := openRunKey(keyRead | keyWrite)
	if err != nil {
		return err
	}
	defer pRegCloseKey.Call(h)
	if !enable {
		pRegDeleteValueW.Call(h, uintptr(unsafe.Pointer(utf16(runValue))))
		return nil
	}
	exe, err := os.Executable()
	if err != nil {
		return err
	}
	exe, _ = filepath.Abs(exe)
	val := `"` + exe + `"`
	if args := strings.Join(os.Args[1:], " "); args != "" {
		val += " " + args
	}
	u, _ := syscall.UTF16FromString(val)
	r, _, _ := pRegSetValueExW.Call(h, uintptr(unsafe.Pointer(utf16(runValue))), 0, regSZ,
		uintptr(unsafe.Pointer(&u[0])), uintptr(len(u)*2))
	if r != 0 {
		return fmt.Errorf("cannot write Run value (%d)", r)
	}
	return nil
}

// ---- -render-test: dump icons as PNG for a visual check ----

func renderTest(dir string) error {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	for _, sz := range []int{16, 20, 24, 32, 48, 256} {
		for _, c := range []struct {
			name string
			lv   level
		}{{"off", levelOff}, {"ok", levelOK}, {"warn", levelWarn}, {"high", levelHigh}} {
			hicon, pix, err := renderIcon(sz, c.lv, true)
			if err != nil {
				return err
			}
			pDestroyIcon.Call(hicon)
			f, err := os.Create(filepath.Join(dir, fmt.Sprintf("icon-%d-%s.png", sz, c.name)))
			if err != nil {
				return err
			}
			err = png.Encode(f, pixelsToImage(sz, pix))
			f.Close()
			if err != nil {
				return err
			}
		}
	}
	return nil
}
