# MCP server 使用本地 helper live status 做 Studio gate

## 判断标准

这件事要修的是事实源边界，不是低概率身份碰撞：

- hub 只负责 task/helper route、claim、blocked、service readiness、helper 连通性。
- Studio stop/play/control/edit/official adapter 状态只来自 MCP server 当前 helper WebSocket 上的 live `task_status`。
- 不做“hub 失败就信本地”，也不做“hub 与本地任一 ready 就放行”。
- 不把 task_id 随机碰撞或同 task_id 跨 helper 拼快照作为核心风险；这类概率太低，不应主导方案。

## 背景

当前 `rbx_studio_server.rs` 把两类事实混在 hub snapshot 里：

- hub route 事实：task 是否 active、service 是否 ready、helper 是否 claim、helper 是否在线 / blocked。
- Studio live 事实：`studio_mode`、`studio_control_state`、`studio_transition_phase`、`edit_runtime_state`、`official_mcp_adapter_state`。

server 自己已经通过 helper WebSocket 接收并保存了 `active_helper.task_status`，但 `run_code` / `insert_model` / `launch_studio_session` / `start_stop_play` / official MCP 的 gate 仍绕回 hub `/status` 读取 Studio live 字段。这样 hub 被迫成为 stop/play 的二手事实源，并引入传播竞态。

## 非目标

- 不改变 hub 对 task claim / blocked / release / service readiness 的权威边界。
- 不让本地 helper live status 绕过 hub route gate。
- 不把 hub 中的 stop/play 字段继续定义为权威。
- 不在本轮删除 hub wire 字段，避免立刻破坏旧脚本展示。
- 不把 helper_id 强绑定作为本轮根因修复的前置条件；如果实现成本低，可以补 helper_id 到 server debug/source 中做 provenance，但不能让这个低概率问题喧宾夺主。

## 当前真正问题

### 1. server gate 读错事实源

`fetch_hub_snapshot_for_gate` 返回的 `HubTaskRuntimeSnapshot` 同时包含 route 字段和 Studio 字段。`require_stop_mode_snapshot`、`require_studio_control_snapshot`、`require_official_adapter_ready_snapshot` 都基于这个 hub snapshot 判断 Studio 状态。

正确边界应是：

- hub snapshot 只回答“这个 task/helper route 是否可用”。
- local helper live status 才回答“Studio 当前是否 stop/play/stopping，official adapter 是否 ready”。

### 2. status/readiness 消费者仍会误信 hub Studio 字段

`roblox-dev-infra/tools/bridge/details/cluster_health.py` 目前从 hub `helpers[].active_tasks[]` 读取 `studio_mode`、`studio_control_state`、`official_mcp_adapter_state`，并据此计算 `edit_ready` / `official_ready`。

如果只改 Rust server gate，不改 dev-infra/status contract，脚本层仍会把 hub debug mirror 当权威，问题没有真正收口。

### 3. local live status 初始化需要明确策略

helper hello / heartbeat 里的 `task_status` 是 optional。不能因为首包 live status 还没到，就让首次 `launch_studio_session` 高频误拒；也不能在缺失状态时直接放行。

正确策略是 bounded wait：控制 gate 需要 live status 时，短暂等待首个 `task_status` 到达；等不到再 fail closed。

## 目标设计

### 1. 拆分 snapshot 类型

把 `HubTaskRuntimeSnapshot` 改成 `HubTaskRouteSnapshot`，只保留 hub 权威字段：

- `claimed_by_helper_id`
- `helper_id`
- `helper_blocked`
- `helper_last_seen_age_ms`
- `task_released`
- `task_service_state`
- `task_accepting_launches`
- `task_services_healthy`
- `remote_state`

从这个类型中删除：

- `studio_mode`
- `studio_mode_age_ms`
- `studio_mode_source`
- `studio_control_state`
- `studio_transition_phase`
- `edit_runtime_state`
- `official_mcp_adapter_state`
- 所有 Studio/edit/official adapter live 字段

新增 `LocalStudioLiveSnapshot`，从 `active_helper.task_status` 构造：

- `task_id`
- `helper_connection_id`
- `helper_last_message_age_ms`
- `studio_mode`
- `studio_mode_age_ms`
- `studio_mode_source`
- `studio_control_state`
- `studio_transition_phase`
- `studio_transition_age_ms`
- `edit_runtime_state`
- `edit_runtime_age_ms`
- `studio_control_last_error`
- `official_mcp_adapter_state`
- `official_mcp_adapter_age_ms`
- `official_mcp_adapter_last_error`
- `studio_status_source = local_helper_live`

如果顺手补 `helper_id` 到 `ActiveHelperConnection`，可以把它加入 `LocalStudioLiveSnapshot` 用于 debug/source 标注；但本轮 gate 不能围绕“task_id 碰撞”设计。

