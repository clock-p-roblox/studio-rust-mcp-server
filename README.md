# clock-p Roblox agent / helper2

本仓当前主线是新版 Roblox agent 本地链路：

```text
task-agent -> helper2 -> mcp2
```

旧 `helper1`、旧 `task-server`、旧 `hub`、旧 Rust clockp MCP server / `mcp1` 代码暂时保留，但不再作为当前实现入口、兼容 fallback 或验收依据。除非任务明确要求追溯旧实现，否则不要按旧链路开发、测试或写文档。

## 当前组件

- `go-helper/cmd/studio-helper`：helper2。负责本机 task session、task-owned Studio 生命周期、mcp2 command broker、截图、日志读取、helper2 MCP。
- `go-helper/cmd/task-agent`：workspace agent。负责 `.clock-p/session.json`、Rojo server、Rojo 公网域名注册、helper2 heartbeat、shutdown/release。
- `plugin-mcp2`：Studio 插件。只连接本机 helper2，执行 helper2 下发的 task-scoped Studio 命令。
- `tools/bridge2`：LLM / 脚本调用的本地 CLI。只读 `.clock-p/session.json`，不猜 machine，不走旧 hub/helper1/mcp1。

## 身份边界

Windows client 侧 helper2 的 public exposure 身份只允许来自同一个本机系统身份目录。正常位置是：

```text
%APPDATA%\dev.clock-p.com\
  machine_name
  feishu-user_name
  feishu-token
```

`machine_name` 必须和 `feishu-token`、`feishu-user_name` 放在同一个目录。helper2 启动时自己读取这些文件；禁止用 `--public-machine-name`、`--public-user`、`--clockbridge-token-file`、`--clockbridge-bin`、`--clockbridge-x-token`、`--clockbridge-register-host` 或 `--clockbridge-register-ip` 覆盖或透传 clockbridge 细节。

`task-agent start` 是另一条边界：`--machine_name` 必须显式传入。task-agent 不读取本机 `machine_name` 文件，也不从历史 workspace 状态推断。task-agent 可以读取本机 `feishu-user_name` 和 `feishu-token` 来注册 Rojo 公网域名、访问 public helper2，但 machine 选择只能来自 `--machine_name`。启动成功后，task-agent 把 `machine_name`、`task_id`、`helper.base_url` 和 Rojo 路由写入 `.clock-p/session.json`。后续 bridge2、MCP、截图、日志、play/stop/mode 命令都只读 `.clock-p/session.json`。

## 本地启动

构建 helper2 和 task-agent：

```powershell
cd go-helper
go test ./...
go build -o bin\studio-helper.exe .\cmd\studio-helper
go build -o bin\task-agent.exe .\cmd\task-agent
```

构建并安装 mcp2 插件：

```powershell
rojo build ..\plugin-mcp2\default.project.json -o $env:LOCALAPPDATA\Roblox\Plugins\MCP2Plugin.rbxm
```

启动 helper2：

```powershell
.\bin\studio-helper.exe --register-domain=false
```

`--register-domain` 默认是 `true`。纯本地开发、不需要把 helper2 域名打到公网时，显式传 `--register-domain=false`。启用时 helper2 内嵌 clockbridge remote forward，不再启动外部 `clockbridge-cli`。

启动 task-agent：

```powershell
.\bin\task-agent.exe start `
  --workspace K:\roblox_space\test_game3 `
  --environment local `
  --machine_name sunjun2 `
  --place_id 113577273791190 `
  --helper-base-url http://127.0.0.1:44750 `
  --register-domain=false `
  --rojo-bin K:\roblox_space\rojo\target\release\rojo.exe `
  --project K:\roblox_space\test_game3\default.project.json
