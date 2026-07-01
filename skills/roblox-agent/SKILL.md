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

task-agent 的规则相反：`--machine_name` 必须显式传入，不读取本机 `machine_name` 文件。这里的 `--machine_name` 是目标 Windows helper 的 `machine_name`，也就是 `aim_helper_machine_name` 的语义，不是 task-agent 当前这台机器的名字。helper2 自己的本地 `machine_name` 仍然来自 Windows 身份文件。task-agent 启动前会从 workspace 根的 `clock-p.workspace.json` 读取 `place_id` 与 code-sync 绑定；启动后，后续命令只读 `.clock-p/session.json`。

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

Studio 侧插件必须使用本仓当前源码构建。更新 `plugin-mcp2` 后，先停止当前 Studio / task-agent，再按 Windows 侧当前插件打包流程生成并安装 `MCP2Plugin.rbxm`。

server 侧不再把插件打包工具作为 code-sync / flush 运行时依赖；本机只能做源码、Python、Go 与静态检查，最终插件包由 Windows 侧部署验证。

## 本地启动

启动 helper2：

```powershell
<mcp_repo>\go-helper\bin\studio-helper.exe --addr 127.0.0.1:44750
```

启动 task-agent：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --machine_name <machine_name>
```

第一次接入一个 workspace，最少先确认这 2 个文件：

```text
clock-p.workspace.json
code-sync.tree.json
```

workspace 根的 `clock-p.workspace.json` 至少要有：

```json
{
  "place_id": "93795519121520",
  "code_sync_config": "code-sync.tree.json"
}
```

说明：

- `place_id` 必填。
- `code_sync_config` 可省略；省略后默认去 workspace 根找 `code-sync.tree.json`。
- 所以如果你只写了 `place_id`，但默认 `code-sync.tree.json` 不存在，`task-agent` / `bridge2 code-sync-*` 仍会失败。
- `code-sync.tree.json` 负责按 Studio DataModel 树声明 code-sync 托管节点。

当前默认 `--environment public`。若要走本地 helper2，显式传：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --machine_name <machine_name> `
  --environment local `
  --helper-url http://127.0.0.1:<helper2_port>
```

`local` 故意保持手工模式：你自己决定 helper2 的监听地址，再显式把 `--helper-url` 传给 task-agent；当前主线不额外封装这条路径。


查看和停止：

```powershell
<mcp_repo>\go-helper\bin\task-agent.exe status --workspace <workspace>
<mcp_repo>\go-helper\bin\task-agent.exe stop --workspace <workspace>
```

## 公网启动


```powershell
<mcp_repo>\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --machine_name <machine_name>
```

public 是默认环境。`task-agent` 会从 workspace 或本机身份目录读取 `feishu-user_name` 来推导公网 helper URL，并读取 `feishu-token` 注入 Bearer 鉴权。`--user` 只用于特殊覆盖，不是正常主线参数。

helper2 自身的 HTTP handler 不做 Bearer 鉴权；但当 `bridge2` 通过公网 `helper_url` 访问时，外层 `dev.clock-p.com` 入口仍要求 `Authorization: Bearer <feishu-token>`。`bridge2` 会自动从 workspace 或身份目录读取 `feishu-token` 注入。


## bridge2 命令

