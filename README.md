# clock-p Roblox agent

本文档只描述当前 Roblox agent 主线。新的联调入口由 `task-agent`、`helper2`、`plugin-mcp2`、`bridge2` 和内嵌 clockbridge 组成。

## 主线结构

```text
LLM / 脚本
  -> tools/bridge2/clockp-roblox-cli
  -> 工作区 .clock-p/session.json
  -> helper2
  -> plugin-mcp2
  -> Roblox Studio

task-agent
  -> 维护 .clock-p/session.json
  -> 启动 Rojo
  -> 向 helper2 心跳
  -> 在需要公网访问时注册 Rojo 诊断域名
```

职责边界：

- `task-agent`：工作区侧会话 owner。启动后写入 `.clock-p/session.json`，后续命令只读该文件，不再手动传会话身份。
- `helper2`：Windows 客户端侧稳定原语。负责 Studio 插件长轮询、task 会话、play/stop、截图、Studio 日志、Rojo 转发和官方 Studio MCP adapter。
- `plugin-mcp2`：Studio 内插件。只和 helper2 通信，负责执行 helper2 下发的 Studio 命令。
- `bridge2`：LLM / 脚本使用的 CLI 层。只做会话读取、JSON 输出和命令编排。
- `clockbridge`：公网转发能力。helper2 内嵌调用库，不依赖外部 `clockbridge-cli` 二进制。

## 本机身份文件

Windows 客户端身份文件统一放在：

```text
%APPDATA%\dev.clock-p.com\
```

当前使用：

- `machine_name`：helper2 读取，用于注册公网 helper 域名。
- `feishu-user_name`：helper2 读取，用于拼接公网用户名段。
- `feishu-token`：helper2 读取，用于 clockbridge 注册认证。

helper2 不再接受下面这些启动参数：

- `--public-machine-name`
- `--public-user`
- `--clockbridge-token-file`
- `--clockbridge-bin`
- `--clockbridge-x-token`
- `--clockbridge-register-host`
- `--clockbridge-register-ip`

`task-agent` 部署在服务端或工作区侧时，`machine_name` 必须由启动参数显式传入。`task-agent` 不读取本机 `machine_name` 文件；启动完成后写入 `.clock-p/session.json`，之后的 `bridge2` 命令从 session 文件取值。

## 启动流程

### 构建

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

### 启动 helper2

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\studio-helper.exe --addr 127.0.0.1:44750
```

`--register-domain` 默认开启。开启后 helper2 会按本机身份文件注册公网 helper 域名，同时保留本地监听端口。

### 启动 task-agent

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe start `
  --workspace K:\roblox_space\test_game3 `
  --place_id 113577273791190 `
  --machine_name sunjun2 `
  --helper-base-url http://127.0.0.1:<helper2_port> `
  --register-domain=false
```

`task-agent` 的 `--register-domain` 默认也是开启的。纯本地测试建议显式传 `--register-domain=false`，避免因为没有 `feishu-user_name` 或 `feishu-token` 身份文件而影响启动。

查看和停止当前 workspace 的 task-agent：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe status --workspace K:\roblox_space\test_game3
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe stop --workspace K:\roblox_space\test_game3
```

公网场景下，`task-agent` 必须显式传 `--machine_name`，并显式选择 `--environment public`：

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

`--user` 可省略，此时 task-agent 会读取本机 `feishu-user_name`。helper2 自身的 HTTP handler 不做 Bearer 鉴权；但当 `bridge2` 通过公网 `helper.base_url` 访问时，外层 `dev.clock-p.com` 入口仍要求 `Authorization: Bearer <feishu-token>`。`bridge2` 会自动从 workspace 或身份目录读取 `feishu-token` 注入。`--register-domain` 默认开启，会注册 Rojo 诊断域名，并在 `.clock-p/session.json` 中写入 `rojo.public_url`。

## session.json

`.clock-p/session.json` 是 bridge2 的唯一会话入口。典型字段：

```json
{
  "task_id": "t0123abcd45",
  "environment": "public",
  "machine_name": "sunjun2",
  "place_id": "113577273791190",
  "task_agent_pid": 12345,
  "task_agent_started_at_ms": 1780000000000,
  "task_agent_status_url": "http://127.0.0.1:32123/status",
  "helper": {
    "base_url": "https://roblox-helper-sunjun2-user-user.dev.clock-p.com",
    "public_url": "https://roblox-helper-sunjun2-user-user.dev.clock-p.com"
  },
  "rojo": {
    "local_url": "http://127.0.0.1:34872",
    "upstream_url": "https://113577273791190-t0123abcd45-rojo-user-user.dev.clock-p.com",
    "public_url": "https://113577273791190-t0123abcd45-rojo-user-user.dev.clock-p.com"
  }
}
```

约束：

- `helper.base_url` 是 bridge2 控制面的目标地址；公网验收时必须是真实公网 helper URL。
- `rojo.local_url` 是 task-agent 本机 Rojo 监听地址，只给 task-agent 自己和内嵌 clockbridge 使用。
- `rojo.upstream_url` 是 helper2 实际拨号的权威 Rojo 上游：本地模式下等于本地 `127.0.0.1`，公网模式下必须等于 helper2 可访问的公网 Rojo URL。
- `rojo.public_url` 只用于诊断公网注册结果，不作为 Studio 初始同步的权威判据。
- 后续命令不得重新拼 machine、user、token 或 task 身份。

