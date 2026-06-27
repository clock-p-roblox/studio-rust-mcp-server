//go:build windows

package studio

import (
	"encoding/binary"
	"fmt"
	"net"
	"syscall"
	"unsafe"
)

const (
	afInet                = 2
	tcpTableOwnerPidAll   = 5
	peerPidRetryAttempts  = 10
	peerPidRetryDelayMs   = 50
	getExtendedTableNoErr = 0
)

var (
	iphlpapi                = syscall.NewLazyDLL("iphlpapi.dll")
	procGetExtendedTcpTable = iphlpapi.NewProc("GetExtendedTcpTable")
)

type mibTCPRowOwnerPID struct {
	State      uint32
	LocalAddr  uint32
	LocalPort  uint32
	RemoteAddr uint32
	RemotePort uint32
	OwningPID  uint32
}

func ResolvePeerProcessID(peerAddr net.Addr, helperPort int) (int, error) {
	tcpAddr, ok := peerAddr.(*net.TCPAddr)
	if !ok {
		return 0, nil
	}
	ip4 := tcpAddr.IP.To4()
	if ip4 == nil {
		return 0, nil
	}

	var size uint32
	procGetExtendedTcpTable.Call(0, uintptr(unsafe.Pointer(&size)), 0, uintptr(afInet), uintptr(tcpTableOwnerPidAll), 0)
	if size == 0 {
		return 0, fmt.Errorf("GetExtendedTcpTable did not report a buffer size")
	}

	buffer := make([]byte, size+uint32(unsafe.Sizeof(mibTCPRowOwnerPID{})))
	result, _, _ := procGetExtendedTcpTable.Call(
		uintptr(unsafe.Pointer(&buffer[0])),
		uintptr(unsafe.Pointer(&size)),
		0,
		uintptr(afInet),
		uintptr(tcpTableOwnerPidAll),
		0,
	)
	if result != getExtendedTableNoErr {
		return 0, fmt.Errorf("GetExtendedTcpTable failed with code %d", result)
	}

	count := binary.LittleEndian.Uint32(buffer[:4])
	rowSize := int(unsafe.Sizeof(mibTCPRowOwnerPID{}))
	offset := 4
	peerIP := net.IP(ip4).String()
	for i := uint32(0); i < count; i++ {
		row := (*mibTCPRowOwnerPID)(unsafe.Pointer(&buffer[offset]))
		localIP := decodeIPv4(row.LocalAddr)
		remoteIP := decodeIPv4(row.RemoteAddr)
		localPort := decodePort(row.LocalPort)
		remotePort := decodePort(row.RemotePort)
		if localIP == peerIP && localPort == tcpAddr.Port && remoteIP == "127.0.0.1" && remotePort == helperPort {
			return int(row.OwningPID), nil
		}
		offset += rowSize
	}

	return 0, nil
}

func decodeIPv4(value uint32) string {
	return net.IPv4(byte(value), byte(value>>8), byte(value>>16), byte(value>>24)).String()
}

func decodePort(value uint32) int {
	return int((value>>8)&0xff | (value & 0xff << 8))
}
