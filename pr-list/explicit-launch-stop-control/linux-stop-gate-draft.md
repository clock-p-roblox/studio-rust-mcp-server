# Linux stop gate 修复 draft

## 判断标准

本轮只修 Linux 侧已经被实测打出来的错误门控，不重新定义 Windows helper/plugin 语义，也不把高层启动链路改成另一个主线。

当前主线按下面理解：

- `launch-game.py` 是高层启动入口。当前已经 running 时，它可以在确认有 fresh runtime control heartbeat 后走 `stop -> 等回 stable edit/stop -> launch`；如果 running 但不可控，应失败为类似 `uncontrolled_play_session`，不能猜测或隐藏 retry。
- `launch_studio_session` 的现行 skill 语义仍允许在 running 下由高层动作处理 stop/relaunch。本轮不把它改成“running 必拒绝”。如果后续要拆成更严格的 low-level primitive，那是单独的主线变更，必须同步 skill、代码和调用方。
- `start_stop_play(stop)` / `stop-game.py` 是恢复入口。server 有权停止自己当前 active helper 能触达的 Studio play/run 状态，即使这次 play 不是当前 launch 启动的。
- `task_id` 是路由和一致性检查，不是 stop ownership 检查。可以检查 active helper / helper live status 是否与请求的 `task_id` 一致，但不能用“不是自己启动的”拒绝 stop。
- hub 负责 task、claim、路由、helper 连通性摘要和 debug/status 聚合；hub 不负责裁决 Studio 当前 play/stop 状态。涉及 Studio live 状态，应读 MCP server 当前连接的 helper live status。

## 本轮必须修的问题

### 1. Server `StopControl` 仍复用了 launch/edit 的 hub readiness gate

原问题是 `queue_tool_request` 对所有 local gate kind 都先调用 `require_hub_route_ready_snapshot`，包括 `StopControl`。

这会导致 `start_stop_play(stop)` 在 hub 认为 task services degraded、`accepting_launches=false`、Rojo/runtime-log/public route 不健康时直接失败。实测失败原因就是 `hub_task_services_not_ready`。这违反 stop 作为恢复入口的主线。

目标行为：

- `Launch`、`Edit`、`Official` 继续走现有 hub route/task-services readiness gate。
- `StopControl` 不再走 `require_hub_route_ready_snapshot`，也不硬依赖 hub 的 `helper_last_seen` / `remote_state` readiness。
- `StopControl` 只使用当前 MCP server 本地 active helper live facts：
  - 请求 `task_id` 与 active helper 的 `task_id` 一致；
  - helper WebSocket 最近有消息，未 stale；
  - helper live `task_status.task_id` 与请求 `task_id` 一致；
  - helper live Studio status 足够新，可以判断当前 mode/control state。

期望结果：

- `studio_mode=stop`：server 侧 stop preflight 通过，不被 launch/edit readiness 阻断；`stop-game.py` 高层可在 status-before 已经是 fresh stop 时直接 no-op 成功，表示“已观察到停止”，不暗示 edit/official/launch 已全部 ready。
- `studio_mode=start_play|run_server` 且 `studio_control_state=ready`：允许下发 stop。
- stopping phase：明确返回 `stopping_requested`。
- unknown/stale/lost/uncontrolled/none_connected：明确失败，不猜测、不继续操作。

### 2. `stop-game.py` 的 stop no-op 错误依赖 `launch_ready`

`fresh_stop_mode_from_status` 当前要求 `launch_ready=true`，这对 stop no-op 是错误的。

目标行为：

- 如果 `/status` 来自 `local_helper_live`，`studio_mode=stop`，并且 `studio_mode_age_ms` 足够新，`stop-game.py` 可以 no-op 成功。
- no-op 成功只代表已经观察到 Studio stop，不代表 `launch_ready`、`edit_ready`、official adapter、Rojo、runtime-log 或 public route 已经恢复。

### 3. diagnostic `run_code` timeout 文案混用了写操作风险

`launch-game.py` 会用 `run_code` + `diagnostic=true` 做启动前后读回。Python `McpClient` 当前把所有 `run_code` 都按写操作风险描述，超时时会暗示可能产生 Studio side effects。

目标行为：

- 不把所有 `run_code` 从非自动重试集合里移出。
- 按 `tool_name + arguments.diagnostic` 区分文案：
  - `diagnostic=true`：说明诊断读请求结果未知、请求可能已经到达 Studio；不写“可能产生 Studio side effects”。
  - 非 diagnostic `run_code`：继续按非幂等/可能产生 Studio side effects 处理，不自动 retry。

## 非目标

- 不修改 Windows helper/plugin stop actuator 消费逻辑。
- 不用加大 timeout 或自动 retry 来掩盖 public HTTPS/clockbridge 偶发 connect hang。
- 不在本轮重定义 `launch_studio_session` 为 running 必拒绝。
- 不移除 `task_id`，也不把 `task_id` 改成 stop ownership。

## 需要同步的文档/skill

- `roblox-launch-game` skill 中 “running 则自动 stop 后再 launch” 需要补一句：前提是有 fresh runtime control heartbeat；否则失败为 uncontrolled，不隐式 retry。
- `roblox-mcp` skill 当前 “`launch_studio_session` 在 running 下允许由高层动作处理 stop/relaunch” 与本轮修复不冲突，不应改成 running 必拒绝。

## 验证计划

- Rust focused test：`StopControl` 在 hub task services degraded 时不被 `require_hub_route_ready_snapshot` 拦住，但仍要求 active helper/task live status 与 `task_id` 一致且 fresh。
- Python focused test：`stop-game.py` 在 `launch_ready=false` 但 local helper live `studio_mode=stop` 且 fresh 时 no-op 成功。
- Python focused test：diagnostic `run_code` timeout 使用诊断读失败文案；非 diagnostic `run_code` 仍使用非幂等写风险文案。
- 静态检查：
  - `cargo fmt -- --check`
  - focused Rust tests for `rbx_studio_server`
  - `python3 -m py_compile tools/bridge/*.py tools/bridge/details/*.py`
  - focused bridge tests
- 使用已有 helper 做 live test：
  - play 状态下 stop；
  - stop 状态下 stop no-op；
  - 顺序 `launch -> stop` 多轮；
  - 并发 `stop + stop`；
  - 并发 `launch + stop`，只要求最终稳定且无 pending/queued 泄漏。
