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

`task-agent` 部署在服务端或工作区侧时，`machine_name` 必须由启动参数显式传入。这里的 `--machine_name` 语义是“目标 Windows helper 的 machine_name”，也就是你前面说的 `aim_helper_machine_name`，不是 task-agent 当前这台机器的名字。helper2 自己的本地 `machine_name` 仍然来自 Windows 身份文件。`task-agent` 不读取本机 `machine_name` 文件；启动前会从 workspace 根的 `clock-p.workspace.json` 读取 `place_id` 与 code-sync 绑定；启动完成后写入 `.clock-p/session.json`，之后的 `bridge2` 命令从 session 文件取值。

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

Studio 侧插件必须使用本仓当前源码构建。更新 `plugin-mcp2` 后，先停止当前 Studio / task-agent，再按 Windows 侧当前插件打包流程生成并安装 `MCP2Plugin.rbxm`。

server 侧不再把插件打包工具作为 code-sync / flush 运行时依赖；本机只能做源码、Python、Go 与静态检查，最终插件包由 Windows 侧部署验证。

### 启动 helper2

```powershell
<mcp_repo>\go-helper\bin\studio-helper.exe --addr 127.0.0.1:44750
```

`--register-domain` 默认开启。开启后 helper2 会按本机身份文件注册公网 helper 域名，同时保留本地监听端口。

### 启动 task-agent

第一次接入一个 workspace，最少先确认这 2 个文件：

```text
clock-p.workspace.json
code-sync.tree.json
```

workspace 根必须先有：

```json
{
  "place_id": "93795519121520",
  "code_sync_config": "code-sync.tree.json"
}
```

文件名固定为 `clock-p.workspace.json`。当前主线推荐只显式传 `--workspace` 和 `--machine_name`；`feishu-user_name`、`feishu-token` 默认从身份目录读取。

说明：

- `place_id` 必填。
- `code_sync_config` 可省略；省略后默认去 workspace 根找 `code-sync.tree.json`。
- 所以如果你只写了 `place_id`，但默认 `code-sync.tree.json` 不存在，`task-agent` / `bridge2 code-sync-*` 仍会直接失败。
- `code-sync.tree.json` 负责按 Studio DataModel 树声明 code-sync 托管节点。

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --machine_name <machine_name> `
```

当前默认 `--environment public`。若要连本机 helper2 而不是公网 helper，显式传：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --machine_name <machine_name> `
  --environment local `
  --helper-url http://127.0.0.1:<helper2_port>
```

`local` 故意保持手工模式：你自己决定 helper2 的监听地址，再显式把 `--helper-url` 传给 task-agent；当前主线不额外封装这条路径。

推荐的新开发者最小顺序：

```text
1. Windows 侧准备 identity 文件
2. workspace 根写 clock-p.workspace.json
3. 确认 code-sync.tree.json 已存在
4. Windows 启动 helper2 并安装最新 plugin-mcp2
5. workspace 侧启动 task-agent
6. 运行 bridge2 status / mode
7. 运行 code-sync-apply
8. 运行 play
```


查看和停止当前 workspace 的 task-agent：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe status --workspace <workspace>
<mcp_repo>\go-helper\bin\task-agent.exe stop --workspace <workspace>
```

公网场景下，`task-agent` 仍然只需要显式传 `--workspace` 和 `--machine_name`；`--environment public` 是默认值：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --machine_name <machine_name>
```

public `task-agent` 会从 workspace 或本机身份目录读取 `feishu-user_name` 来推导公网 helper URL，并读取 `feishu-token` 注入 Bearer 鉴权。`--user` 只用于特殊覆盖，不是正常主线参数。

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
  "helper_url": "https://roblox-helper-sunjun2-user-user.dev.clock-p.com",
  "task_session_token": "opaque-random-token",
  "code_sync": {
    "protocol_version": 2,
    "workspace_id": "workspace-id",
    "place_id": "<place_id>",
    "machine_name": "<machine_name>",
    "mapping_profile": "sync_lua_v1",
    "code_sync_config_hash": "hex-hash",
    "target_authority_hash": "hex-hash",
    "config_path": "code-sync.tree.json",
    "targets": [
      {
        "studio_path": ["Workspace", "ClockPTest"]
      }
    ]
  }
}
```

约束：

- `helper_url` 是 bridge2 控制面的目标地址；公网验收时必须是真实公网 helper URL。
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
```

这里有两个 play 层级：bridge2 `play` 是标准 launch；helper2 / MCP 的 `studio_play` 是被 bridge2 `play` 调用的底层原语。

`play` 的成功判据是程序化状态链，不读日志：

- bridge2 生成本次随机 `launch_id`，随 play args 下发。
- helper2 response 必须 echo 同一个 `requested_launch_id`。
- plugin-mcp2 command result 必须 echo 同一个 `launch_id`。
- 最终 Studio mode 必须变为 `play_server`。
- 最终 `mode_seq` 必须不同于 play 前的 edit `mode_seq`。
- 最终 mode payload 的 `launch_id` 必须等于本次请求的 `launch_id`。

截图只作为可选视觉检查，不作为 play 成功的主判据。

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

## helper2 MCP 工具

helper2 当前 MCP 工具面：

```text
helper2_status
helper2_studio_mode
helper2_studio_play
helper2_studio_stop
helper2_studio_screenshot
helper2_studio_run_code
helper2_official_ping
helper2_official_store_image
helper2_official_generate_mesh
helper2_official_generate_procedural_model
helper2_official_wait_job
helper2_official_search_creator_store
helper2_official_insert_from_creator_store
```

MCP 工具和 bridge2 共享同一个 task-scoped helper2 控制通道，但语义层级不同：`helper2_studio_play` 只是标准 launch 的底层受理步骤，返回后仍需要继续查询 mode；正常启动游戏应走 bridge2 `play`，由它完成 `launch_id`、`mode_seq` 和 `play_server` 的完整验证。



公网验收时需要确认两件事：

- bridge2 控制面确实走 `helper_url` 的公网地址。


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

bridge2 Python：

```powershell
cd <mcp_repo>
py -3 -m py_compile tools\bridge2\cli.py
py -3 -m unittest discover tools\bridge2 -p "test*.py"
```

公网矩阵：

```powershell
cd <mcp_repo>
py -3 util\helper2_task_session_gate_test.py --place-id <place_id>
```

公网矩阵必须覆盖：

- helper2 公网 URL 鉴权和 task 可见性。
- play / stop / screenshot 的 bridge2 直连路径。
- MCP play / stop / screenshot。

## 文档原则

以后新增文档只写当前主线，不引用历史实现作为操作依据。
