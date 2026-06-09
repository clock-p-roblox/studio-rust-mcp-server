# helper 持有 Studio session 状态设计草案

## 目标

把 Windows / Studio 侧状态收敛到 helper。插件只执行 Studio API、观测 Studio 事实并向 helper 上报；hub 只保存 helper 摘要；clockp MCP / task-server 不缓存、不解释 helper 侧状态。

本次只做最小闭环，不做完整大重构：

- 保留现有 tool 路由与 hub heartbeat。
- 新增最小字段表达“当前 play 是否可被 helper 控制”。
- stop 前先判断控制链是否存在，避免 helper 已写 stop request 但运行态无人消费时傻等 60 秒。
- 不引入复杂 session 对象、不改 official MCP 调度、不改 Rojo 转发。

## 状态权威

### helper 权威状态

helper 对每个插件实例维护：

- `studio_mode`: `stop | start_play | run_server`
- `studio_control_state`: `none | ready | lost`
- `studio_transition_phase`: `idle | starting | running | stopping | error`
- `studio_control_observed_at`: 最近一次运行态控制脚本心跳时间

其中 `studio_mode` 仍来自插件观测，但是否可控由 helper 根据控制脚本心跳决定。

### 插件职责

插件只做：

- 执行 `StudioTestService` API。
- 安装运行态控制脚本。
- 等待 Studio 日志。
- 把工具响应、注册信息、运行态控制脚本心跳发给 helper。

插件不再隐含宣称“stop 一定可执行”。stop 之前必须能证明运行态控制脚本仍在。

### hub / clockp MCP

hub 只保存 helper heartbeat 中的摘要字段，不解释 Studio 生命周期。

clockp MCP 继续从 hub 读快照做门控，但不自行推断 stop/play 细节。

## 最小状态流

### start_play

1. 插件安装运行态控制脚本。
2. 插件调用 `ExecutePlayModeAsync`。
3. 插件观测到 start log 后返回 `studio_mode=start_play`。
4. 运行态控制脚本向 helper 周期性上报心跳。
5. helper 将该实例置为 `studio_control_state=ready`，`studio_transition_phase=running`。

### stop

1. 插件收到 stop tool。
2. 插件 POST helper stop request。
3. helper 只有在该实例 `studio_control_state=ready` 时接受 stop request。
4. 运行态控制脚本 GET 到 stop request 后调用 `EndTest`。
5. 插件等到 stop log 后返回 `studio_mode=stop`。
6. helper 将状态更新为 `studio_mode=stop`、`studio_control_state=none`、`studio_transition_phase=idle`。

如果插件直接观测到 Studio 已经是 `stop`，则以该事实为准，清理本地 tracker 与控制脚本后返回，不再向 helper 发送 stop request。

如果第 3 步不满足，helper 返回明确错误：

```text
uncontrolled_play_session: Studio is in play mode but helper has no fresh control heartbeat
```

## 新增接口

### POST `/v1/mcp/plugin/control-heartbeat`

运行态控制脚本调用，payload：

```json
{
  "instance_id": "...",
  "mode": "start_play"
}
```

helper 校验 instance 存在后更新：

- `studio_mode=mode`
- `studio_control_state=ready`
- `studio_transition_phase=running`
- `studio_control_observed_at=now`

## Todo

