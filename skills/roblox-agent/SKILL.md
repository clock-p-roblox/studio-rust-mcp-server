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
  -> 向 helper2 心跳
```

不要使用其他 Roblox 联调入口。不要新写 wrapper。

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
<mcp_repo>\go-helper\bin\studio-helper.exe
```

不要使用公网矩阵测试临时目录里的 `studio-helper.exe`。启动前先重新 build，并确保 `go` 在 PATH 中。

```powershell
cd <mcp_repo>\go-helper
New-Item -ItemType Directory -Force bin | Out-Null
go build -o bin\studio-helper.exe ./cmd/studio-helper
go build -o bin\task-agent.exe ./cmd/task-agent
```

## 构建并安装 plugin-mcp2

Studio 侧插件也必须使用本仓当前源码构建。更新 `plugin-mcp2` 后，先停止当前 Studio / task-agent，再安装插件：

```powershell
cd <mcp_repo>\plugin-mcp2
rojo build default.project.json --plugin MCP2Plugin.rbxm
```

如果 `rojo` 不在 PATH 中，使用本机实际的 `rojo.exe` 路径执行同一条 build 命令。

## 本地启动

启动 helper2：

```powershell
<mcp_repo>\go-helper\bin\studio-helper.exe --addr 127.0.0.1:44750
```

启动 task-agent：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --place_id <place_id> `
  --machine_name <machine_name> `
  --helper-base-url http://127.0.0.1:<helper2_port> `
  --code-sync-config code-sync.roots.json `
  --code-sync-project default.project.json
```

测试者必须显式提供 `--workspace` 和 `--place_id`。不要在测试入口里内置固定测试 place。


查看和停止：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe status --workspace <workspace>
<mcp_repo>\go-helper\bin\task-agent.exe stop --workspace <workspace>
```

## 公网启动


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

public `task-agent` 访问公网 helper 时会读取 workspace 或本机身份目录里的 `feishu-token`，并注入 Bearer 鉴权。

helper2 自身的 HTTP handler 不做 Bearer 鉴权；但当 `bridge2` 通过公网 `helper.base_url` 访问时，外层 `dev.clock-p.com` 入口仍要求 `Authorization: Bearer <feishu-token>`。`bridge2` 会自动从 workspace 或身份目录读取 `feishu-token` 注入。


## bridge2 命令

`--workspace` 是顶层参数，必须写在子命令前。所有命令只输出 JSON。

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> status
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> mode
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> ensure-edit
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> launch
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> stop
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> screenshot
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play-mode-logs
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> run-code-direct --file code.lua
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> run-code --file code.lua
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-manifest
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-live-manifest
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-dry-run
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-apply
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-apply --no-ensure-edit
```

`launch` 读取 workspace 下的 `prelaunch.json`，按顺序执行 prelaunch steps，全部成功后再调用 `play`。`play` 是 direct Studio start 原语，不执行 build、flush 或其他 prelaunch workflow。

`run-code-direct` 不做模式切换。`run-code` 会先 ensure edit。需要 edit 的子命令自己决定是否 ensure；CLI 顶层不做全局 ensure。

`ensure-edit` 只处理两种稳定态：当前已是 `edit` 则直接成功；当前是 `play_server` 则调用 `stop` 等待回到 `edit`。其他模式一律失败，不做猜测。

`code-sync-manifest` 是本地扫描，不需要 `.clock-p/session.json`。`code-sync-live-manifest`、`code-sync-dry-run`、`code-sync-apply` 走 task-scoped helper2 / mcp2 链路，并用 BLAKE3 hash 验证 Studio live tree。`code-sync-apply` CLI 默认会先 ensure edit；只有显式 `--no-ensure-edit` 时才保留“当前不是稳定 edit 就直接失败”的 direct 语义。

`prelaunch.json` 第一版支持：

- `ensure_edit`
- `shell`
- `code_sync_apply`

其中 `code_sync_apply` 默认 `ensure_edit = true`；如需保留 direct flush 语义，必须显式写 `ensure_edit: false`。

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
cd <mcp_repo>
py -3 -m py_compile tools\bridge2\cli.py
cd plugin-mcp2
rojo build default.project.json -o ..\output\MCP2Plugin.rbxm
cd go-helper
go test -count=1 ./...
```

涉及公网路由时跑：

```powershell
cd <mcp_repo>
py -3 util\helper2_task_session_gate_test.py --place-id <place_id>
```
