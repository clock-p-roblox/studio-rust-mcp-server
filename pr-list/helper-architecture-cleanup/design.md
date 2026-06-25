# helper 架构整理 draft

## 目标

把 Windows helper 从“所有事情都管的状态大脑”整理成“本机事实拥有者 + 单步动作执行器”。

这次整理不推翻 hub / helper / Studio 插件 / runtime / clockp MCP 的总体架构，只重新划清职责边界，消掉 stop/play、runtime-log、状态观测里已经反复打出来的问题。

## 2026-06-25 复核结论

本轮只完成了代码拆分提交 `8f2ea89 Refactor Studio helper modules`。该提交不改变 stop/play、runtime-log、状态 gate 的运行语义。

当前 draft 与代码对照后的结论：

- P0.1 已完成：新拆出的 Rust / Luau / Python 模块已经进入提交。
- P0.2 仍需 CI 或 Linux 侧验证：Windows 本地 `cargo clippy -- -D warnings` 已通过，但 Windows 客户端不负责代替 Linux CI。
- P0.3 仍成立，但不得混入“纯拆分”提交；应作为独立健壮性修复处理。
- P1 / P2 / P3 / P4 主线问题仍成立，拆分没有修复它们。
- “任何情况下都可以 stop”的准确语义需要收窄：stop 不应受 helper-launched / ownership / fresh heartbeat 这类权限条件限制；但在 `none_response` / `none_connected` 等无法确认执行端的状态下，必须明确失败并返回 reason，不能猜测或兜底。
- `explicit-session-state-launch-restart` draft 比本总 draft 更接近 stop/play 的最终状态机；后续实现应以它的状态枚举和单步原语语义为准。

## 当前职责边界

### clockp MCP

职责：

- LLM 唯一入口。
- 校验 `task_id`。
- 调用 hub / helper / Studio 插件链路。
- 暴露稳定工具和诊断字段。
- 把失败原因和诊断字段返回给 LLM。

边界：

- 不缓存 Windows Studio 的 play/stop 真相。
- 不直接猜 Studio 当前状态。
- 不复制 helper 的状态机。
- 不负责 helper/task 所有权管理。

### Linux 高层脚本

职责：

- `launch-game.py` / `stop-game.py` 承担用户意图层编排。
- `launch-game.py` 默认 restart 时按 `status -> stop -> status -> launch` 走。
- `stop-game.py` 在已 stop 时 no-op 成功，在 play 时显式 stop。
- 把每一步的状态和 reason 打印给 LLM。

边界：

- 控制动作只能走 clockp MCP / 标准工具入口。
- 状态读取可以走 clockp MCP 或明确公开的 hub/helper status API。
- 不绕过工具入口直接调用 helper 私有控制 HTTP。
- 状态查询只用于选择下一步；helper 执行动作时必须重新裁决当时事实。

### hub

职责：

- 管 task / helper register / heartbeat / claim。
- 管 task 与 helper 的绑定事实。
- 透传 helper 上报的 task status。

边界：

- 不裁决 Studio play/stop。
- 不推导 Studio 状态机。
- 不执行 Studio 控制动作。

### Windows helper

职责：

- 本机 Studio / 插件 / runtime 的事实入口。
- 启动 Studio。
- 接收插件注册和工具回包。
- 提供 live probe / status。
- 执行单步动作：launch、stop、run code、screenshot、Rojo forward、runtime-log forward、official adapter。
- 可以保存最近一次状态作为诊断，但所有控制动作前必须重新获取或重新合成当前事实。

边界：

- 不在 `LaunchStudioSession` 里隐式 stop 再 launch。
- 不用旧缓存替代 live probe。
- 不用“是否 helper 启动”当 stop 权限。
- 不把 task 编排策略搬进 helper。

### Studio 插件

职责：

- 注册到 helper。
- 回答 live probe。
- 在 edit runtime 执行 edit 类工具和 launch。
- 在 play/runtime 侧暴露 stop actuator 相关能力。
- edit runtime 和 play runtime 是两个事实来源；helper 合成状态时不能把它们当成同一个实例。

边界：

- 查询状态不应有副作用。
- `getStudioMode()` 不应读日志并修改状态。
- 不决定 restart 策略。
- 不管理 task 归属。

### play/server runtime

职责：

- poll stop request。
- 调 `StudioTestService:EndTest`。
- 上报 stop observed / failed。
- 把 runtime-log 发给 helper。

边界：

- 不直接连 Linux runtime-log 公网服务。
- 不决定是否 restart。

### Rojo 插件

职责：

- 从 helper 获取 pathful Rojo base URL。
- 只连接 helper 本机 `/rojo-forward/{place_id}/task/{task_id}`。

边界：

- 不直接连接 Linux Rojo。
- 不参与 stop/play 裁决。
- 不管理 task 归属。

### runtime-log 服务

职责：