`--workspace` 是顶层参数，必须写在子命令前。所有命令只输出 JSON。

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> status
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> mode
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> ensure-edit
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> launch
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> launch --data-file debug\play-data\startup-profile.json --wait-seconds 10
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play --data-file debug\play-data\startup-profile.json
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> stop
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> screenshot
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> run-code-direct --file code.lua
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> run-code --file code.lua
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-manifest
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-live-manifest
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-dry-run
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> code-sync-apply
```

这里有三个层级：

- bridge2 `launch` 是工程级启动入口：先跑 workspace 的 `prelaunch.json`，全部成功后再调用 `play`。
- bridge2 `play` 是直接 PlayMode 入口：只负责下发 play args、等待 `play_server` 并校验 `launch_id`，不执行 build 或 code-sync。
- helper2 / MCP 的 `studio_play` 是被 bridge2 `play` 调用的底层原语。

`run-code-direct` 不做模式切换。`run-code` 会先 ensure edit。需要 edit 的子命令自己决定是否 ensure；CLI 顶层不做全局 ensure。

`play` 的成功判据不读日志，也不靠截图：

- bridge2 生成本次随机 `launch_id`，随 play args 下发。
- helper2 response 必须 echo 同一个 `requested_launch_id`。
- plugin-mcp2 command result 必须 echo 同一个 `launch_id`。
- 最终 Studio mode 必须变为 `play_server`。
- 最终 `mode_seq` 必须不同于 play 前的 edit `mode_seq`。
- 最终 mode payload 的 `launch_id` 必须等于本次请求的 `launch_id`。

`launch` 默认读取 workspace 根目录的 `prelaunch.json`。可用 `--prelaunch <path>` 指定其他 workspace 内文件；prelaunch path、shell cwd 和 code-sync config 都必须留在 workspace 内。

示例：

```json
{
  "steps": [
    {
      "kind": "shell",
      "name": "build",
      "argv": ["npm", "run", "build:roblox"],
      "cwd": "."
    },
    {
      "kind": "ensure_edit",
      "name": "ensure-edit"
    },
    {
      "kind": "code_sync_apply",
      "name": "code-sync"
    }
  ]
}
```

支持的 step：

- `shell`：在 workspace 内指定 `cwd` 执行 `argv`，非 0 exit code 会阻止 play。
- `ensure_edit`：若当前在 play，则先 stop 回 edit。
- `code_sync_apply`：执行当前 bridge2 `code-sync-apply`。默认读取 `clock-p.workspace.json.code_sync_config`，也可显式写 `"config": "code-sync.tree.json"`；默认 `"ensure_edit": true`。

`launch` 和 `play` 都支持 `--data-file` / `--data-json`，二者都会把 data 传给同一套 PlayMode args。`launch --wait-seconds N` 会在 play 成功后等待 N 秒并返回等待后的 mode，用于 smoke 或压测启动观察。

调试参数、测试场景参数和大段输入统一走 PlayMode `data`。默认推荐 `--data-file`：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play --data-file debug\play-data\startup-profile.json
```

`--data-file` 用于当前主线测试、可复用 debug preset、压测矩阵和需要 review / 复跑的输入。`--data-json` 只用于很短的一次性 smoke：

```powershell
tools\bridge2\clockp-roblox-cli.cmd --workspace <workspace> play --data-json "{\"probe\":\"smoke\"}"
```

运行时代码从 `StudioTestService:GetTestArgs().data` 读取这些参数；不要再用 workspace 静态 `runtime_launch_params.lua` 承载测试输入。`data` 顶层必须是 JSON object。

`code-sync-manifest` 是本地扫描，不需要 `.clock-p/session.json`。`code-sync-live-manifest`、`code-sync-dry-run`、`code-sync-apply` 走 task-scoped helper2 / mcp2 链路，只允许稳定 edit 态，并用 BLAKE3 hash 验证 Studio live tree。

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
- `helper2_official_ping`
- `helper2_official_store_image`
- `helper2_official_generate_mesh`
- `helper2_official_generate_procedural_model`
- `helper2_official_wait_job`
- `helper2_official_search_creator_store`
- `helper2_official_insert_from_creator_store`

MCP 工具和 bridge2 共享同一个 task-scoped helper2 控制通道，但语义层级不同：`helper2_studio_play` 只是标准 launch 的底层受理步骤，返回后仍需要继续查询 mode；正常启动游戏应走 bridge2 `play`，由它完成 `launch_id`、`mode_seq` 和 `play_server` 的完整验证。

## 验证

文档或脚本改动后至少跑：

```powershell
cd <mcp_repo>
py -3 -m py_compile tools\bridge2\cli.py
cd go-helper
go test -count=1 ./...
```

涉及公网路由时跑：

```powershell
cd <mcp_repo>
py -3 util\helper2_task_session_gate_test.py --place-id <place_id>
```
