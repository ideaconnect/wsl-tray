//go:build windows

package main

import (
	"testing"
	"time"
)

// Exercises the live sampler against whatever is running on this machine.
// Uses a well-known always-present process when the WSL2 VM is not running.
func TestMonitorPoll(t *testing.T) {
	for _, name := range []string{"vmmemWSL", "explorer.exe"} {
		m := NewMonitor(name, 30*time.Second)
		st, changed := m.Poll(true)
		t.Logf("%s: first poll -> running=%v pid=%d cpu=%.2f mem=%s changed=%v", name, st.Running, st.PID, st.CPU, formatBytes(st.Mem), changed)
		if !st.Running {
			continue
		}
		if st.CPU != -1 {
			t.Errorf("%s: expected CPU unknown (-1) after baseline, got %v", name, st.CPU)
		}
		time.Sleep(1500 * time.Millisecond)
		st, changed = m.Poll(true)
		t.Logf("%s: second poll -> cpu=%.2f mem=%s changed=%v", name, st.CPU, formatBytes(st.Mem), changed)
		if st.CPU < 0 {
			t.Errorf("%s: expected a CPU measurement after forced second poll", name)
		}
		if !changed {
			t.Errorf("%s: forced poll should report a change", name)
		}
		// Unforced poll inside the stats interval must be a no-op.
		_, changed = m.Poll(false)
		if changed {
			t.Errorf("%s: unforced poll within interval should not report a change", name)
		}
	}
}

func TestFormatBytes(t *testing.T) {
	cases := map[uint64]string{
		600 << 20:         "600 MB",
		1024 << 20:        "1.00 GB",
		(5 << 30) + 1<<29: "5.50 GB",
	}
	for in, want := range cases {
		if got := formatBytes(in); got != want {
			t.Errorf("formatBytes(%d) = %q, want %q", in, got, want)
		}
	}
}