- Linux runtime-log 服务拥有最终日志落盘事实。
- helper 只拥有本机接收事实、转发尝试和最近错误摘要。

边界：

- Roblox runtime 收到 helper ack 只代表 helper 已接收，不代表 Linux artifact 已落盘。
- helper 不应把远端 runtime-log 服务状态伪装成本机 Studio 状态。

### official adapter

职责：

- official MCP 被隐藏在 clockp MCP 后面。
- Windows helper 管 hidden StudioMcp.exe / adapter 本机生命周期。
- clockp MCP 只暴露筛选后的 official 能力。

边界：

- hub 不理解 official tool 语义。
- 单次 official tool 业务失败不应污染 adapter health。

## 状态与错误处理细则

### 状态数量

对外可用于 gate 的 Studio 会话状态只有 6 个：

```text
studio_session_state =
  stop
  starting_play
  play
  stopping
  none_response
  none_connected
```

不再增加第 7 个对外状态。

含义：

- `stop`：稳定 edit 态。edit 插件可响应，允许 launch。
- `starting_play`：launch 已经开始，但还没有拿到可靠 play runtime heartbeat。
- `play`：已拿到 fresh play runtime heartbeat。
- `stopping`：helper 已记录非 terminal stop intent，等待 play runtime 消费和 Studio 回到 edit。
- `none_response`：helper 有候选插件实例，但本次不能确认其事实。
- `none_connected`：helper 当前没有可用插件实例。

内部诊断字段可以更多，例如 `stop_request_id`、`last_poll_at`、`last_error`、`studio_mode`、`transition_phase`。这些字段只能解释状态来源和失败原因，不能成为另一套 gate 状态机。

### 状态来源

状态事实只来自三类输入：

- edit runtime fact：edit 插件上报 `stop` 或 `starting_play`。
- play runtime fact：play/server runtime heartbeat 上报 `play`。
- helper control fact：helper 记录 stop intent 后合成 `stopping`。

其他来源均不能生成状态事实：

- Studio log 只做诊断。
- hub 只透传，不推导。
- clockp MCP 只快失败，不合成。
- Linux 脚本只编排，不缓存。
- Rojo 和 runtime-log 不参与 stop/play 状态。

### 合成规则

helper 是唯一 `studio_session_state` 合成者。合成规则按优先级执行：

```text
if no selected plugin instance:
    none_connected
else if active non-terminal stop_request exists:
    stopping
else if fresh play runtime heartbeat exists:
    play
else if fresh edit runtime report == starting_play:
    starting_play
else if fresh edit runtime report == stop:
    stop
else:
    none_response
```

约束：

- `none_response` 不覆盖最后可信事实，它只表示“本次无法确认”。如果一个控制动作需要当前事实，`none_response` 必须失败。
- 多实例或 task 身份不唯一时，不靠“最新时间”猜成功态；返回明确错误。
- 任何有副作用命令到达 helper 后，都必须重新合成当时状态。上游刚查到的状态不能当执行许可。
- 多实例不唯一、task/place 身份冲突不归类成普通 `none_connected`，而是返回 identity ambiguity reason。

### 输出模型

`/status`、hub heartbeat、clockp MCP status 至少同时暴露两类字段：

```text
studio_session_state        # 本次可用于 gate 的状态，只能是 6 个状态之一
last_known_session_state    # 最近一次可信事实，只用于诊断和展示
last_session_error_reason   # 最近一次状态相关失败 reason，可为空
```

只有 `studio_session_state` 可以参与 gate。`last_known_session_state`、`transition_phase`、`stop_result_phase`、`studio_mode` 都不能参与 gate，也不能被 Linux 脚本或 clockp MCP 拼成第二套状态机。

`none_response` 时，调用方可以展示 `last_known_session_state`，但所有控制动作必须按 `none_response` 失败。

### stop request 终结规则

`stopping` 只由 active non-terminal stop request 产生。stop request 只有以下 terminal 结果：

- `completed`：helper 已确认回到 `stop`。
- `failed`：runtime actuator 上报 EndTest 或执行失败。
- `timed_out`：超过 stop 等待窗口，未确认回到 `stop`。

terminal 后：

- stop request 不再算 active，因此不能继续让状态保持 `stopping`。
- terminal reason 写入 `last_session_error_reason` 和 stop 诊断字段。
- helper 重新按当前 edit/play/runtime fact 合成 `studio_session_state`。
- 原始 `start_stop_play(stop)` 调用返回失败或成功；后续命令必须基于重新合成后的状态重新裁决。
- 不新增 `stop_failed` 状态，失败只通过 reason 和诊断字段表达。

如果 terminal 后仍能确认 fresh play heartbeat，状态可以是 `play`，但 `last_session_error_reason` 必须保留给调用者判断。helper 不自动重试 stop。