```

`task-agent` 的 Rojo server 总是在本地启动。`--register-domain` 默认是 `true`；启用时 task-agent 会内嵌 clockbridge，把本地 Rojo 注册成公网域名，并把 `.clock-p/session.json` 与 heartbeat 里的 `rojo.upstream_url` 写成公网 URL。纯本地开发时显式传 `--register-domain=false`。

## 公网路由

公网链路仍然是同一套 helper2 / task-agent / mcp2，不恢复旧 hub、helper1 或 mcp1：

```text
task-agent -> public helper2 URL -> helper2 -> mcp2 -> Studio
helper2 Rojo proxy -> local task-agent Rojo server
```

helper2 公网域名由本机身份文件推导：

```text
https://roblox-helper-{machine_name}-{feishu-user_name}-user.dev.clock-p.com
```

task-agent 公网 Rojo 域名由显式 machine 对应的 task 和本机 user 推导：

```text
https://{place_id}-{task_id}-rojo-{feishu-user_name}-user.dev.clock-p.com
```

当前线上只保证 `roblox-helper-*` helper2 公网域名可作为外部入口。Rojo 插件连接时仍先访问本机 helper2，再由 helper2 转发到 task-agent 启动的本机 Rojo server。

`.clock-p/session.json` 中：

- `helper.base_url`：公网 helper2 URL，public 模式下外部脚本和 LLM 只走这个入口。
- `rojo.upstream_url`：helper2 实际使用的 Rojo upstream，当前必须是本机 `http://127.0.0.1:<port>`。
- `rojo.public_url`：task-agent 诊断字段，表示尝试注册的独立 Rojo 公网 URL；当前线上 Nginx/gateway 未保证该子域可作为 client 路由，不能交给 helper2 或 Rojo 插件作为权威 upstream。

公网测试时，即使 server/client 在同一台机器上模拟，也必须让外部控制面走 `helper.base_url` 的公网 URL；Rojo 同步是否成功要看 Studio 日志中 Rojo 插件是否真正连上并完成初始同步，不能只看 HTTP 200。


停止 task-agent：

```powershell
.\bin\task-agent.exe stop --workspace K:\roblox_space\test_game3
```

## bridge2 CLI

入口：

```text
tools/bridge2/clockp-roblox-cli.cmd
tools/bridge2/clockp-roblox-cli.sh
tools/bridge2/cli.py
```

所有 bridge2 命令只输出一个 JSON 对象。成功和失败都走 JSON；stderr 默认不输出。参数错误、help、异常也会转成 JSON。

Phase 19A 命令：

```text
status
mode
ensure-edit
play
stop
screenshot
run-code-direct
run-code
play-mode-logs
```

`run-code-direct` 不做 `ensure-edit`，当前 Studio 不是 edit 时直接失败，也不会 stop。

`run-code` 会先执行 `ensure-edit`：如果当前是 `play_server`，先 stop，再轮询到新的 `edit` mode，然后执行代码。

`run-code-direct` 和 `run-code` 都支持：

```text
--code <luau>
--file <path>
```

二者必须且只能提供一个。

示例：

```powershell
py -3 tools\bridge2\cli.py --workspace K:\roblox_space\test_game3 mode
py -3 tools\bridge2\cli.py --workspace K:\roblox_space\test_game3 run-code --code "print('hello'); return 'ok'"
```

## helper2 HTTP / MCP

当前 task-scoped HTTP API：