### 2. route gate 仍走 hub

新增或重命名：

- `fetch_task_route_snapshot`
- `require_hub_route_ready_snapshot`

route gate 只检查：

- hub configured。
- task 存在，未 released，未 expired。
- `service_state == ready`。
- `accepting_launches == true`。
- required services healthy。
- task 已 claim。
- claimed helper 在线、未 blocked、last seen 新鲜。
- helper active task remote_state 为 `connected | ready`。

注意：当前 `fetch_hub_snapshot_for_gate` 没有显式挡 `service_state != ready`、`accepting_launches=false`、required services unhealthy。新 route gate 必须补上。

### 3. Studio gate 只走本地 live status

新增 locked helper：

```text
local_studio_live_snapshot_locked(state, requested_task_id, action)
require_local_stop_ready_locked(...)
require_local_control_ready_locked(...)
require_local_official_ready_locked(...)
```

这些函数必须在最终入队 / official dispatch 的同一把 `state.lock()` 内使用，避免读完 local status 后连接生命周期变化再入队。

本地 live gate 检查：

- 存在 active helper。
- `active_helper.task_id == requested_task_id`。
- helper WebSocket `last_message_at` 未超过 `HELPER_HEARTBEAT_TIMEOUT`。
- 存在 `active_helper.task_status`。
- `task_status.task_id == requested_task_id`。
- 当前 gate 所需 age 字段存在且不超过 `HELPER_TASK_STATUS_STALE_AFTER`。

如果缺少 `task_status`，或已有 `task_status` 但当前 gate 所需字段还不可判定，比如缺少 `studio_mode` / `studio_mode_age_ms`，先做 bounded wait，等待首个可判定 live status；超时后 fail closed。bounded wait 期间不能持有 `state` lock。最终放行前仍要在同一把 lock 内重新读取并校验 local status。

### 4. 各类工具 gate 规则

#### `RunCode` / `InsertModel`

非 diagnostic `run_code` 和 `insert_model` 仍要求 Studio 在 stop/edit：

1. 先过 hub route gate。
2. 再在最终入队锁内执行 local stop gate。
3. 要求 `studio_mode == stop` 且 `studio_mode_age_ms` 新鲜。

diagnostic `run_code` 可以不要求 stop gate，但仍不能绕过 active helper task_id fencing。

#### `launch_studio_session` / `start_stop_play`

控制类工具：

1. 先过 hub route gate。
2. 如本地尚无可判定 control status，bounded wait 首个可判定 status。
3. 在最终入队锁内执行 local control gate。
4. 精确 match `studio_mode`：
   - `stop`：允许。
   - `start_play | run_server`：要求 `studio_control_state == ready`，且 transition 不是 `stopping_requested | stopping_observed`。
   - `None`、过期、其他字符串：拒绝。

这保留“controlled running session 可以被显式 stop/control”的现有能力，同时不再用 hub 二手状态放行。

#### official MCP

official MCP 分两类：

- `official_mcp_ping` / adapter 诊断类：要求 route + active helper/capability fence，但不要求 adapter ready。
- 真实 official MCP 动作：要求 route + local official gate。

真实 official MCP 动作要求：

1. hub route ready。
2. 本地 `studio_mode == stop` 且 mode age 新鲜。
3. 本地 `official_mcp_adapter_state == ready` 且 adapter age 新鲜。

### 5. 入队顺序

`queue_tool_request` 是 `/mcp` 与 `/proxy` 的统一入口，建议顺序：

1. normalize + validate + task_id sanitize。
2. 按工具类型执行 hub route gate。这个步骤会 await hub，不能持有 `state` lock。
3. 如果工具需要本地 live status 且当前没有可判定 status，bounded wait。
4. 拿 `state` lock。
5. 在同一把 lock 内完成 active helper task_id fence、local Studio gate、request id collision check、入队、`output_map.insert`。

这不是为了防 task_id 碰撞，而是为了避免普通连接生命周期变化导致“用旧 local status 判定、新连接入队”。

official MCP dispatch 同理：hub route gate 在锁外；local official gate、active helper fence、`output_map.insert`、发送 request 的 sender 选择必须在同一次锁内完成。

### 6. `/status` 输出

`/status` 不再用 hub snapshot 填 `studio_mode` 等字段。新增来源字段：

- `route_status_source`: `hub | hub_error | hub_unconfigured | task_id_unconfigured`
- `studio_status_source`: `local_helper_live | local_helper_missing | local_helper_stale | local_task_status_missing | task_id_mismatch`

短期兼容策略：