- [x] helper: 给 `PluginInstance` 增加最小控制状态字段。
- [x] helper: 增加 control heartbeat request/handler/route。
- [x] helper: heartbeat 上报 `studio_control_state` 与 `studio_transition_phase`。
- [x] helper: stop request handler 在 play/run 下要求 fresh control heartbeat，否则返回 `409 uncontrolled_play_session`。
- [x] helper: 插件 response 更新状态时按 `studio_mode` 收敛控制状态。
- [x] plugin: 运行态控制脚本增加 control heartbeat。
- [x] plugin: `RunScriptInPlayMode` 保持一次性测试语义，不安装持久运行态控制脚本。
- [x] plugin: stop 失败时保留 helper 返回的明确错误，不再把所有情况压成等 stop log。
- [x] plugin: 已观测到 Studio 为 stop 时直接清理 tracker，不再请求 helper stop。
- [x] hub: active task payload 透传新字段。
- [x] clockp MCP: status / 快照解析透传新字段，不新增策略。
- [x] tests: helper 单测覆盖 ready/lost stop request。
- [x] tests: hub payload 透传字段。
- [x] tests: clockp MCP status 解析字段。
- [x] debug HTTP: 给 hub / helper / clockp MCP 增加可脚本读取的实体状态接口。
- [x] debug script: Studio 测试前必须读取 hub / helper / clockp MCP 实体状态并判定通过。
- [x] debug script: 如果 place / task / helper / server 状态不一致，禁止启动或继续 Studio 测试。
- [x] helper: 插件状态变化时立即通过 remote WS 推送 task status，避免 server 状态落后一拍。
- [x] tests: 增加控制心跳丢失/恢复、无插件注册、server readiness 矩阵、preflight 聚合失败测试。
- [x] Studio 实机验证：不连 dev.clock-p.com，且必须在 debug script 判定通过后执行。

## Checklist

- [x] `cargo fmt --check`
- [x] `cargo test --bin studio_helper`
- [x] `cargo test --bin roblox_hub`
- [x] `cargo test --bin rbx-studio-mcp`
- [x] Windows 本机启动本地 hub。
- [x] Windows 本机启动本地 helper 指向本地 hub。
- [x] Windows 本机启动本地 clockp MCP HTTP 实例指向本地 hub。
- [x] 运行 Studio 前置状态读取脚本。
- [x] 使用本地 task 配置验证 helper register / claim / heartbeat。
- [x] 验证 stop request 在无 control heartbeat 时快速失败。
- [x] 验证 control heartbeat 后 stop request 被接受。
- [x] 验证 helper / hub / clockp MCP status 透传 `studio_control_state` 与 `studio_transition_phase`。
- [x] 验证已有 happy path 单测不回退。

## 测试集

### 单元与脚本测试

- Studio 启动参数：`studio_launch_args_include_place_and_universe_ids`、`studio_launch_args_reject_missing_universe_id`，确保 helper 自动启动 Studio 时必须带 `-placeId` 与 `-universeId`。
- 控制状态机：覆盖无控制心跳时 stop 拒绝、fresh heartbeat 时 stop 接受、已 stop 时重复 stop 接受。
- 故障恢复：`control_heartbeat_restores_lost_running_instance_and_stop_moves_to_stopping` 模拟运行态心跳丢失后 stop 快速失败，再由 heartbeat 恢复，随后 stop 进入 `stopping`。
- 非法输入：`control_heartbeat_rejects_invalid_or_stopped_modes_without_mutating_instance` 确保非法 mode / stop heartbeat 不污染状态。
- 过期实例：`stop_request_status_returns_gone_for_expired_instance` 确保插件实例丢失时返回明确 GONE。
- 状态推送：`immediate_remote_task_status_heartbeat_queues_current_snapshot` 确保状态变化会立即向 server 推送当前 task status。
- server readiness：play 中 `launch_ready=true` 但 `edit_ready=false`、`official_ready=false`；helper stale 时三者都 false。
- preflight：`util/test_studio_debug_preflight.py` 模拟 task expired、helper 未 active、插件未注册、server 无 active helper，要求一次性聚合多个错误。

### Studio 实机操作

本地无鉴权链路，不连 dev.clock-p.com：

