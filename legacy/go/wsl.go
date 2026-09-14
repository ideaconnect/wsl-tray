//go:build windows

package main

import (
	"fmt"
	"os/exec"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

// Status is what the UI shows.
type Status struct {
	Running bool
	PID     uintptr
	CPU     float64 // percent of all host cores; <0 = not measured yet
	Mem     uint64  // working set of the WSL2 VM process, bytes
	MemPct  float64 // Mem as a percentage of the host's physical RAM
	Updated time.Time
}

// Monitor samples the WSL2 utility-VM process ("vmmemWSL") through
// NtQuerySystemInformation. That call needs no process handle, so it works
// for a standard (non-elevated) user even though vmmemWSL runs as SYSTEM.
type Monitor struct {
	procName   string
	statsEvery time.Duration
	ncpu       float64
	totalMem   float64 // host physical RAM, bytes

	cur     Status
	lastT   time.Time // wall clock of the last CPU baseline
	lastCPU int64     // kernel+user time (100 ns units) at lastT
	lastPID uintptr
	buf     []byte
}

func NewMonitor(procName string, statsEvery time.Duration) *Monitor {
	return &Monitor{
		procName:   strings.ToLower(procName),
		statsEvery: statsEvery,
		ncpu:       float64(numCPU()),
		totalMem:   float64(totalPhysMem()),
		cur:        Status{CPU: -1},
		buf:        make([]byte, 512*1024),
	}
}

func (m *Monitor) Current() Status { return m.cur }

// Poll checks whether the VM process exists (cheap) and, when it does and the
// stats interval has elapsed (or force is set), refreshes CPU/memory.
// Returns the status and whether anything visible changed.
func (m *Monitor) Poll(force bool) (Status, bool) {
	now := time.Now()
	pid, cpuTime, ws, found := m.find()

	if !found {
		changed := m.cur.Running
		m.cur = Status{CPU: -1, Updated: now}
		m.lastPID = 0
		return m.cur, changed
	}

	// Newly started (or restarted) VM: establish a CPU baseline, show memory now.
	if !m.cur.Running || pid != m.lastPID {
		m.cur = Status{Running: true, PID: pid, CPU: -1, Mem: ws, MemPct: m.memPct(ws), Updated: now}
		m.lastPID, m.lastT, m.lastCPU = pid, now, cpuTime
		return m.cur, true
	}

	// Steady state: refresh every statsEvery. Right after a baseline (CPU still
	// unknown) the next poll is allowed through so a first reading appears
	// quickly instead of after a full interval.
	elapsed := now.Sub(m.lastT)
	if !force && elapsed < m.statsEvery && m.cur.CPU >= 0 {
		return m.cur, false
	}
	if elapsed >= time.Second { // too short a window gives meaningless numbers
		delta := float64(cpuTime-m.lastCPU) * 100 // 100 ns units -> ns
		pct := delta / float64(elapsed.Nanoseconds()) / m.ncpu * 100
		if pct < 0 {
			pct = 0
		}
		m.cur.CPU = pct
		m.lastT, m.lastCPU = now, cpuTime
	}
	m.cur.Mem = ws
	m.cur.MemPct = m.memPct(ws)
	m.cur.Updated = now
	return m.cur, true
}

func (m *Monitor) memPct(b uint64) float64 {
	if m.totalMem <= 0 {
		return 0
	}
	return float64(b) / m.totalMem * 100
}

// SYSTEM_PROCESS_INFORMATION (x64 layout).
type unicodeString struct {
	Length        uint16
	MaximumLength uint16
	_             [4]byte
	Buffer        *uint16
}

type sysProcInfo struct {
	NextEntryOffset              uint32
	NumberOfThreads              uint32
	WorkingSetPrivateSize        int64
	HardFaultCount               uint32
	NumberOfThreadsHighWatermark uint32
	CycleTime                    uint64
	CreateTime                   int64
	UserTime                     int64
	KernelTime                   int64
	ImageName                    unicodeString
	BasePriority                 int32
	_                            [4]byte
	UniqueProcessId              uintptr
	InheritedFromUniqueProcessId uintptr
	HandleCount                  uint32
	SessionId                    uint32
	UniqueProcessKey             uintptr
	PeakVirtualSize              uintptr
	VirtualSize                  uintptr
	PageFaultCount               uint32
	_                            [4]byte
	PeakWorkingSetSize           uintptr
	WorkingSetSize               uintptr
	QuotaPeakPagedPoolUsage      uintptr
	QuotaPagedPoolUsage          uintptr
	QuotaPeakNonPagedPoolUsage   uintptr
	QuotaNonPagedPoolUsage       uintptr
	PagefileUsage                uintptr
	PeakPagefileUsage            uintptr
	PrivatePageCount             uintptr
}

const (
	systemProcessInformation = 5
	statusInfoLengthMismatch = 0xC0000004
)

// find walks the process list and returns the first entry whose image name
// matches m.procName (case-insensitive).
func (m *Monitor) find() (pid uintptr, cpuTime int64, ws uint64, ok bool) {
	var needed uint32
	for {
		st, _, _ := pNtQuerySystemInformation.Call(systemProcessInformation,
			uintptr(unsafe.Pointer(&m.buf[0])), uintptr(len(m.buf)), uintptr(unsafe.Pointer(&needed)))
		if uint32(st) == 0 {
			break
		}
		if uint32(st) != statusInfoLengthMismatch {
			return 0, 0, 0, false
		}
		m.buf = make([]byte, int(needed)+64*1024)
	}

	off := 0
	for {
		p := (*sysProcInfo)(unsafe.Pointer(&m.buf[off]))
		if p.ImageName.Buffer != nil && p.ImageName.Length > 0 {
			n := int(p.ImageName.Length / 2)
			name := syscall.UTF16ToString(unsafe.Slice(p.ImageName.Buffer, n))
			if strings.ToLower(name) == m.procName {
				return p.UniqueProcessId, p.UserTime + p.KernelTime, uint64(p.WorkingSetSize), true
			}
		}
		if p.NextEntryOffset == 0 {
			return 0, 0, 0, false
		}
		off += int(p.NextEntryOffset)
	}
}

// ShutdownWSL runs `wsl.exe --shutdown` with no console window.
func ShutdownWSL() error {
	cmd := exec.Command("wsl.exe", "--shutdown")
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true}
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("wsl --shutdown: %v (%s)", err, strings.TrimSpace(string(out)))
	}
	return nil
}

func formatBytes(b uint64) string {
	const mb = 1 << 20
	if b < 1024*mb {
		return fmt.Sprintf("%d MB", b/mb)
	}
	return fmt.Sprintf("%.2f GB", float64(b)/float64(1<<30))
}