如果 terminal 后无法确认 edit/play 事实，状态为 `none_response`，并带上 terminal stop reason。

### 单一错误处理原则

所有失败只有一种主处理方式：

```text
记录诊断 -> 带 reason 返回调用者 -> 停止当前动作
```

这里的“调用者”按动作边界定义：

- 同步工具调用：调用者是当前 MCP/tool/script 请求。
- 后台转发任务：调用者是后续读取 status / preflight / wait marker 的上层流程；后台任务必须把 reason 暴露到这些入口，不能只写本地日志。
- 长连接维护：调用者是依赖该连接的下一次工具或 status 查询；连接错误必须体现在 status reason 中。

禁止：

- catch 后吞掉错误。
- catch 后换成另一个有副作用动作。
- 自动 retry 控制命令。
- 用旧状态兜底继续执行。
- 用日志或猜测把未知状态改成可执行状态。

允许：

- 在进程边界把底层错误转换成统一 `reason`。
- 在状态里记录最近错误、时间戳、request id。
- 对无副作用的连接保持机制继续使用既有 reconnect；但它不能替代本次控制命令的失败结果。
- 后台任务可以异步执行，但单个后台 item 失败后必须终止该 item，并把 reason 暴露到 status / MCP 诊断入口。

### 各环节失败处理

| 环节 | 失败例子 | 唯一处理方式 | 向上反馈 |
| --- | --- | --- | --- |
| helper 选择 task/plugin | 无实例、task 不唯一、place/task 不匹配 | 不执行工具，返回 `studio_plugin_not_connected` 或 identity reason | MCP tool error，脚本非 0 输出 |
| 状态 probe | 插件实例存在但本次无响应 | 返回 `none_response`，控制动作失败 | helper `/status`、MCP status、tool error |
| edit runtime report | edit 插件未上报或过期 | 状态不能成为 `stop`，返回 `none_response` 或具体 stale reason | helper status -> hub snapshot -> MCP status/tool error |
| play runtime heartbeat | heartbeat 过期 | 状态不能成为 `play`；如果正在控制，返回 actuator/heartbeat reason | helper status -> hub snapshot -> MCP status/tool error |
| launch | 当前不是 `stop` | 不启动，不 stop，返回对应 reason | `launch_studio_session` tool error，`launch-game.py` 非 0 |
| stop | 当前是 `play` 以外不可停状态 | 不登记 stop request，返回对应 reason | `start_stop_play` tool error，`stop-game.py` 非 0 |
| stop request poll | runtime 未 poll 到 request | 保持 `stopping` 到超时；超时将 request 标记 `timed_out`，返回 `stop_timeout` 和 request id | helper status + tool error + 脚本非 0 |
| EndTest | Roblox API 调用失败 | runtime 上报 failed；request 标记 `failed`；工具返回 reason | runtime -> helper -> MCP tool error -> 脚本非 0 |
| late plugin response | 请求已超时后才回包 | 可用于状态收敛；不改写原请求结果为成功 | helper status / MCP status 诊断 |
| hub heartbeat | hub 不可达 | helper 保留本机事实并记录 hub error；依赖 hub snapshot 的上层看到 stale reason | helper status、MCP status/tool preflight error |
| clockp MCP gate | hub/helper snapshot stale | 快失败，返回 reason；不重试控制动作 | MCP tool error，脚本非 0 |
| Linux `launch-game.py` | 任一步失败 | 非 0 退出，打印 JSON reason；不继续下一步 | 脚本输出给 LLM |
| runtime-log forward | 远端慢或失败 | 当前后台转发 item 失败并记录 reason；不得阻塞 Roblox runtime | helper status -> MCP status/preflight/wait marker reason -> 脚本非 0 |

### 控制命令分流

`launch_studio_session` 只启动：

| 当前状态 | 行为 |
| --- | --- |
| `stop` | 执行 launch |
| `play` | 失败 `studio_already_playing` |
| `starting_play` | 失败 `launch_already_in_progress` |
| `stopping` | 失败 `studio_stop_in_progress` |
| `none_response` | 失败 `studio_plugin_no_response` |
| `none_connected` | 失败 `studio_plugin_not_connected` |

`start_stop_play(stop)` 只停止：

| 当前状态 | 行为 |
| --- | --- |
| `play` | 登记 stop request，等待 stop |
| `stop` | 成功 `already_stopped` |
| `stopping` | 失败 `studio_stop_in_progress` |
| `starting_play` | 失败 `studio_starting_play` |
| `none_response` | 失败 `studio_plugin_no_response` |
| `none_connected` | 失败 `studio_plugin_not_connected` |

### 向上反馈格式

所有失败至少包含：

```json
{
  "success": false,
  "reason": "studio_stop_in_progress",
  "stage": "start_stop_play",
  "task_id": "t_example",
  "studio_session_state": "stopping"
}
```

涉及 stop 时额外包含：