1. 启动本地 hub。
2. 创建 task，placeId 为 `134795435066737`，故意不传 universeId。
3. 启动本地 clockp MCP HTTP server。
4. 启动 Windows helper，由 helper claim task 并解析 universeId。
5. 确认 Studio 命令行：`-task EditPlace -placeId 134795435066737 -universeId 9327304100`。
6. 等待插件注册，运行 `util/studio_debug_preflight.py --require-plugin --check-studio-log`。
7. 通过 MCP 调 `get_studio_mode`，确认初始 `stop`。
8. 调 `launch_studio_session(start_play)`。
9. 读取 helper / server / hub，确认 helper 与 server 均为 `start_play / ready / running`，official adapter 为 `blocked_by_studio_mode`。
10. 调 `start_stop_play(stop)`。
11. 读取 helper / server / hub，确认 helper 与 server 均为 `stop / none / idle`，official adapter 为 `ready`。
12. 在已 stop 状态重复 `start_stop_play(stop)`，确认幂等成功且状态不漂移。

### Studio 实测结果

- preflight：OK。
- 初始 mode：`stop`。
- `launch_studio_session(start_play)`：约 1.68s，成功进入 `start_play`。
- server 在 start 后立即看到：`start_play / ready / running`，`server_last_message_age_ms` 约 2-4ms。
- `start_stop_play(stop)`：约 0.88s，成功回到 `stop`。
- server 在 stop 后立即看到：`stop / none / idle`，`server_last_message_age_ms` 约 13-16ms。
- 重复 stop：约 0.11s，成功且状态保持 `stop / none / idle`。

结论：play/stop 工具链和 helper/server/hub 状态链路当前稳定。之前发现的 server 状态落后一拍问题，已通过状态变化即时 WS 推送修复。

## Studio 测试前置状态读取协议

### 问题记录

上一轮 Studio 实机测试无效。helper 确实拉起了 Studio 进程，但 Studio 弹出 `Error fetching latest place version`，说明 place 没有正确打开；该测试只能证明进程启动和部分 HTTP/WS 链路存在，不能证明插件运行态正确。

该记录不得作为通过依据。

### 新增只读 HTTP 接口

本次新增只读 debug 接口，只服务于脚本化验收，不参与业务决策：

- hub: `GET /v1/debug/helpers/{helper_id}`，读取 helper 记录、active task、blocked、last seen、task ownership。
- hub: 继续使用现有 `GET /v1/tasks/{task_id}` 读取 task 事实。
- helper: `GET /v1/debug/tasks/{task_id}`，读取 helper 对该 task 的 claimed task、launch process、plugin instances、remote connection、official adapter、当前 task snapshot。
- clockp MCP/server: `GET /v1/debug/state`，读取 active helper connection、helper task_status、queue / pending request 计数、hub snapshot 摘要。

这些接口不得返回 token、bearer、recover token、task token 或本机敏感路径之外的秘密数据。

### Studio 前置判定

Studio 测试前脚本必须并行、只读、限时读取各实体状态；默认预算 1 秒左右，超时视为前置检查失败，不继续进入 Studio 动作测试：

1. hub task：task 存在、`service_state=ready`、`accepting_launches=true`、`released=false`、`claimed_by_helper_id` 符合预期或为空。
2. hub helper：helper 存在、未 blocked、`active_tasks` 不超过 capacity。
3. helper task：helper 已指向本地 hub；如果测试要自动打开 Studio，必须看到 task 已 claim 且 launch process 的 place_id 与 task place_id 一致。
4. clockp MCP/server：`task_id` 与 hub task 一致；如已启动 server，`status_source=hub` 且 hub snapshot 无错误。
5. Studio log / place 校验：如果使用线上 placeId 自动打开 Studio，必须在 Studio 日志中没有 `DataModelLoadingFailure` / `Error fetching latest place version` 后，才允许把插件注册视为有效。

前置脚本不得修复状态，不得 release task，不得 stop/play，不得调用 official MCP 真实功能；它只负责阻止盲测，并一次性报告 hub / helper / server / Studio log 的事实差异。

任何一步失败，脚本必须停止，不执行 Studio play/stop 工具测试。

### 推荐测试路径

优先使用两段式测试：

- 无 Studio 测试：只启动本地 hub/helper/server，验证 HTTP debug 状态、helper claim、helper/server WS、无 control heartbeat stop 快速失败。
- Studio 测试：只在前置状态脚本通过后执行。若线上 placeId 不可打开，应改用明确的可打开 place，或改为本地空白 place + `PlaceId=0` + helper `--skip-claim-studio-launch` 的专用路径，不把失败的线上 place 当作实机验证。