- 保留旧 `status_source` 字段。
- `status_source` 继续表达 route 层来源，例如 `hub` / `hub_error` / `hub_unconfigured`。
- 不把 `status_source` 改成 `hub+local_helper_live`，避免打断现有 `studio_debug_preflight.py` 等脚本。

ready 字段计算：

- `task_services_ready`: 只来自 hub route。
- `launch_ready`: hub route ready 且本 server 有当前 task 的 active helper，且 active helper WebSocket fresh。
- `edit_ready`: `launch_ready` + 本地 live `studio_mode == stop` + `studio_mode_age_ms` 新鲜 + `edit_runtime_state == ready` + `edit_runtime_age_ms` 新鲜。
- `official_ready`: `edit_ready` + 本地 live `official_mcp_adapter_state == ready` + `official_mcp_adapter_age_ms` 新鲜。

hub route 失败但本地 live status 存在时，`/status` 可以展示本地 `studio_mode` 方便 debug，但 `launch_ready/edit_ready/official_ready` 必须是 false。hub route ready 但本地 active helper WebSocket stale 时，`launch_ready/edit_ready/official_ready` 也必须是 false。

### 7. dev-infra/status 消费者同步迁移

同一阶段必须更新 `roblox-dev-infra`。执行上要和 server `/status` 新字段同一个 PR / 同一个部署窗口收口，不能先让 dev-infra 依赖尚不存在的字段：

- `cluster_health.py` 不再从 hub `helpers[].active_tasks[].studio_*` 计算 `edit_ready` / `official_ready`。
- Studio readiness 优先读 MCP `/status` 的 local studio 字段与 `studio_status_source`。
- hub `active_tasks[].studio_*` 只保留为 debug mirror，不参与 readiness。
- `debug_status_summary`、`describe_server_readiness`、launch preflight、stop preflight 都要按这个边界调整。
- `studio_debug_preflight.py` 如果检查 `status_source`，要么继续接受 route 层 `hub`，要么显式检查新增 `route_status_source` / `studio_status_source`。
- 兼容读取只能用于过渡展示：如果 MCP `/status` 新字段缺失、超时或 local studio source 不可用，`edit_ready/official_ready` 必须 false，不能回退 hub Studio mirror。

这一步不能放到“以后再看”，否则 server gate 修对了，脚本层仍会按 hub 二手状态误判。

### 8. hub 字段降权与后续清理

分两步：

1. 本轮不删除 hub wire 字段，但所有 gate/readiness 不再读取 hub Studio 字段。文档和 status contract 标注 hub `active_tasks[].studio_*` 是 legacy debug mirror，不能作为权威。
2. 下一轮更新 hub/status contract，让 hub 自己只保留 active task id、remote connection state、helper last seen、blocked、capacity 等连通性字段。
3. 如果需要一个 debug/status 聚合入口展示 Studio live 字段，聚合层必须实时读取权威源，比如 MCP server `/status` / `/v1/debug/state` 或 helper debug endpoint；读取失败就返回 `unavailable` / `stale` / `source_error`，不能用 hub 本地缓存临时猜 stop/play/edit/official 状态。

不要在同一 PR 既改 server gate 又删除 hub 字段，避免无谓破坏旧展示链路。

## 工具矩阵

| 工具 | hub route gate | local Studio gate | 说明 |
| --- | --- | --- | --- |
| `launch_studio_session` | 是 | control gate | 需要 route 可用，本地判断 stop/play/control/stopping |
| `start_stop_play` | 是 | control gate | stop-only，running 时要求本地 control ready |
| `run_code` diagnostic=false | 是 | stop gate | 要求本地 stop |
| `run_code` diagnostic=true | 可不要求 | 无 stop gate | 仍需 active helper task fence |
| `insert_model` | 是 | stop gate | 要求本地 stop |
| official MCP 真实动作 | 是 | official gate | 要求本地 stop + adapter ready |
| official MCP ping/诊断 | 是 | capability/active helper fence | 不要求 adapter ready |
| `get_studio_mode` | 可不要求 | 无 gate | 诊断工具，返回真实 helper/plugin 观察 |
| `take_screenshot` / `read_studio_log` | 可不要求 | 无 Studio gate | 仍需 active helper task fence |

“可不要求”是诊断/观测例外，不代表本地 live status 可以绕过 route gate 去执行控制动作。如果后续决定 hub blocked/service 权威要覆盖所有 helper-routed 操作，可以把“可不要求”收紧成“是”，但那是策略收紧，不是本轮根因。

## 测试计划

### Rust server 单元测试