```json
{
  "stop_request_id": 12,
  "stop_request_recorded_at": 1780000000000,
  "runtime_actuator_last_poll_age_ms": 250,
  "stop_result_phase": "observed",
  "last_error": null
}
```

字段缺失必须表示“该事实不存在或不可确认”，不能让调用者靠猜测补全。

后台 runtime-log 失败至少暴露：

```json
{
  "runtime_log_forward_state": "error",
  "runtime_log_forward_reason": "runtime_log_upstream_unreachable",
  "runtime_log_forward_last_error_at": 1780000000000
}
```

`launch-game.py` 等待 runtime marker 时必须把该 reason 打印给 LLM，而不是只表现为 artifact 缺失。

### 稳妥性判断

这套方案能稳妥解决当前主问题的前提是：

- 状态只有一个 owner：Windows helper。
- 控制命令只有单步语义：launch 不 stop，stop 不 launch。
- 上层脚本只编排，不偷调 helper 私有口。
- 每个失败都原路返回 reason，不被中间层吞掉。

如果实现中需要新增状态、自动重试、或中间层吞错，必须先回到 draft 重新讨论；默认不允许。

## 问题清单

### P0：提交 / CI / 基础稳定性

#### P0.1 新拆出的模块未被 Git 跟踪

现状：

- 已在提交 `8f2ea89 Refactor Studio helper modules` 中跟踪。

影响：

- 当前已解决。后续继续拆分时仍要显式检查新增文件是否被提交。

处理：

- 已完成。

#### P0.2 Ubuntu clippy 可能因 warning 失败

现状：

- CI 使用 `cargo clippy -- -D warnings`。
- Windows `cargo check` 通过，但仍有 warning。
- `studio_process.rs` 有非 Windows 下 unused import 风险。

影响：

- 功能正确也可能 CI 失败。

处理：

- 收紧 `#[cfg(target_os = "windows")]` import。
- 消掉当前 helper bin 的 warning，至少让 clippy 不被明显 warning 卡住。

#### P0.3 `summarize_error` 按字节截断 UTF-8

现状：

- `summarize_error` 使用 `&trimmed[..LIMIT]`。

影响：

- 中文或其他多字节错误文本超过限制时，截在字符中间会 panic。
- 该函数大量用于错误路径，可能让 helper 在处理错误时崩溃。

处理：

- 改为按合法 UTF-8 边界截断。
- 加单测覆盖中文长错误。
- 该项是独立 bugfix，不应混入纯拆分提交。

### P1：stop / launch 职责冲突

#### P1.1 helper 仍在 `LaunchStudioSession` 里隐式 stop 再 launch

现状：

- `LaunchStudioSession` 分支调用 `stop_running_session_before_launch_if_needed()`。

影响：

- launch 失败时无法区分 stop 失败、stop 超时、launch 失败、插件 late response。
- 与“restart 由 `launch-game.py` 决定”冲突。
- helper 继续承担高层业务策略。

处理：

- 删除 helper 内部 stop-before-launch 路径。
- `LaunchStudioSession` 只在当前 stable edit/stop 时转发给插件。
- 非 stop 状态直接返回明确 reason。

#### P1.2 stop 仍因 `uncontrolled_play_session` 被拒绝

现状：

- stop 要求 fresh play control heartbeat。
- control heartbeat 丢失时，真实 Studio 仍可能在 play，但 helper 拒绝 stop。

影响：

- 与“stop 不受 helper-launched / ownership / stale heartbeat 权限限制；是否 helper 启动只做 debug 记录”冲突。
- 可能导致 server/LLM 明知需要 stop，却被 helper 拒绝。

处理：

- stop 权限不再绑定 helper-launched 或 fresh heartbeat。
- 如果能确认当前是 play，就应登记 stop intent。
- 若 play runtime actuator 不可用，或处于 `none_response` / `none_connected`，应返回可对账 reason，例如 `stop_actuator_unavailable` / `studio_plugin_no_response` / `studio_plugin_not_connected`，而不是把整个 stop 拒绝为 ownership 问题，也不能猜测执行。

#### P1.3 多插件实例 task snapshot 可能选错事实

现状：

- `task_studio_mode_snapshot()` 在同 task 多实例下按最新 `studio_mode_observed_at` 选一个实例。
- edit runtime 和 play/runtime 是不同运行时，可能同时存在。

影响：

- fresh edit 上报可能覆盖 play 真相。
- 出现“helper 认为 edit/stop，但 Studio 实际仍在 play”的状态错乱。

处理：

- 引入明确的 `studio_session_state` 合成规则。
- 合成优先级建议：`stopping > play > starting_play > stop > none_response > none_connected`。
- 不再简单按最新时间选实例。

#### P1.4 late plugin response 被忽略

现状：

- 插件回包只有 pending 还存在时才更新相关状态。
- 控制请求超时后，插件晚回包可能被当 unknown response。

