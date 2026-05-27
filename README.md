# clock-p Studio MCP Server

This repository is used by clock-p as the internal `clockp MCP` server that runs inside each debug task.

Do not configure this binary directly as a Claude / Cursor / Codex MCP server for clock-p Roblox workflows. Codex and other LLM operators must use the clock-p Roblox skills and high-level scripts documented in `../clock-p-platform/docs/roblox-debug-cluster-constitution.md`.

The current clock-p shape is:

- `rbx-studio-mcp --http --plugin-port ... --http-port ... --no-auth` runs inside a task tab.
- Linux exposes task-scoped Rojo / clockp MCP / runtime-log routes through clockbridge.
- Windows `studio_helper` registers with hub, claims one task, launches Studio, and bridges plugin/helper traffic.
- `official MCP` is Roblox's Studio-internal MCP. clockp MCP may route selected internal capabilities to official MCP, but platform scripts and Codex do not call official MCP directly.

### Included clockp MCP tools

- **run_code** - Runs a command in Roblox Studio and returns printed output.
- **insert_model** - Inserts a model from the Roblox marketplace.
- **get_console_output** - Gets Studio console output.
- **launch_studio_session** - Launches Studio into start_play or run_server through helper.
- **start_stop_play** - Starts or stops play mode.
- **run_script_in_play_mode** - Runs a one-time script in play mode.
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
