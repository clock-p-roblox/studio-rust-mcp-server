---
name: roblox-agent
description: Use when starting, stopping, inspecting, testing, or documenting the current clock-p Roblox agent based on task-agent, helper2, plugin-mcp2, bridge2, session.json, and embedded clockbridge public routing.
---

# Roblox Agent

本 skill 是当前 Roblox 联调主线入口。先读仓库根 `README.md`；如果本文和 README 冲突，以 README 与代码为准。

## 当前主线

```text
LLM / 脚本
  -> tools/bridge2/clockp-roblox-cli
  -> workspace/.clock-p/session.json
  -> helper2
  -> plugin-mcp2
  -> Roblox Studio

task-agent
  -> 写 session.json
  -> 启动 Rojo
  -> 向 helper2 心跳
```

不要使用其他 Roblox 联调入口作为 fallback。不要新写 wrapper。

## 身份文件

helper2 在 Windows 客户端读取：

```text
%APPDATA%\dev.clock-p.com\machine_name
%APPDATA%\dev.clock-p.com\feishu-user_name
%APPDATA%\dev.clock-p.com\feishu-token
```

helper2 不接受这些人工覆盖参数：

- `--public-machine-name`
- `--public-user`
- `--clockbridge-token-file`
- `--clockbridge-bin`
- `--clockbridge-x-token`
- `--clockbridge-register-host`
- `--clockbridge-register-ip`

task-agent 的规则相反：`--machine_name` 必须显式传入，不读取本机 `machine_name` 文件。task-agent 启动后，后续命令只读 `.clock-p/session.json`。

## 构建

helper2 必须使用本仓当前源码 build 出来的本地二进制：

```text
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\studio-helper.exe
```

不要使用公网矩阵测试临时目录里的 `studio-helper.exe`，也不要继续启动旧的 `go-helper\bin` 残留二进制。启动前先重新 build。当前 Windows 测试机的 Go 在 `K:\Program Files\Go\bin\go.exe`；如果 PATH 已配置好，也可以直接用 `go`。

```powershell
cd K:\roblox_space\studio-rust-mcp-server\go-helper
New-Item -ItemType Directory -Force bin | Out-Null
& 'K:\Program Files\Go\bin\go.exe' build -o bin\studio-helper.exe ./cmd/studio-helper
& 'K:\Program Files\Go\bin\go.exe' build -o bin\task-agent.exe ./cmd/task-agent
```

## 本地启动

启动 helper2：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\studio-helper.exe --addr 127.0.0.1:44750
```

启动 task-agent：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe start `
  --workspace K:\roblox_space\test_game3 `
  --place_id 113577273791190 `
  --machine_name sunjun2 `
  --helper-base-url http://127.0.0.1:<helper2_port> `
  --register-domain=false
```

`task-agent` 的 `--register-domain` 默认也是开启的。纯本地测试建议显式传 `--register-domain=false`。

查看和停止：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe status --workspace K:\roblox_space\test_game3
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe stop --workspace K:\roblox_space\test_game3
```

## 公网启动

helper2 默认 `--register-domain=true`。task-agent 公网模式示例：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe start `
  --workspace K:\roblox_space\test_game3 `
  --environment public `
  --place_id 113577273791190 `
  --machine_name sunjun2 `
  --user <feishu-user_name> `
  --rojo-bin K:\roblox_space\rojo\target\release\rojo.exe `
  --project K:\roblox_space\test_game3\default.project.json
```

helper2 的 HTTP 请求不做 Bearer 鉴权；`feishu-token` 只用于 helper2 和 task-agent 注册 clockbridge 公网域名。

公网验收必须确认 bridge2 控制面走 `helper.base_url` 公网地址，并且 Studio 日志出现 Rojo initial sync 成功记录。不要只看 HTTP 状态码。

## bridge2 命令

`--workspace` 是顶层参数，必须写在子命令前。所有命令只输出 JSON。

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 status
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 mode
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 ensure-edit
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 play
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 stop
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 screenshot
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 play-mode-logs
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 run-code-direct --file code.lua
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 run-code --file code.lua
```

`run-code-direct` 不做模式切换。`run-code` 会先 ensure edit。需要 edit 的子命令自己决定是否 ensure；CLI 顶层不做全局 ensure。

官方 adapter 命令：

- `official-ping`
- `official-store-image --file image.png`
- `official-generate-mesh --text-prompt "small tree" --size-x 1 --size-y 2 --size-z 3`
- `official-generate-procedural-model --prompt "wooden crate"`
- `official-wait-job --generation-id <generation_id>`
- `official-search-creator-store --query tree --asset-type Model --max-results 3`
- `official-insert-from-creator-store --asset-id 123456789`

`official-generate-mesh`、`official-generate-procedural-model`、`official-insert-from-creator-store` 默认 ensure edit，可用 `--no-ensure-edit` 跳过。

## helper2 MCP

当前工具：

- `helper2_status`
- `helper2_studio_mode`
- `helper2_studio_play`
- `helper2_studio_stop`
- `helper2_studio_screenshot`
- `helper2_studio_run_code`
- `helper2_runtime_log`
- `helper2_official_ping`
- `helper2_official_store_image`
- `helper2_official_generate_mesh`
- `helper2_official_generate_procedural_model`
- `helper2_official_wait_job`
- `helper2_official_search_creator_store`
- `helper2_official_insert_from_creator_store`

## 验证

文档或脚本改动后至少跑：

```powershell
cd K:\roblox_space\studio-rust-mcp-server
py -3 -m py_compile tools\bridge2\cli.py
cd go-helper
go test -count=1 ./...
```

涉及公网路由时跑：

```powershell
cd K:\roblox_space\studio-rust-mcp-server
py -3 util\helper2_public_route_matrix_test.py `
  --place-id 113577273791190 `
  --kill-existing `
  --public-ready-timeout 120 `
  --initial-mode-timeout 240
```