## bridge2 CLI

入口脚本：

- `tools/bridge2/clockp-roblox-cli.cmd`
- `tools/bridge2/clockp-roblox-cli.sh`
- `tools/bridge2/cli.py`

所有命令只输出 JSON。成功和失败都必须是 JSON，便于 LLM 和脚本读取。

常用命令：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 status
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 mode
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 ensure-edit
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 play
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 stop
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 screenshot
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 play-mode-logs
```

Lua 执行：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 run-code-direct --file code.lua
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 run-code --file code.lua
```

语义：

- `run-code-direct` 只转发执行，不做模式切换。
- `run-code` 先确认 Studio 处于 edit 模式，再调用 `run-code-direct`。
- 每个子命令自己决定是否需要 ensure-edit；CLI 顶层不做默认 ensure。

官方 Studio MCP adapter 命令：

```text
official-ping
official-store-image
official-generate-mesh
official-generate-procedural-model
official-wait-job
official-search-creator-store
official-insert-from-creator-store
```

常见参数：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 official-store-image --file image.png
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 official-generate-mesh --text-prompt "small tree" --size-x 1 --size-y 2 --size-z 3
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 official-generate-procedural-model --prompt "wooden crate"
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 official-wait-job --generation-id <generation_id>
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 official-search-creator-store --query tree --asset-type Model --max-results 3
tools\bridge2\clockp-roblox-cli.cmd --workspace K:\roblox_space\test_game3 official-insert-from-creator-store --asset-id 123456789
```

需要 edit 模式的官方命令由对应子命令自己执行 ensure-edit。`official-generate-mesh`、`official-generate-procedural-model` 和 `official-insert-from-creator-store` 可用 `--no-ensure-edit` 跳过。

## helper2 HTTP 原语

当前 task-scoped 原语：

```text
POST /session/{task_id}/heartbeat
POST /session/{task_id}/release
GET  /session/{task_id}/status
GET  /session/{task_id}/studio/mode
POST /session/{task_id}/studio/play
POST /session/{task_id}/studio/stop
GET  /session/{task_id}/studio/screenshot
POST /session/{task_id}/studio/run-code-direct
GET  /session/{task_id}/runtime-log
POST /session/{task_id}/official/ping
POST /session/{task_id}/official/store-image
POST /session/{task_id}/official/generate-mesh
POST /session/{task_id}/official/generate-procedural-model
POST /session/{task_id}/official/wait-job
POST /session/{task_id}/official/search-creator-store
POST /session/{task_id}/official/insert-from-creator-store
```

Studio 日志读取是 helper2 能力。bridge2 对外命令名是 `play-mode-logs`，语义是从 helper2 读取当前 task/play 日志。

## helper2 MCP 工具

helper2 当前 MCP 工具面：

```text
helper2_status
helper2_studio_mode
helper2_studio_play
helper2_studio_stop
helper2_studio_screenshot
helper2_studio_run_code
helper2_runtime_log
helper2_official_ping
helper2_official_store_image
helper2_official_generate_mesh
helper2_official_generate_procedural_model
helper2_official_wait_job
helper2_official_search_creator_store
helper2_official_insert_from_creator_store
```

MCP 工具与 bridge2 都使用相同的 task-scoped helper2 语义。

## Rojo 与公网

Rojo 由 `task-agent` 启动和看护。helper2 通过 `rojo.upstream_url` 连接 Rojo，并向 Studio 插件提供本机转发入口。

公网验收时需要确认两件事：

- bridge2 控制面确实走 `helper.base_url` 的公网地址。
- Studio 日志里出现 Rojo initial sync 成功记录，证明游戏内容已实际同步进 Studio。

不能只用 HTTP 状态码判断 Rojo 是否可用；需要结合 Studio 日志确认初始同步完成。

## 测试工作区

当前测试工作区：

```text
K:\roblox_space\test_game3
```

当前测试 place：

```text
113577273791190
```

## 验证命令

Go 侧：

```powershell
cd K:\roblox_space\studio-rust-mcp-server\go-helper
go test -count=1 ./...
New-Item -ItemType Directory -Force bin | Out-Null
& 'K:\Program Files\Go\bin\go.exe' build -o bin\studio-helper.exe ./cmd/studio-helper
& 'K:\Program Files\Go\bin\go.exe' build -o bin\task-agent.exe ./cmd/task-agent
```

bridge2 Python：

```powershell
cd K:\roblox_space\studio-rust-mcp-server
py -3 -m py_compile tools\bridge2\cli.py
```

公网矩阵：

```powershell
cd K:\roblox_space\studio-rust-mcp-server
py -3 util\helper2_public_route_matrix_test.py `
  --place-id 113577273791190 `
  --kill-existing `
  --public-ready-timeout 120 `
  --initial-mode-timeout 240
```

公网矩阵必须覆盖：

- helper2 公网 URL 鉴权和 task 可见性。
- play / stop / screenshot 的 bridge2 直连路径。
- MCP play / stop / screenshot / log。
- Studio 日志里的 Rojo initial sync。

## 文档原则

以后新增文档只写当前主线。历史实现不作为入口、fallback 或验收依据。
