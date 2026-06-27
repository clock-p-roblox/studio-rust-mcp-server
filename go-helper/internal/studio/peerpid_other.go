//go:build !windows

package studio

import "net"

func ResolvePeerProcessID(peerAddr net.Addr, helperPort int) (int, error) {
	return 0, nil
}
