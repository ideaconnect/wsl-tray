//go:build windows

package main

import (
	"syscall"
	"unsafe"
)

// Raw Win32 bindings. Only stdlib syscall is used so the binary stays a single
// dependency-free executable.

var (
	user32   = syscall.NewLazyDLL("user32.dll")
	kernel32 = syscall.NewLazyDLL("kernel32.dll")
	shell32  = syscall.NewLazyDLL("shell32.dll")
	gdi32    = syscall.NewLazyDLL("gdi32.dll")
	advapi32 = syscall.NewLazyDLL("advapi32.dll")
	ntdll    = syscall.NewLazyDLL("ntdll.dll")

	pRegisterClassExW              = user32.NewProc("RegisterClassExW")
	pCreateWindowExW               = user32.NewProc("CreateWindowExW")
	pDefWindowProcW                = user32.NewProc("DefWindowProcW")
	pGetMessageW                   = user32.NewProc("GetMessageW")
	pTranslateMessage              = user32.NewProc("TranslateMessage")
	pDispatchMessageW              = user32.NewProc("DispatchMessageW")
	pPostMessageW                  = user32.NewProc("PostMessageW")
	pPostQuitMessage               = user32.NewProc("PostQuitMessage")
	pDestroyWindow                 = user32.NewProc("DestroyWindow")
	pSetTimer                      = user32.NewProc("SetTimer")
	pKillTimer                     = user32.NewProc("KillTimer")
	pCreatePopupMenu               = user32.NewProc("CreatePopupMenu")
	pAppendMenuW                   = user32.NewProc("AppendMenuW")
	pDestroyMenu                   = user32.NewProc("DestroyMenu")
	pTrackPopupMenuEx              = user32.NewProc("TrackPopupMenuEx")
	pSetForegroundWindow           = user32.NewProc("SetForegroundWindow")
	pGetCursorPos                  = user32.NewProc("GetCursorPos")
	pMessageBoxW                   = user32.NewProc("MessageBoxW")
	pRegisterWindowMessageW        = user32.NewProc("RegisterWindowMessageW")
	pGetSystemMetrics              = user32.NewProc("GetSystemMetrics")
	pSetProcessDpiAwarenessContext = user32.NewProc("SetProcessDpiAwarenessContext")
	pSetProcessDPIAware            = user32.NewProc("SetProcessDPIAware")
	pDestroyIcon                   = user32.NewProc("DestroyIcon")
	pCreateIconIndirect            = user32.NewProc("CreateIconIndirect")
	pGetDC                         = user32.NewProc("GetDC")
	pReleaseDC                     = user32.NewProc("ReleaseDC")
	pLoadCursorW                   = user32.NewProc("LoadCursorW")

	pGetModuleHandleW     = kernel32.NewProc("GetModuleHandleW")
	pCreateMutexW         = kernel32.NewProc("CreateMutexW")
	pGetLastError         = kernel32.NewProc("GetLastError")
	pGetSystemInfo        = kernel32.NewProc("GetSystemInfo")
	pGlobalMemoryStatusEx = kernel32.NewProc("GlobalMemoryStatusEx")

	pShellNotifyIconW = shell32.NewProc("Shell_NotifyIconW")

	pCreateCompatibleDC = gdi32.NewProc("CreateCompatibleDC")
	pDeleteDC           = gdi32.NewProc("DeleteDC")
	pCreateDIBSection   = gdi32.NewProc("CreateDIBSection")
	pCreateBitmap       = gdi32.NewProc("CreateBitmap")
	pSelectObject       = gdi32.NewProc("SelectObject")
	pDeleteObject       = gdi32.NewProc("DeleteObject")

	pRegOpenKeyExW    = advapi32.NewProc("RegOpenKeyExW")
	pRegSetValueExW   = advapi32.NewProc("RegSetValueExW")
	pRegQueryValueExW = advapi32.NewProc("RegQueryValueExW")
	pRegDeleteValueW  = advapi32.NewProc("RegDeleteValueW")
	pRegEnumKeyExW    = advapi32.NewProc("RegEnumKeyExW")
	pRegCloseKey      = advapi32.NewProc("RegCloseKey")

	pNtQuerySystemInformation = ntdll.NewProc("NtQuerySystemInformation")
)