影响：

- Studio 已经完成动作，但 helper 仍保持失败或 stale 状态。
- 造成“操作成功但上游认为失败”的对账困难。

处理：

- 控制类 late response 仍应进入状态收敛路径。
- response 是否还需要返回给原请求，与状态是否可采信分开处理。

### P2：状态模型和可观测性

#### P2.1 `PluginInstance` 职责过宽

现状：

`PluginInstance` 同时保存：

- 插件身份：place/task/pid。
- Studio mode。
- control state。
- transition phase。
- edit runtime freshness。
- stop request id。
- 工具队列与 notify。

影响：

- 任意 handler 都能改关键状态。
- 状态来源混在一起，难以证明谁是事实 owner。

处理：

- 拆成：
  - `PluginRegistry`：实例身份和连接活性。
  - `StudioSessionState`：live probe 和 session state。
  - `StudioControl`：stop request / control pending / late response。
  - `PluginToolQueue`：工具派发和回包。

#### P2.2 `/status` stop 链路字段不足

现状：

- 有 `stop_request_id`，但 `/status` 和 task snapshot 没充分暴露。
- stop 超时只暴露 `last_mode / last_phase / edit_runtime_state`。

影响：

- Linux、hub、helper、插件无法对账：
  - stop intent 是否登记。
  - runtime 是否 poll 到。
  - 是否调用 EndTest。
  - stop result 是否回报。

处理：

- 暴露：
  - `active_stop_request_id`
  - `stop_request_recorded_at`
  - `stop_request_last_polled_at`
  - `stop_request_last_poll_id`
  - `stop_result_phase`
  - `stop_result_error`
  - `last_control_response_at`
  - `last_control_response_late`

#### P2.3 helper 仍用缓存状态代替 live probe

现状：

- helper 缓存 plugin 上报并自己推导状态。
- 状态查询不是一次 live probe。

影响：

- 旧状态可能持续影响 gate。
- 多实例时状态合成更难解释。

处理：

- 对外状态使用 live probe 语义：
  - `stop`
  - `starting_play`
  - `play`
  - `stopping`
  - `none_response`
  - `none_connected`
- helper 可以保存最近一次 probe 作为诊断，但执行动作时必须重新计算状态。

#### P2.4 session state 输出模型不清

现状：

- 当前状态输出没有明确区分“本次 probe 结果”和“最近一次可信事实”。
- `none_response` 容易和最后已知的 `play/stopping/stop` 混在一起。

影响：

- Linux、hub、helper 对账时无法判断：这是当前事实，还是只是本次问不到插件。
- 后续实现容易把 probe 失败当成状态事实覆盖掉 play/stopping。

处理：

- 明确输出两类字段：
  - `studio_session_state`：本次可用于 gate 的合成状态，只能是 6 个状态之一。
  - `last_known_session_state`：最近一次可信事实，只用于诊断和展示。
- 控制动作只按本次可确认事实执行。
- 无法确认时失败并返回 reason，不猜。

#### P2.5 `launched_studios` 可残留已退出 Studio PID

现状：

- 实机验证时，helper `/status.claimed_tasks[].studio_pid` 和 `/status.launched_studios[]` 曾显示 PID `21632`。
- Windows 进程表里该 PID 已不存在。
- 同一时刻插件实例仍上报真实 Studio PID `19180`，Rojo / MCP 都连接正常。

影响：

- helper 的 active Studio 账本和插件事实不一致。
- 上层排障时会误以为 helper 当前控制的是一个已不存在的 Studio 进程。
- 后续状态合成如果读取 stale `launch_processes`，可能再次出现“账本显示 edit/stop，但真实插件实例是另一个 Studio”的错判。

处理：

- `launched_studios` 只能表示仍然存在的 helper-launched Studio 进程。
- 已退出进程必须从 active launch 账本移除；如需保留，只能进入 diagnostic last-exit 字段。
- `/status.claimed_tasks[].studio_pid` 不能优先使用已退出 launch process PID 覆盖 fresh plugin instance PID。
- `place_statuses[].studio_pids` 不能混入不存在的 PID。
- 无法证明 PID 仍存在时，返回明确诊断 reason，不猜。

### P3：runtime-log 链路

#### P3.1 runtime-log forward 仍同步等待远端 HTTP

现状：

- Roblox runtime 打 helper。
- helper 再同步等远端 runtime-log HTTP 返回。
- helper HTTP client timeout 是 20 秒。

影响：

- 没彻底解决 Studio shutdown 卡住问题。
- 远端慢或断时，Roblox runtime 仍会被 helper 请求拖住。

处理：

- Roblox -> helper：快速 ack。
- helper -> Linux runtime-log：后台发送。
- 本轮不引入复杂本地缓存重试；沿用以前语义，失败记日志和状态，不阻塞 Roblox runtime。