```text
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

`/session/{task_id}/runtime-log` 是 helper2 内部 task-scoped 日志读取 API。bridge2 对外命令名是 `play-mode-logs`；不要把新 CLI 命令叫做 `runtime-log`，也不要恢复旧独立 runtime-log server。

helper2 MCP 当前只暴露新版 task-scoped 工具，不保留旧工具别名：

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

旧 `run_code`、`insert_model`、`launch_studio_session`、`start_stop_play`、`take_screenshot` 等旧 MCP 名称不再作为新版 MCP 工具暴露。

## run-code 安全边界

`studio_run_code` 是本地可信联调能力，不是强安全沙箱。它会拦截明显的 Studio 生命周期控制 API token：

```text
StudioTestService
ExecutePlayModeAsync
ExecuteRunModeAsync
EndTest
```

这用于防止 LLM 通过任意 Luau 绕过 `play/stop/ensure-edit` 编排；不要把它理解成对恶意 Luau 的完整隔离。

## official CLI

official generation bridge 属于 Phase 19B 本地范围。helper2 每次请求启动一个新的官方 `StudioMCP.exe`，绑定当前 task-owned Studio，执行一个白名单工具，然后结束进程。

当前 bridge2 命令：

```text
official-ping
official-store-image
official-generate-mesh
official-generate-procedural-model
official-wait-job
official-search-creator-store
official-insert-from-creator-store
```

helper2 official 原语都是 direct：只要求 live task 和 task-owned Studio，不做 `ensure-edit`、不 stop、不重试。是否走公网取决于调用方读取 `.clock-p/session.json` 后使用的 `helper.base_url`。

bridge2 的 per-command 规则：

- `official-ping`、`official-store-image`、`official-wait-job`、`official-search-creator-store` 不调用 `ensure-edit`。
- `official-generate-mesh`、`official-generate-procedural-model`、`official-insert-from-creator-store` 默认先 `ensure-edit`，可用 `--no-ensure-edit` 跳过。
- `official-wait-job` 默认按短轮询执行，`timeout` 默认为 1 秒。官方返回 `{"status":"Timeout","lastKnownStatus":"Polling"}` 时，bridge2 仍返回 `ok:true`，上层 skill / 脚本继续轮询；`Completed` / `Failed` / `Cancelled` 才是终态。

当前官方 `list_roblox_studios` 只返回 `id/name/active`，不返回 PID。为避免猜测窗口，Phase 19B 本地 official 命令只在官方 CLI 看到一个 Studio、helper2 当前 task 也只有一个 task-owned Studio、且官方 `execute_luau(Edit)` 读到的 `game.PlaceId` 等于 task place_id 时执行；多 Studio 场景返回结构化失败，不作为本轮验收项。

## 当前验收

提交前至少跑：

```powershell
py -3 -m unittest .\tools\bridge2\test_cli.py
cd go-helper
go test ./cmd/studio-helper ./cmd/task-agent ./internal/taskagent ./internal/tasksession
rojo build ..\plugin-mcp2\default.project.json -o $env:TEMP\MCP2Plugin-test.rbxm
```

涉及 Studio 行为时，必须用真实 Studio 本地跑通。只靠单测或 fake HTTP helper 不算完成。

Phase 19A 已验证过的本地真实 Studio 路径包括：

- bridge2 `status` / `mode` / `screenshot` / `play-mode-logs`
- edit 模式 `run-code-direct`
- play 模式 `run-code-direct` 失败且不 stop
- play 模式 `run-code` 先 stop、等待 edit、再执行
- forbidden token 返回 JSON 失败
- helper2 MCP `tools/list` 只暴露新版 task-scoped 工具
- helper2 MCP `helper2_studio_run_code` 在真实 Studio edit 模式成功返回 `print` / `warn` / `return`
- helper2 MCP `helper2_studio_run_code` 的编译失败和 forbidden token 拦截都返回结构化 JSON，不转成旧工具或旧路由异常

本轮不验收 Studio 多开场景；不要把多开行为作为 Phase 19A 完成条件。

Phase 19A / 19B 的本地验收已经完成；后续公网验收必须确认 helper2 和 Rojo 都通过内嵌 clockbridge 注册域名，且 bridge2 全程只按 `.clock-p/session.json` 里的公网 URL 路由。

## 旧代码说明

仓内旧 Rust MCP、旧 helper、旧 hub/task-server 相关代码目前只作为历史实现保留。不要删除它们，除非有单独清理任务；也不要在新文档里把它们写成当前操作入口。
