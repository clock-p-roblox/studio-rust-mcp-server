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
  -> 向 helper2 心跳
```

职责边界：

- `task-agent`：工作区侧会话 owner。启动后写入 `.clock-p/session.json`，后续命令只读该文件，不再手动传会话身份。
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
<mcp_repo>\go-helper\bin\studio-helper.exe
```

不要使用公网矩阵测试临时目录里的 `studio-helper.exe`。启动前先重新 build，并确保 `go` 在 PATH 中。

```powershell
cd <mcp_repo>\go-helper
New-Item -ItemType Directory -Force bin | Out-Null
go build -o bin\studio-helper.exe ./cmd/studio-helper
go build -o bin\task-agent.exe ./cmd/task-agent
```

### 构建并安装 plugin-mcp2

Studio 侧插件也必须使用本仓当前源码构建。更新 `plugin-mcp2` 后，先停止当前 Studio / task-agent，再安装插件：

```powershell
cd <mcp_repo>\plugin-mcp2
rojo build default.project.json --plugin MCP2Plugin.rbxm
```

如果 `rojo` 不在 PATH 中，使用本机实际的 `rojo.exe` 路径执行同一条 build 命令。

### 启动 helper2

```powershell
<mcp_repo>\go-helper\bin\studio-helper.exe --addr 127.0.0.1:44750
```

`--register-domain` 默认开启。开启后 helper2 会按本机身份文件注册公网 helper 域名，同时保留本地监听端口。

### 启动 task-agent

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --place_id <place_id> `
  --machine_name <machine_name> `
  --helper-base-url http://127.0.0.1:<helper2_port> `
  --code-sync-config code-sync.roots.json `
  --code-sync-project default.project.json
```

测试者必须显式提供 `--workspace` 和 `--place_id`。文档、脚本和测试入口都不要内置固定测试 place。


查看和停止当前 workspace 的 task-agent：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe status --workspace <workspace>
<mcp_repo>\go-helper\bin\task-agent.exe stop --workspace <workspace>
```

公网场景下，`task-agent` 必须显式传 `--machine_name`，并显式选择 `--environment public`：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --environment public `
  --place_id <place_id> `
  --machine_name <machine_name> `
  --user <feishu-user_name> `
  --code-sync-config code-sync.roots.json `
  --code-sync-project default.project.json
```


## session.json

`.clock-p/session.json` 是 bridge2 的唯一会话入口。典型字段：

```json
{
  "task_id": "t0123abcd45",
  "environment": "public",
  "machine_name": "<machine_name>",
  "place_id": "<place_id>",
  "task_agent_pid": 12345,
  "task_agent_started_at_ms": 1780000000000,
  "task_agent_status_url": "http://127.0.0.1:32123/status",
  "helper": {
    "base_url": "https://roblox-helper-sunjun2-user-user.dev.clock-p.com",
    "public_url": "https://roblox-helper-sunjun2-user-user.dev.clock-p.com"
  },
  "task_session_token": "opaque-random-token",
  "code_sync": {
    "protocol_version": 1,
    "workspace_id": "workspace-id",
    "place_id": "<place_id>",
    "machine_name": "<machine_name>",
    "project_id": "<project_id>",
    "mapping_profile": "sync_lua_v1",
    "code_sync_config_hash": "hex-hash",
    "roots_authority_hash": "hex-hash",
    "config_path": "code-sync.roots.json",
    "project_path": "default.project.json",
    "roots": [
      {
        "root_id": "workspace-test",
        "studio_path": ["Workspace", "ClockPTest"]
      }
    ]
  }
}
```

约束：

- `helper.base_url` 是 bridge2 控制面的目标地址；公网验收时必须是真实公网 helper URL。
- 后续命令不得重新拼 machine、user、token 或 task 身份。

## bridge2 CLI

入口脚本：

- `tools/bridge2/clockp-roblox-cli.cmd`
- `tools/bridge2/clockp-roblox-cli.sh`
- `tools/bridge2/cli.py`

所有命令只输出 JSON。成功和失败都必须是 JSON，便于 LLM 和脚本读取。

常用命令：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> status
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> mode
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> ensure-edit
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> stop
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> screenshot
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play-mode-logs
```

代码 flush：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-manifest
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-live-manifest
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-dry-run
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-apply
```


Lua 执行：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> run-code-direct --file code.lua
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> run-code --file code.lua
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
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> official-store-image --file image.png
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> official-generate-mesh --text-prompt "small tree" --size-x 1 --size-y 2 --size-z 3
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> official-generate-procedural-model --prompt "wooden crate"
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> official-wait-job --generation-id <generation_id>
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> official-search-creator-store --query tree --asset-type Model --max-results 3
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> official-insert-from-creator-store --asset-id 123456789
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
POST /session/{task_id}/code-sync/get-manifest
POST /session/{task_id}/code-sync/apply
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



公网验收时需要确认两件事：

- bridge2 控制面确实走 `helper.base_url` 的公网地址。


## 测试工作区

测试者必须提供工作区和 place，不允许在测试入口里内置固定测试 place。本机这次可用的测试值：

```text
workspace: D:\roblox_space\test_game2
place_id: 105986423068266
```

## 验证命令

Go 侧：

```powershell
cd <mcp_repo>\go-helper
go test -count=1 ./...
New-Item -ItemType Directory -Force bin | Out-Null
go build -o bin\studio-helper.exe ./cmd/studio-helper
go build -o bin\task-agent.exe ./cmd/task-agent
```

plugin-mcp2：

```powershell
cd <mcp_repo>\plugin-mcp2
rojo build default.project.json -o ..\output\MCP2Plugin.rbxm
```

bridge2 Python：

```powershell
cd <mcp_repo>
py -3 -m py_compile tools\bridge2\cli.py
py -3 -m unittest discover tools\bridge2 -p "test*.py"
```

公网矩阵：

```powershell
cd <mcp_repo>
py -3 util\helper2_task_session_gate_test.py
```

公网矩阵必须覆盖：

- helper2 公网 URL 鉴权和 task 可见性。
- play / stop / screenshot 的 bridge2 直连路径。
- MCP play / stop / screenshot / log。

## 文档原则

以后新增文档只写当前主线，不引用历史实现作为操作依据。