const (
	wmDestroy     = 0x0002
	wmClose       = 0x0010
	wmCommand     = 0x0111
	wmTimer       = 0x0113
	wmContextMenu = 0x007B
	wmLButtonUp   = 0x0202
	wmRButtonUp   = 0x0205
	wmNull        = 0x0000
	wmApp         = 0x8000

	ninSelect    = 0x0400
	ninKeySelect = 0x0401

	nimAdd        = 0
	nimModify     = 1
	nimDelete     = 2
	nimSetVersion = 4

	nifMessage         = 0x01
	nifIcon            = 0x02
	nifTip             = 0x04
	nifShowTip         = 0x80
	notifyIconVersion4 = 4

	mfString    = 0x0000
	mfGrayed    = 0x0001
	mfChecked   = 0x0008
	mfSeparator = 0x0800

	tpmLeftAlign   = 0x0000
	tpmBottomAlign = 0x0020
	tpmRightButton = 0x0002
	tpmReturnCmd   = 0x0100

	mbYesNo        = 0x0004
	mbIconQuestion = 0x0020
	mbIconError    = 0x0010
	mbDefButton2   = 0x0100
	idYes          = 6

	smCxSmIcon = 49

	dibRGBColors = 0
	biRGB        = 0

	hkeyCurrentUser = 0x80000001
	keyRead         = 0x20019
	keyWrite        = 0x20006
	regSZ           = 1
	regExpandSZ     = 2
	regDWORD        = 4

	errorAlreadyExists = 183
	wsExOverlapped     = 0
	cwUseDefault       = 0x80000000
	idcArrow           = 32512
)

type wndClassExW struct {
	CbSize        uint32
	Style         uint32
	LpfnWndProc   uintptr
	CbClsExtra    int32
	CbWndExtra    int32
	HInstance     uintptr
	HIcon         uintptr
	HCursor       uintptr
	HbrBackground uintptr
	LpszMenuName  *uint16
	LpszClassName *uint16
	HIconSm       uintptr
}

type point struct{ X, Y int32 }

type msg struct {
	HWnd    uintptr
	Message uint32
	WParam  uintptr
	LParam  uintptr
	Time    uint32
	Pt      point
}

type guid struct {
	Data1 uint32
	Data2 uint16
	Data3 uint16
	Data4 [8]byte
}

type notifyIconDataW struct {
	CbSize           uint32
	HWnd             uintptr
	UID              uint32
	UFlags           uint32
	UCallbackMessage uint32
	HIcon            uintptr
	SzTip            [128]uint16
	DwState          uint32
	DwStateMask      uint32
	SzInfo           [256]uint16
	UVersion         uint32
	SzInfoTitle      [64]uint16
	DwInfoFlags      uint32
	GuidItem         guid
	HBalloonIcon     uintptr
}

type bitmapInfoHeader struct {
	Size          uint32
	Width         int32
	Height        int32
	Planes        uint16
	BitCount      uint16
	Compression   uint32
	SizeImage     uint32
	XPelsPerMeter int32
	YPelsPerMeter int32
	ClrUsed       uint32
	ClrImportant  uint32
}

type bitmapInfo struct {
	Header bitmapInfoHeader
	Colors [1]uint32
}

type iconInfo struct {
	FIcon    int32
	XHotspot uint32
	YHotspot uint32
	HbmMask  uintptr
	HbmColor uintptr
}

type systemInfo struct {
	OemID                  uint32
	PageSize               uint32
	MinAppAddr, MaxAppAddr uintptr
	ActiveProcessorMask    uintptr
	NumberOfProcessors     uint32
	ProcessorType          uint32
	AllocationGranularity  uint32
	ProcessorLevel         uint16
	ProcessorRevision      uint16
}

func utf16(s string) *uint16 {
	p, _ := syscall.UTF16PtrFromString(s)
	return p
}

func lastError() uint32 {
	r, _, _ := pGetLastError.Call()
	return uint32(r)
}

func loWord(v uintptr) uint32 { return uint32(v) & 0xFFFF }

type memoryStatusEx struct {
	Length               uint32
	MemoryLoad           uint32
	TotalPhys            uint64
	AvailPhys            uint64
	TotalPageFile        uint64
	AvailPageFile        uint64
	TotalVirtual         uint64
	AvailVirtual         uint64
	AvailExtendedVirtual uint64
}

// totalPhysMem returns the machine's physical RAM in bytes (0 on failure).
func totalPhysMem() uint64 {
	ms := memoryStatusEx{Length: uint32(unsafe.Sizeof(memoryStatusEx{}))}
	if r, _, _ := pGlobalMemoryStatusEx.Call(uintptr(unsafe.Pointer(&ms))); r == 0 {
		return 0
	}
	return ms.TotalPhys
}

func numCPU() int {
	var si systemInfo
	pGetSystemInfo.Call(uintptr(unsafe.Pointer(&si)))
	if si.NumberOfProcessors == 0 {
		return 1
	}
	return int(si.NumberOfProcessors)
}

func messageBox(hwnd uintptr, text, caption string, flags uint32) int {
	r, _, _ := pMessageBoxW.Call(hwnd, uintptr(unsafe.Pointer(utf16(text))), uintptr(unsafe.Pointer(utf16(caption))), uintptr(flags))
	return int(r)
}
