# 当前 Roblox agent 主线设计

本文记录当前主线，不保留历史链路说明。实现、测试和文档都以这里和仓库根 README 为准。

## 目标

把 LLM 调 Roblox Studio 的路径收敛成一条稳定链路：

```text
bridge2 CLI / helper2 MCP
  -> .clock-p/session.json
  -> helper2 task API
  -> plugin-mcp2
  -> Roblox Studio
```

`task-agent` 负责创建会话、启动 Rojo、写入 session 文件和向 helper2 心跳。helper2 负责所有 Studio 侧原语。bridge2 只做 CLI 编排和 JSON 输出。

## 身份与配置

helper2 从 `%APPDATA%\dev.clock-p.com\` 读取：

- `machine_name`
- `feishu-user_name`
- `feishu-token`

`task-agent` 启动时必须显式传入 `--machine_name`。启动成功后，`.clock-p/session.json` 成为后续命令的唯一会话来源。

helper2 内嵌 clockbridge 库。helper2 启动时 `--register-domain` 默认开启，并自动用本机身份文件注册公网 helper 域名。

## session 文件约束

`.clock-p/session.json` 必须包含：

- `task_id`
- `machine_name`
- `place_id`
- `task_agent_pid`
- `task_agent_started_at_ms`
- `task_agent_status_url`
- `helper.base_url`
- `rojo.local_url`
- `rojo.upstream_url`
- `rojo.public_url`，启用 Rojo 公网诊断域名时存在

bridge2 和 helper2 MCP 都按 session 文件路由。除 `task-agent start` 外，其他命令不再手动传 machine、user、token 或 task 身份。

## bridge2 命令

所有命令只输出 JSON，成功失败都用同一结构表达。

基础命令：

- `status`
- `mode`
- `ensure-edit`
- `play`
- `stop`
- `screenshot`
- `play-mode-logs`

Lua 命令：

- `run-code-direct`
- `run-code`

`run-code-direct` 不做模式切换。`run-code` 先执行 ensure-edit，再调用 direct 命令。是否 ensure-edit 由子命令决定，CLI 顶层不设置全局默认。

官方 Studio MCP adapter 命令：

- `official-ping`
- `official-store-image`
- `official-generate-mesh`
- `official-generate-procedural-model`
- `official-wait-job`
- `official-search-creator-store`
- `official-insert-from-creator-store`

`official-wait-job` 的参数名以实际 Roblox 官方 CLI / adapter 接口为准；文档和实现必须保持一致。
bridge2 当前参数以 `tools/bridge2/clockp_bridge2/commands.py` 的 argparse 定义为准。

## helper2 原语

helper2 对外稳定原语：

- task heartbeat / release / status
- mode
- play / stop
- screenshot
- run-code
- play-mode log read
- Studio log read
- Rojo forward
- official adapter forward

helper2 只提供稳定原语，复杂流程放在 bridge2 Python 层编排。

## ensure-edit

ensure-edit 既是 bridge2 的工具函数，也是显式 CLI 子命令；它不是 CLI 顶层默认行为。

需要 edit 模式的子命令流程：

```text
读取 session.json
查询 helper2 mode
如果已是 edit，继续执行 direct 原语
如果是 play，调用 stop 并等待回到 edit
如果状态不可确认，返回 JSON 失败
```

`*-direct` 命令不得执行 ensure-edit。

## Rojo 与公网

task-agent 启动本地 Rojo，并将本地地址写入：

- `rojo.local_url`
- `rojo.upstream_url`

启用 Rojo 公网诊断域名时，task-agent 额外写入 `rojo.public_url`。该字段用于诊断域名注册，不替代 helper2 到 Rojo 的本地上游。

公网验收必须检查：

- bridge2 控制面使用公网 `helper.base_url`。
- helper2 可以看到 live task。
- play / stop / screenshot / log 通过公网 helper2 可用。
- Studio 日志显示 Rojo initial sync 成功。

## 验收

每次改动按风险选择验证。涉及 helper2 / task-agent / bridge2 / Rojo 路由时，至少执行：

```powershell
cd K:\roblox_space\studio-rust-mcp-server\go-helper
go test -count=1 ./...
New-Item -ItemType Directory -Force bin | Out-Null
go build -o bin\studio-helper.exe ./cmd/studio-helper
go build -o bin\task-agent.exe ./cmd/task-agent
```

涉及公网时执行：

```powershell
cd K:\roblox_space\studio-rust-mcp-server
py -3 util\helper2_public_route_matrix_test.py `
  --place-id 113577273791190 `
  --kill-existing `
  --public-ready-timeout 120 `
  --initial-mode-timeout 240
```