- hub route ready 但本地 `task_status` 缺失或 control 所需字段缺失时，control gate bounded wait 后 fail closed。
- hub route ready 但本地 active helper WebSocket stale 时，`launch_ready/edit_ready/official_ready` 全 false。
- hub route ready 但本地 `studio_mode=start_play` 且 `studio_control_state=lost` 时，control tool 被拒绝。
- hub route ready 但本地 `studio_transition_phase=stopping_observed` 时，control tool 被拒绝。
- hub route ready 但本地 `studio_mode=starting` / `unknown` 时，control tool 被拒绝。
- hub route ready 但本地 `studio_mode=stop` 时，`RunCode` / `InsertModel` stop gate 通过。
- hub route ready 但本地 `studio_mode_age_ms` stale 时，stop gate 拒绝。
- hub route ready 且本地 official adapter ready 且 age 新鲜时，official MCP gate 通过。
- hub route ready 但本地 official adapter stale 或 not ready 时，official MCP gate 拒绝。
- non-diagnostic `run_code` / `insert_model` 和 official 真实动作在 local status 不可判定时 bounded wait，超时后 fail closed。
- hub route 失败但本地 live ready 时，control/official/edit 操作仍 fail closed。
- hub 中故意给出相反 `studio_mode`，server gate 结果仍由本地 `active_helper.task_status` 决定。
- local live gate 与入队在同一把锁内执行：测试 helper 替换/断连后不会用旧 status 入队。
- diagnostic `run_code` 不走 stop gate，但仍不能绕过 active helper task fence。
- `/status` 在 hub error + local live 时展示 local Studio 字段，但 `launch_ready=false`。

### Python dev-infra 测试

- `test_cluster_health.py`：hub active task 显示 `stop`，MCP `/status` local 显示 `start_play` 或 stale 时，不得报 `edit_ready=true`。
- `test_cluster_health.py`：hub active task 显示 `stop/ready`，但 MCP `/status` 超时、缺少 `studio_status_source` 或 local studio source 不可用时，不得回退 hub Studio mirror，`edit_ready/official_ready` 必须 false。
- `test_cluster_health.py`：hub route ready + MCP local stop/ready 时，`edit_ready/official_ready` 正确为 true。
- `test_debug_cluster_status.py`：summary 中 route 状态与 studio 状态来源分开展示。
- `test_launch_game_cli.py`：launch preflight 不再因为 hub debug mirror 的 stop/play 字段误判。
- `test_stop_game.py` 或等价测试：stop 前读取 MCP `/status` 的 local Studio 字段，不再依赖 hub active task studio mirror。
- `status-contract.md` 示例更新：标明 hub Studio 字段非权威，MCP `/status` 的 studio 字段来自 local helper live。

### hub 测试

本轮如果不删除 hub 字段，只需更新命名/注释和 contract 测试，不改变 wire contract。

下一轮清理 hub 字段时再补：

- hub helper heartbeat 只保存 active task id / remote state / last seen，不保存 stop/play。
- `/status` 不再把 stop/play 组织在 route readiness 结构中。

## 迁移顺序

1. 更新 draft/status contract，定义 hub 只拥有 route/helper 连通性字段；hub 里的旧 Studio 字段只是 legacy debug mirror。若存在聚合 status，需要实时读取 MCP server/helper 的 live source，失败时显式 unknown/unavailable。
2. 在 server 内引入 `HubTaskRouteSnapshot` 和 `LocalStudioLiveSnapshot`。
3. 把 `require_stop_mode_snapshot`、`require_studio_control_snapshot`、`require_official_adapter_ready_snapshot` 改成本地 live status 判断。
4. 修改 `queue_tool_request` 和 official MCP dispatch：hub route gate 在锁外，local live gate 与最终入队/dispatch 同锁。
5. 修改 server `/status` 的 Studio 字段来源和 source 标记。
6. 同 PR 更新 dev-infra status/readiness 消费者，让 Studio readiness 读取 MCP `/status` local studio source，不再读 hub `active_tasks[].studio_*`。
7. 补齐 Rust + Python 防回归测试。
8. 下一 PR 再清理 hub `/status` 中 stop/play 字段的权威语义和 wire 字段；如仍需要聚合展示，新增实时聚合路径，而不是继续复用 hub 缓存字段。

## 验收标准

- `launch_studio_session` / `start_stop_play` / `run_code` / `insert_model` / official MCP 的 Studio gate 不再读取 hub 中的 stop/play 字段。
- hub 不可达或 route 不 ready 时，涉及 route 的操作 fail closed；不会只因本地 Studio 状态 ready 而放行。
- 本地 helper live status 缺失或过期时，涉及 Studio 状态的操作 bounded wait 后 fail closed；不会 fallback 到 hub Studio 字段。
- `/status` 明确区分 route source 与 studio source；旧 `status_source` 保持 route 层兼容。
- dev-infra readiness 不再从 hub active task 的 Studio 字段判断 edit/official ready。
- 关键测试覆盖 hub Studio 字段与本地 live status 冲突的情况。