#### P3.2 runtime-log 模块复用 Rojo 命名 helper

现状：

- `runtime_log_forward.rs` 使用 `rojo_forward_target_path()`。

影响：

- 行为没错，但模块概念不干净。

处理：

- 后续整理时改为通用 `forward_target_path()`。

### P4：日志、插件副作用和测试

#### P4.1 Studio log 读取仍按最新 log 文件

现状：

- 读取 Studio log 时按最新 `_Studio_` log 文件选择。

影响：

- 多 Studio、多日志文件时可能读错。
- 影响 launch/stop 诊断。

处理：

- 绑定当前 Studio pid / helper-launched process / plugin instance。
- 无法确定时返回明确 reason，不猜。

#### P4.2 插件 `getStudioMode()` 有副作用

现状：

- 查询状态时会读日志并修改 tracker。

影响：

- 查询动作污染状态机。

处理：

- 拆成纯查询和状态迁移。
- live probe 只返回事实，不修改控制状态。

#### P4.3 测试集中在主文件，并保护旧行为

现状：

- 大量测试仍在 `studio_helper.rs` 主测试模块。
- 部分测试保护自动 restart、heartbeat 丢失拒绝 stop 等旧设计。

影响：

- 继续拆模块时难以定位问题。
- 旧测试会阻碍新职责边界落地。

处理：

- 随模块拆分迁移测试。
- 对旧行为测试改成新设计的断言。

### P5：低优先级小问题

#### P5.1 unregister 清 remote error 用错 key

现状：

- unregister 时用 `place_id` 删除 `last_remote_errors`。
- 实际 remote error key 是 `task:{task_id}`。

影响：

- 状态里可能残留旧 remote error。

处理：

- 改为按 task connection key 清理。

#### P5.2 `config.rs` 名不副实

现状：

- `HelperConfig` 包含 token mutex、HTTP client、hub notify。

影响：

- 它不是纯 config，更像 runtime context。

处理：

- 后续改名或拆成 `args/auth/runtime_context`。

## 修改顺序

顺序按依赖关系排，不按问题编号排。原则是：先统一状态事实，再改 stop，再改 launch/restart；runtime-log 和结构治理不得混入 stop/play 语义提交。

### Phase 0：基线收口与独立小修

目标：

- 当前拆分提交可追踪、可构建、可继续开发。
- 只处理不影响 stop/play 语义的独立问题。

范围：

- 已完成：提交 `8f2ea89 Refactor Studio helper modules`，新拆出的 Rust / Luau / Python 模块已跟踪。
- 保留验证：`cargo fmt -- --check`、`cargo clippy -- -D warnings`、`cargo test`、release build、插件构建。
- 可单独修 `summarize_error` UTF-8 截断，并加中文长错误单测。
- Linux / CI clippy 由 CI 或 Linux 侧验证，Windows 客户端不伪造结论。

不做：

- 不改 stop/play。
- 不改 runtime-log 语义。
- 不改 launch restart。

完成标准：

- 代码提交不漏文件。
- Windows 本地基础验证通过。
- `summarize_error` 如修复，必须是独立提交。

### Phase 1：建立权威 `studio_session_state`

目标：

- 先得到唯一可用于 gate 的 `studio_session_state`。
- 旧 `studio_mode / studio_control_state / studio_transition_phase / edit_runtime_state` 降级为诊断字段。

范围：

- 定义状态：
  - `stop`
  - `starting_play`
  - `play`
  - `stopping`
  - `none_response`
  - `none_connected`
- helper 保存 edit/runtime/control 原始事实，并在一个明确函数里合成状态。
- helper `/status`、hub heartbeat、MCP status 暴露 `studio_session_state`。
- helper `/status`、hub heartbeat、MCP status 同时暴露 `last_known_session_state` 和 `last_session_error_reason`，但它们只做诊断。
- hub 只透传，不推导。
- clockp MCP 只做快失败，不复制状态机。
- 插件查询状态不能有副作用；`getStudioMode()` 的日志推断只能保留为诊断，不能驱动 gate。
- 多实例合成不再按最新时间盲选。

必须一并消掉的问题：

- P1.3 多实例 snapshot 选错。
- P2.3 helper 缓存状态替代 live probe。
- P2.4 session state 输出模型不清。
- P2.5 `launched_studios` 残留已退出 Studio PID。
- P4.2 插件状态查询污染状态机。

完成标准：

- edit + play 同时存在时，状态为 `play`。
- 已登记 stop request 时，状态为 `stopping`。
- 无插件为 `none_connected`，插件存在但不可确认为 `none_response`。
- 只有 `studio_session_state` 参与 gate；`last_known_session_state` 不能参与 gate。
- 查询状态不会改变 tracker / 控制状态。
- 所有有副作用动作到 helper 后都重新计算当时状态。
- `/status` 里的 active Studio PID 必须来自仍存在的进程或 fresh plugin instance；已退出 PID 只能作为诊断历史，不能继续出现在 active `launched_studios` / `place_statuses[].studio_pids`。

