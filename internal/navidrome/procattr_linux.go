//go:build linux

package navidrome

import "syscall"

// childAttr puts the child in its own process group and has the kernel kill
// it when wn-server dies.
func childAttr() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{Setpgid: true, Pdeathsig: syscall.SIGKILL}
}
