//go:build !linux

package navidrome

import "syscall"

// childAttr puts the child in its own process group. Pdeathsig exists only on
// Linux.
func childAttr() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{Setpgid: true}
}