## HTTP debug 方案 3x reviewer

### Reviewer A：状态来源

结论：通过。新增接口只读实体状态，不改变状态所有权；hub 读 hub 实体，helper 读 helper 实体，clockp MCP/server 读 server 实体。脚本只做验收判定，不反向写状态。

### Reviewer B：最小接口

结论：通过。只新增三个缺口接口：hub helper detail、helper task detail、server debug state。已有 hub task detail 和 `/status` 继续复用，不重复建模。

### Reviewer C：防止盲测

结论：通过。Studio 测试必须由脚本先读实体状态并检查 place 打开事实；出现 task/helper/place 不一致或 Studio 打不开 place 时直接停止，不能把进程存在当作通过。

### preflight 验证记录

- 脚本：`util/studio_debug_preflight.py`
- 本地 hub：`http://127.0.0.1:44758`
- 本地 helper：`http://127.0.0.1:44750`
- 本地 clockp MCP/server：`http://127.0.0.1:44880`
- 本地 task：`tfb42dbb7b2`
- 本地 place：`0`
- 结果：`[studio_debug_preflight] OK`
- 注意：该记录只证明 Studio 前置实体读取门禁可用；没有进入 Studio，不作为 Studio 实机验证。

### placeId 134795435066737 实测记录

- 使用 place：`134795435066737`
- helper 启动命令：`RobloxStudioBeta.exe -task EditPlace -placeId 134795435066737`
- 本地 hub/helper/clockp MCP/server 基础链路：启动成功，helper 曾 claim task，server 曾看到 active helper。
- Studio 结果：插件未注册，helper `active_instances=0`。
- Studio 日志结果：`DataModelLoadingFailure: Error fetching latest place version`，并出现 `Place does not exist`。
- preflight 结果：失败，不能继续 play/stop 或 MCP tool 实测。
- 后续状态：task 过期，helper 释放 claim，server 无 active helper。

作废结论：该失败不是账号/权限的充分证据。后续复核发现官方 Studio CLI 打开已发布 place 时需要同时传 `-placeId`、`-universeId`、`-task`；本记录缺少 `-universeId`，因此不能用于判断 place 本身不可打开。

### placeId 134795435066737 + universeId 9327304100 复测记录

- helper 在 task 缺少 universeId 时，会通过 `https://apis.roblox.com/universes/v1/places/{placeId}/universe` 解析 universeId。
- `134795435066737` 解析得到 `9327304100`。
- helper 实际启动命令：`RobloxStudioBeta.exe -task EditPlace -placeId 134795435066737 -universeId 9327304100`。
- Studio 日志确认：`placeid: 134795435066737`、`universeid: 9327304100`。
- 插件注册成功。
- `util/studio_debug_preflight.py --require-plugin --check-studio-log` 返回 OK。

结论：前一轮 place 0 / latest version 失败的根因是 helper 自动启动 Studio 时没有传 `-universeId`。修复后命令行和 Studio 日志里的 place/universe 都正确。

### preflight 聚合错误记录

`util/studio_debug_preflight.py` 已改为并行读取并聚合实体状态错误，不再遇到第一个失败就停止。这样在 Studio 打不开且插件未注册时，脚本能一次性报告完整阻塞点，避免只看到“插件没注册”而漏掉 place 打不开的根因。

当前失败现场下，脚本能同时报告：

- helper 未 claim task。
- hub task 已过期且不接受 launch。
- hub helper active_tasks 为空。
- clockp MCP/server 无 active helper。
- Studio 日志包含 `DataModelLoadingFailure`、`Error fetching latest place version`、`Place does not exist`。

## 3x reviewer 审核

### Reviewer A：状态权威

通过条件：helper 是 Windows 侧唯一状态源；插件只上报事件；hub / MCP 只透传。

