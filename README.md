# clock-p Studio MCP Server

## Current mainline status

The active roblox-agent refactor mainline is the hubless Phase 9+ path:

```text
task-agent -> helper2 -> mcp2
```

Current ownership:

- `go-helper/cmd/task-agent`: workspace-local task session, Rojo supervision, helper2 heartbeat, and shutdown/release.
- `go-helper/cmd/studio-helper`: helper2 local session authority, task lease tracking, task-owned Studio lifecycle, and mcp2-facing helper APIs.
- `mcp2`: the Studio plugin side of the helper2 command channel.

Deprecated legacy stack:

- old hub
- old task-server
- helper1
- mcp1 / old Rust clockp MCP server path
- old runtime-log server

Do not use the deprecated stack as the implementation target, compatibility fallback, or validation gate for the Phase 9+ mainline. In particular, do not run `cargo` checks/tests as Phase 9+ verification unless the task is explicitly about the legacy Rust binaries.

Phase 9+ verification should use the Go helper2/task-agent path:

```sh
cd go-helper
go test ./...
go build ./cmd/studio-helper ./cmd/task-agent
```

The sections below are retained as historical notes for the old stack and should not be read as the current architecture.

---

This repository is used by clock-p as the internal `clockp MCP` server that runs inside each debug task.

Do not configure this binary directly as a Claude / Cursor / Codex MCP server for clock-p Roblox workflows. Codex and other LLM operators must use the clock-p Roblox skills and high-level scripts in `../roblox-dev-infra`.

The current clock-p shape is:

- `rbx-studio-mcp --http --plugin-port ... --http-port ... --no-auth` runs inside a task tab.
- Linux exposes task-scoped Rojo / clockp MCP / runtime-log routes through clockbridge.
- Windows `studio_helper` registers with hub, claims one task, launches Studio, and bridges plugin/helper traffic.
- `official MCP` is Roblox's Studio-internal MCP. clockp MCP may route selected internal capabilities to official MCP, but platform scripts and Codex do not call official MCP directly.

### Included clockp MCP tools

- **run_code** - Runs a command in Roblox Studio and returns printed output.
- **insert_model** - Inserts a model from the Roblox marketplace.
- **get_console_output** - Gets Studio console output.
- **launch_studio_session** - The only tool that launches Studio into start_play or run_server through helper.
- **start_stop_play** - Stops the current play/run session.
- **get_studio_mode** - Gets the current Studio mode.
- **take_screenshot** - Captures the active Studio window through helper.

### Build from source

```sh
cargo build
```

### Backend mode (domain/proxy friendly)

For clock-p task usage, run this process without installer behavior and expose two local endpoints:

- Studio plugin bridge (long polling): `http://127.0.0.1:44755`
- MCP Streamable HTTP: `http://127.0.0.1:44756/mcp`

```sh
cargo run -- --http --plugin-port 44755 --http-port 44756 --no-auth
```

By default `--http` requires a bearer token. It checks in this order:

1. `--bearer-token`
2. `--bearer-token-file`
3. `~/.dev.clock-p.com/feishu-token`

Use `--no-auth` only for local debugging.

Useful flags:

- `--plugin-port <port>`: plugin bridge port (default `44755`)
- `--http-port <port>`: MCP HTTP port (default `44756`)
- `--write-plugin <path>`: export bundled `MCPStudioPlugin.rbxm` and exit
- `--workspace-path <path>`: explicit workspace identity

### Clock-p task cluster mode

`clock-p` 当前的联调链路已经升级为 task 化 debug cluster：

- server cluster 先向 hub 创建 task，拿到 `task_id`
- rojo / clockp MCP / runtime-log 都按同一个 `task_id` 暴露公网 host
- helper 是机器级单例，通过 hub claim task，再连到对应 task 的远端 MCP WebSocket
- restart / stop 后重新启动都会申请新的 `task_id`
- 当前不再使用 recover 语义

hub 本身只做 control plane：

- helper register / heartbeat / claim
- task create / heartbeat / release
- helper block / drain

为了覆盖极端故障场景，hub 现在会把 task 状态落盘；helper heartbeat 也会回填和校正 task claim。这样在 hub 重启、helper 重启、supervisor 过期等场景下，Linux 阶段就能先验证控制面的稳定性。

本仓新增 `roblox_hub` 二进制作为 hub server：

```sh
cargo run --bin roblox_hub -- --port 44758 --no-auth
```

常见返回字段：

- task：`task_id / task_token / routes`
- helper：`helper_id / capacity / active_launches`

### Clock-p helper mode

`clock-p` 的 Studio helper 现在是机器级单例：

- `Rojo plugin -> helper 本地 forward URL -> helper 使用当前 claimed task 的 Rojo route 转发`
- `clockp MCP plugin -> helper -> helper 通过 task-scoped clockp MCP 通道收发工具调用`
- `helper -> hub register / heartbeat / claim`
- `take_screenshot -> helper 截图 -> helper 通过 MCP WebSocket 分片上传 -> MCP server 落盘到 workspace artifacts`
- `official_mcp_store_image(image_base64, mime_type) -> helper 写入 Windows 本机 task-scoped 临时图片 -> helper 调用 hidden official MCP store_image(filePath)`
- official MCP 长任务的同步等待上限是 30 分钟；LLM / Codex 操作时超过 3 分钟仍未返回，应先汇报并检查状态后再决定是否继续等。

