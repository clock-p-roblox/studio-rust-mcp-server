module github.com/clock-p-roblox/studio-rust-mcp-server/go-helper

go 1.22

require (
	github.com/clock-p/clockbridge v0.0.0
	github.com/zeebo/blake3 v0.2.4
)

require (
	github.com/gorilla/websocket v1.5.3 // indirect
	github.com/klauspost/cpuid/v2 v2.0.12 // indirect
)

replace github.com/clock-p/clockbridge => ../../https-proxy