结论：通过。新增字段挂在 helper plugin instance，由 helper heartbeat 上报。hub 与 MCP 只解析和展示，不基于这些字段新增复杂策略。

风险：插件仍保留 `StudioModeTracker`，但本次只把“是否可 stop”这一关键事实迁到 helper；完整移除 tracker 属于后续大重构。

### Reviewer B：最小字段

通过条件：不用复杂 session object；字段能解释当前 stop 卡住的核心原因。

结论：通过。只新增 `studio_control_state` 与 `studio_transition_phase` 两个对外字段，内部只加一个 observed_at 时间。

风险：没有显式 `session_id`，旧运行态心跳理论上可能污染新 session。当前插件 instance 与 Studio PID 绑定，且 stop 卡住的当前问题优先解决；session_id 放入后续版本。

### Reviewer C：无兜底与可测试

通过条件：不能在控制链丢失时盲等或自动猜 stop；必须明确失败。

结论：通过。stop request handler 在 helper 侧硬拒绝不可控 play，插件直接返回错误，不进入 60 秒 stop log 等待。

风险：如果控制脚本刚启动但第一次心跳未到，立即 stop 会返回 uncontrolled。这个是有意设计，不做隐式等待，避免兜底。

## 实现后 3x reviewer 复审

### Reviewer A：状态权威复审

结论：通过。helper 是唯一会判断控制链是否存在的一侧；hub 和 clockp MCP 只透传字段。插件只负责安装控制脚本、调用 Studio API、上报 mode 与 heartbeat。

### Reviewer B：最小实现复审

结论：通过。对外只新增 `studio_control_state` / `studio_transition_phase` 两个字段，内部只新增心跳时间戳与小型判断函数。没有引入 session object，没有改 official MCP / Rojo 行为。

### Reviewer C：无兜底复审

结论：通过。不可控 play 下 stop request 明确返回 `409 uncontrolled_play_session`；插件观测到 Studio 已是 stop 时按事实清理 tracker，不再伪造 stop 请求或等待不存在的 stop log。

### 已作废的本地验证记录

- 本地 hub: `http://127.0.0.1:44758`
- 本地 helper: `http://127.0.0.1:44750`
- 本地 clockp MCP HTTP: `http://127.0.0.1:44756/status`
- 本地 task: `t37ba06d59a`
- 无 control heartbeat 的 stop request 返回 `409 uncontrolled_play_session`。
- control heartbeat 后 stop request 返回 `stop_requested=true`。
- helper / hub / clockp MCP 最终均显示 `studio_mode=stop`、`studio_control_state=none`、`studio_transition_phase=idle`。

作废原因：后续 Studio 实机测试发现 place 未正确打开，不能把这段记录当作 Studio 验收依据。保留它只作为早期 HTTP 链路探索记录。

## 状态传播复审与修正

### 问题

实现后复查发现 helper 状态有两条出口：

- helper -> clockp MCP/server remote WS：插件状态变化时已经即时推送。
- helper -> hub heartbeat：仍然只依赖 5 秒维护循环。

clockp MCP 的门控事实来源是 hub `/status`，不是 server 自己缓存。因此如果 play/stop 刚完成后立刻调用 `run_code` 或 official tool，server 可能已经通过 WS 看到了新状态，但 hub 仍保留旧快照，门控仍会读到 stale / running / not_ready。

这不是需要增加兜底等待的问题，而是状态传播模型不自洽：helper 作为 Windows 侧状态权威，任何状态变化都必须同时推动两个只读下游。

### 修正原则

- 不增加新的 hub API。
- 不让 clockp MCP 改读自己的 WS 缓存。
- 不让 server 推断 helper 状态。
- helper 仍是状态写入源，hub 仍是 Linux / server 侧门控事实源。
- 插件状态变化只调用 helper；helper 负责把同一份 task status 同步给 remote WS 与 hub heartbeat。

### 实现