### Phase 2：重做 stop 单步语义与 stop 对账

目标：

- `start_stop_play(stop)` 成为清晰的 stop-only 原语。
- stop 不受 helper-launched / ownership / stale heartbeat 权限限制。
- stop 每一跳可对账。

依赖：

- Phase 1 的 `studio_session_state` 已可用。

范围：

- `state=play`：登记 `stop_request_id`，进入 `stopping`，等待 `stop`。
- `state=stop`：返回 already stopped 成功。
- `state=stopping`：失败 `studio_stop_in_progress`，不递增 stop id。
- `state=starting_play`：失败 `studio_starting_play`。
- `state=none_response`：失败 `studio_plugin_no_response`。
- `state=none_connected`：失败 `studio_plugin_not_connected`。
- `/status` 暴露 stop request / runtime poll / EndTest result / late response 字段。
- late control response 可以收敛状态，但不伪造原请求成功。
- stop request 超时或失败后必须 terminal，不再 active，不再让状态永久保持 `stopping`。

必须一并消掉的问题：

- P1.2 `uncontrolled_play_session` 作为 stop 权限拒绝。
- P2.2 stop 链路字段不足。
- P1.4 late plugin response 被忽略。

完成标准：

- play 状态可 stop，不要求 helper-launched。
- actuator 不可用时 reason 明确。
- stop 超时返回非 0 reason，不继续做后续动作。
- `/status` 能对上 stop intent、poll、result。
- stop timeout / failed 后，下一次状态由当前事实重新合成，失败只通过 reason 和诊断字段表达。

### Phase 3：协同切换 launch/restart 职责

目标：

- helper 的 `LaunchStudioSession` 只负责单步 launch。
- 默认 restart 只在 Linux 高层 `launch-game.py`。

依赖：

- Phase 1 的状态可用。
- Phase 2 的 stop 语义和对账可用。
- Linux 侧同批准备好 `launch-game.py` 默认 restart 编排。

范围：

- Windows helper 删除运行路径：
  - `stop_running_session_before_launch_if_needed(...)`
  - `merge_launch_response_with_helper_restart(...)`
- `LaunchStudioSession` 分流：
  - `stop`：允许 launch。
  - `play`：失败 `studio_already_playing`。
  - `starting_play`：失败 `launch_already_in_progress`。
  - `stopping`：失败 `studio_stop_in_progress`。
  - `none_response`：失败 `studio_plugin_no_response`。
  - `none_connected`：失败 `studio_plugin_not_connected`。
- Linux `launch-game.py` 默认 `restart=true`：
  - 只在 `play` 时显式 stop。
  - 只在 `stop` 时显式 launch。
  - 其他状态直接失败。
- 更新 MCP tool description，去掉“launch 会先 stop”的暗示。

必须一并消掉的问题：

- P1.1 helper 内部隐式 stop 再 launch。
- 旧测试保护自动 restart。
- Linux 高层脚本依赖 helper 隐式 restart。

完成标准：

- helper 内部没有 stop-before-launch 运行路径。
- play 状态直接 low-level launch 必然失败且不 stop。
- `launch-game.py` 在 play 且 restart=true 时显式 stop -> 确认 stop -> launch。
- `launch-game.py --restart false` 在 play 时失败且不 stop。

Phase 3 Windows 侧实施记录：

- `LaunchStudioSession` 分支只做 mode 校验、`studio_session_state` 校验和插件转发。
- helper 已删除内部 stop-before-launch 路径，不能再在 low-level launch 内登记 stop request。
- `studio_session_state=play` 时 low-level launch 返回 `studio_already_playing`，由上层显式调用 `start_stop_play(stop)` 后再 launch。
- `starting_play / stopping / none_response / none_connected` 都直接返回单一 reason，不做隐式补救。
- 插件仍保留本地“非 stop 不启动”的保护；返回 JSON 里 `restart_applied=false` 仅表示本次 low-level launch 没做 restart。
- 插件在 launch 时创建的一次性 `MCPStudioSessionControl` 必须由插件生命周期清理：状态轮询确认 `stop` 且没有 pending start 后，删除带 clockp marker 的控制脚本。重复 stop 不再承担清理职责。
- 本机没有 `roblox-dev-infra` 仓库，Linux `launch-game.py` 同步修改未在本提交内完成；上线时必须同批切换，否则默认 restart 会在 play 状态收到 `studio_already_playing`。

Phase 3 Windows 侧验证：

- `cargo test --bin studio_helper`
- `cargo test --bin rbx-studio-mcp`
- `py -3 -m unittest util.test_plugin_session_control_cleanup util.test_studio_stop_timeout_contract`
- `py -3 util/local_state_sync_probe.py`
- 本地 probe 覆盖真实 Studio：stop -> launch、play 中重复 low-level launch 被拒且状态保持 play、stop、repeat stop、`MCPStudioSessionControl` 清理为 true。