helper 二进制是本仓库里的 `studio_helper`：

```sh
cargo run --bin studio_helper -- --port 44750 --hub-base-url https://roblox-hub-<user>-public.dev.clock-p.com
```

默认行为：

- helper 自己读取 `feishu-user_name` 和 `feishu-token`
- 在 Windows 用户会话里直接运行 `studio_helper.exe` 时，会自动推导 `hub_base_url = https://roblox-hub-<user>-public.dev.clock-p.com`
- 所以正常情况下不再需要额外传 `--user-name`、`--bearer-token-file` 或 `--hub-base-url`
- helper 有稳定 `helper_id`；Windows helper 使用 `MachineGuid` 派生，其他平台使用持久化本地 id
- helper 注册 hub 后，会 claim task 并拿到：
  - `task_id`
  - `mcp_base_url`
  - `rojo_base_url`
- hub 会拒绝活跃重复 `helper_id`；helper 应在启动早期先 register hub，并把 `helper_id_conflict` 视为 fatal startup error
- hub 可以 block 某个 helper；block 后 helper 不能再 register / claim，新 claim 必须等 drain ack 或 timeout force release 后才交给其他 helper
- plugin 侧只需要配置 helper 端口
- helper 还额外提供：
  - `GET /status`
  - `GET /v1/rojo/config?placeId=...`
  - `POST /v1/mcp/register`
  - `GET /v1/mcp/plugin/request?instance_id=...`
  - `POST /v1/mcp/plugin/response`
  - `POST /v1/helper/screenshot`
  - `POST /v1/helper/runtime-screenshot`
  - `GET /v1/helper/studio-log`

clockp MCP HTTP 服务额外开放：

- `GET /ws/helper`

这个 WebSocket 由 helper 主动连接。Linux helper 提供 register / heartbeat / claim / 远端 WS 代理能力，但不提供 Win32 专属的截图、窗口定位和 Studio 本地日志读取能力。

### Helper block / drain

`block-helper` 是 hub control plane 操作，不是 Windows 进程 kill。它的目标是阻止 helper 继续拿新 task，并请求它释放当前 task，避免旧 helper 和新 helper 同时持有同一个 task。

目标状态机：

- `blocked/draining`：helper 已被 block，不允许 register / claim；已 claim task 保持在该 helper 名下，等待释放。
- `blocked/drained`：helper 后续 heartbeat 不再上报 pending task，hub 视为 ack 并清 claim。
- `blocked/force_released`：超过 drain deadline 后，hub 强制清 pending claim；这不表示 Windows helper 或 Studio 已停止。

协议约束：

- blocked helper heartbeat 仍返回 200 JSON。
- 旧 helper 的兼容释放依赖 `release_task_ids`；`ok:false` 只能作为新 helper 的拒绝/诊断信号。
- hub 计算 `reported_active_task_ids = active_task_ids ∪ active_tasks[].task_id`。
- `release_task_ids` 覆盖 helper 上报但 hub 不允许它继续持有的全部 task，包括 pending、claim 不匹配、不存在、expired 或 released 的 task。
- pending task 不再出现在 `reported_active_task_ids` 时，hub 才清 `claimed_by_helper_id`。
- drain timeout 使用独立 deadline，不使用 helper 最近 heartbeat；坏 helper 持续 heartbeat 也不能无限占住 task。
- force release 后，如果 blocked helper 迟到 heartbeat 仍上报 task，hub 继续返回 release 指令。
- 存在 pending drain 时，`unblock-helper` 不应静默丢弃 drain 状态。

`/v1/helpers` 的 blocked helper 输出应能解释排障链路：hub 是否发过 release、helper 最后上报了哪些 task、还有哪些 pending task、drain deadline 是否已到、deadline 还剩多久、哪些 task 已 force release。Linux 侧只能验证 hub API / 协议 / 单测，不能声称 Windows helper 或 Studio 已被实机停止。

当前收口方向：

- helper 是本机唯一权威配置源
- plugin 不应自行猜测或长期缓存 task 路由

### Ubuntu 交叉编译 Windows helper

如果你要在 Ubuntu 上产出 `studio_helper.exe`，先安装交叉编译依赖：

```sh
sudo ./util/install-ubuntu-windows-cross.sh
```

这个脚本会安装：

- `binutils-mingw-w64-x86-64`
- `gcc-mingw-w64-x86-64-posix`
- `g++-mingw-w64-x86-64-posix`
- Rust target `x86_64-pc-windows-gnu`

然后执行：

```sh
cargo build --release --bin studio_helper --target x86_64-pc-windows-gnu
```

产物路径：

```text
target/x86_64-pc-windows-gnu/release/studio_helper.exe
```

## Verify setup

For clock-p workflows, verify through the platform scripts:

```sh
python3 ../clock-p-platform/tools/bridge/debug-cluster-status.py --workspace <workspace>
python3 ../clock-p-platform/tools/bridge/roblox-mcp-request.py --workspace <workspace> status
```

Codex and other LLM operators should not send requests directly to this server as their MCP client. Use `roblox-start-services` and `roblox-launch-game` skills.