- 新增 helper 内部 `hub_heartbeat_notify`。
- 新增统一出口 `queue_task_status_updates`。
- 插件 stop request、control heartbeat、status update、tool response 中的状态变化，统一调用 `queue_task_status_updates`。
- `queue_task_status_updates` 会尽力向 remote WS 推送 task status，同时唤醒 hub heartbeat 循环。
- 即使当前没有 remote WS 连接，也必须唤醒 hub heartbeat；因为 hub 仍需要最新 helper-owned task status。
- hub heartbeat 保留 5 秒周期循环，`Notify` 只是增加立即唤醒，不替代周期健康上报。

### 新增测试

- `immediate_task_status_update_queues_remote_snapshot_and_hub_notify`：有 remote WS 时，状态变化同时产生 server heartbeat frame 和 hub heartbeat notify。
- `task_status_update_notifies_hub_without_remote_connection`：无 remote WS 时，状态变化仍会唤醒 hub heartbeat。

### 状态传播 3x reviewer

#### Reviewer A：事实源一致性

结论：通过。server gate 继续只读 hub，helper 状态变化会立即推动 hub 更新；没有引入第二个门控事实源。

#### Reviewer B：最小实现

结论：通过。只增加一个进程内 `Notify` 和统一出口函数，不新增外部字段、不新增协议、不改变 hub task 所有权。

#### Reviewer C：无兜底

结论：通过。修正的是状态传播时机，不是增加等待、重试或猜测。状态不满足时仍按现有门控失败，不做隐式补救。

### 本地实测记录

测试脚本：`util/local_state_sync_probe.py`。

本轮使用本地 hub / helper / clockp MCP HTTP，不连接 `dev.clock-p.com`。task 由脚本在本地 hub 创建，脚本持续发送 ready heartbeat；helper 自动 claim task 并启动 Studio。

- placeId：`134795435066737`
- universeId：`9327304100`
- 本地 task：`t64a7a1989f`
- preflight：`[studio_debug_preflight] OK`
- 初始 `get_studio_mode`：`stop`
- `launch_studio_session(start_play)`：成功，约 2079ms
- `start_stop_play(stop)`：成功，约 902ms
- 重复 `start_stop_play(stop)`：成功
- start 后 hub / helper / server 三边均为：`studio_mode=start_play`、`studio_control_state=ready`、`studio_transition_phase=running`、`official_mcp_adapter_state=blocked_by_studio_mode`
- stop 后 hub / helper / server 三边均为：`studio_mode=stop`、`studio_control_state=none`、`studio_transition_phase=idle`、`official_mcp_adapter_state=ready`

结论：helper 状态变化后，hub 不再依赖 5 秒周期碰运气；实际 play / stop / repeat stop 链路里，hub、helper、server 三边状态均能在工具调用完成后立刻收敛。

### 断线恢复实测记录

测试脚本：`util/local_disconnect_probe.py`。

本轮仍只使用本地 hub / helper / clockp MCP HTTP，不连接 `dev.clock-p.com`。task-server 进程保持运行，中间只切断 helper 到 clockp MCP/server 的 TCP 代理 120 秒，用来模拟数据面网络断线。

- 断线前：连续 2 轮 `launch_studio_session(start_play)` / `start_stop_play(stop)` 均成功。
- 断线中：helper remote state 从 `retrying` 收敛到 `connecting`；task-server `/status` 仍可读，`status_source=hub`，进程未退出。
- 恢复后：helper 重新连上 clockp MCP/server，connection id 变化，`util/studio_debug_preflight.py` 再次返回 OK。
- 恢复后：再次连续 2 轮 `launch_studio_session(start_play)` / `start_stop_play(stop)` 均成功。
- 最终三边状态：`studio_mode=stop`、`studio_control_state=none`、`studio_transition_phase=idle`、`official_mcp_adapter_state=ready`。

结论：helper 到 server 的数据面断线不会让 task-server 挂死；恢复连接后，hub / helper / server 状态能重新收敛，play/stop 仍可继续执行。该测试不验证公网 bridge，只验证本地实体状态模型与断线重连行为。