### Phase 4：runtime-log 快速 ack + 后台转发

目标：

- Roblox runtime 不再因为远端 runtime-log 慢或断而卡住 Studio shutdown。

依赖：

- 不依赖 Phase 3，但不能和 Phase 2 / Phase 3 混在同一提交。
- 最好在 stop/play 主线稳定后做，避免测试时混淆 stop 问题和日志问题。

范围：

- Roblox runtime -> helper：helper 接收后快速 ack。
- helper -> Linux runtime-log：后台发送。
- 失败写 helper 日志和 status reason；MCP status / preflight / wait marker 必须把该 reason 继续返回给上层。
- 不引入持久缓存、不引入复杂重试系统。
- 如果实现使用内存队列，队列必须有明确上限；无法接收时 Roblox 请求快速失败并返回 reason，不能先 ack 再静默丢弃。
- 明确语义：Roblox 侧成功只代表 helper 已接收，不代表 Linux artifact 已落盘。

必须一并消掉的问题：

- P3.1 runtime-log forward 同步等待远端 HTTP。

完成标准：

- 远端 runtime-log 不可用时，Roblox 侧请求仍快速返回。
- helper status 能看到最近 runtime-log forward 错误。
- MCP status / preflight / wait marker 能把 runtime-log forward reason 打印给 LLM。
- shutdown 不再因 runtime-log 慢卡 20s/60s。

### Phase 5：结构治理，不改行为

目标：

- 在 stop/play 语义稳定后，再把 helper 大文件继续拆清楚。

范围：

- `plugin_registry.rs`：注册、注销、实例选择。
- `studio_session.rs`：session state / live probe / 状态合成。
- `studio_control.rs`：launch / stop 单步控制。
- `plugin_queue.rs`：工具派发、pending、late response。
- `hub_client.rs`
- `remote_ws.rs`
- `rojo_forward.rs`
- `official_adapter.rs`
- `screenshot.rs`
- `studio_log.rs`
- `runtime_log_forward.rs` 内的 Rojo 命名改成通用 forward 命名。
- 测试随模块迁移。

必须一并消掉的问题：

- P2.1 `PluginInstance` 职责过宽。
- P4.3 测试集中在主文件，并保护旧行为。
- P5.2 `config.rs` 名不副实。
- P3.2 runtime-log 模块复用 Rojo 命名。

完成标准：

- 主文件只保留启动、路由拼装、模块 wiring。
- 模块测试跟随模块。
- 单文件原则上低于 500 行，复杂模块目标低于 1000 行；这是治理目标，不阻塞前面行为修复。

### Phase 6：Studio log 绑定与诊断收尾

目标：

- Studio log 只做诊断，不做状态事实。
- 读取日志时尽量绑定正确 Studio pid / instance / launch。

范围：

- `read_studio_log` 绑定 helper-launched pid 或当前插件实例可证明的 Studio。
- 多 Studio 或无法判定时返回明确 reason，不猜最新文件。
- 清掉旧状态字段参与决策的残留测试。
- P5.1 unregister 清 remote error 用错 key 可在本阶段或临近模块治理时修。

必须一并消掉的问题：

- P4.1 Studio log 读取按最新 log 文件。
- P5.1 unregister 清 remote error 用错 key。

完成标准：

- 多 Studio 时不读错日志；无法确认时明确失败。
- 日志不参与 `studio_session_state` 的事实判断。

## 最终推荐执行顺序

1. Phase 0：基线收口与独立小修。
2. Phase 1：建立权威 `studio_session_state`。
3. Phase 2：重做 stop 单步语义与 stop 对账。
4. Phase 3：协同切换 launch/restart 职责。
5. Phase 4：runtime-log 快速 ack + 后台转发。
6. Phase 5：结构治理，不改行为。
7. Phase 6：Studio log 绑定与诊断收尾。

不能跳过 Phase 1。没有权威状态，Phase 2 的 stop 会继续猜；没有 Phase 2，Phase 3 的默认 restart 没有可靠 stop 原语。

Phase 3 必须 Windows helper 和 Linux `launch-game.py` 协同切换。单独删除 helper 内部 restart 会短暂破坏默认 launch；单独改 Linux 高层则仍会被 helper 内部隐式 stop+launch 干扰。

## 本轮不应该做的事

- 不继续用补丁方式增加兜底 retry。
- 不让 helper 继续兼容旧 restart 路径。
- 不把 stop ownership 继续绑在 helper-launched session 上。
- 不让 Linux MCP 复制 helper 状态机。
- 不引入 runtime-log 持久缓存重试系统。
- 不在不清楚事实 owner 的情况下新增状态字段。
