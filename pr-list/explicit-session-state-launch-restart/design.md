# 显式 Studio 会话状态与 launch restart 设计

## 状态

本 draft 已按当前裁决整理，待实现前作为边界约束使用。

- Reviewer A（插件运行时）：PASS。状态来源按 edit runtime 与 play runtime 拆开，插件必须提供无副作用 live probe；不允许 edit 插件靠日志推断覆盖 play 真相。
- Reviewer B（Windows helper）：PASS。helper 不再把 `LaunchStudioSession` 做成 stop+start 复合操作；每次有副作用动作前必须主动 query 插件 live state，并只按本次 query 结果分流。
- Reviewer C（Linux / launch-game.py）：PASS。自动 restart 放在 `launch-game.py` 高层脚本，底层 MCP 原语保持单步、可对账。

如果实现中恢复了 helper 内部自动 stop+launch、把 `stopping` 中的重复 stop 吞成成功、helper 缓存/猜测 play-stop 状态、或继续用旧 `studio_mode` 做主 gate，本 draft 视为不通过。

## 背景

当前链路里有两个混在一起的问题：

1. helper 的 `LaunchStudioSession` 在发现已有 running session 时，会内部调用 stop，再转发 launch 给插件。
2. 状态判断依赖 `studio_mode / studio_control_state / studio_transition_phase / edit_runtime_state` 组合，且 edit 插件状态上报会读取日志并可能改写状态。

这导致一次高层 launch 失败后，上游重试可能触发隐式 stop/start 循环；同时 server、helper、Studio 输出看到的状态不容易对账。

新的设计把职责拆开：

- MCP 原语只做单步动作。
- `launch-game.py` 作为高层脚本负责默认 restart 编排。
- 状态只使用一次 helper 主动 query 插件得到的 live 枚举做 gate。

## 目标

- 对外暴露唯一权威状态：`studio_session_state`，但它必须来自 helper 当前这次 live query，不得来自 helper 缓存或推断。
- `launch_studio_session` 只允许 `stop -> starting_play -> play`。
- `start_stop_play(stop)` 只允许 `play -> stopping -> stop`；如果本次 live query 已经是稳定 `stop`，则返回无副作用成功 `already_stopped`。
- `launch-game.py` 默认 `restart=true`，只在当前为 `play` 时先显式 stop，再显式 launch。
- 所有失败返回非 0，并携带机器可读 `reason`，供 LLM 下一步判断。
- 去掉 helper 内部自动 stop+launch，不保留兼容分支。

## 非目标

- 不实现自动重启 Studio。
- 不在 helper 里为 launch 做隐式 stop。
- 不把 `stopping` 中的重复 stop 当成功。
- 不用旧 `studio_mode` gate 继续做主判断。
- 不用旧 Studio log 作为最终 play/stop 真相。
- 不让 helper 用 stop request、heartbeat 过期、日志或上一次状态自行推导 `edit/play/starting_play/stopping`。
- 不保留迁移兼容语义；Linux 侧同步改。

## 权威状态

字段名：

```text
studio_session_state:
  stop
  starting_play
  play
  stopping
  none_response
  none_connected
```

含义：

- `stop`：本次 live query 命中的插件明确回答稳定停止态，edit 插件可响应，允许 start。
- `starting_play`：本次 live query 命中的插件明确回答已经进入启动链路，但还没有拿到可靠 play runtime 执行信号。
- `play`：本次 live query 命中的插件明确回答 play runtime 已可控。
- `stopping`：本次 live query 命中的插件明确回答已经发出 stop，尚未收敛回稳定 `stop`。
- `none_response`：helper 有插件实例记录，但本次主动 query 没有得到有效响应。
- `none_connected`：helper 当前没有可用插件实例。

`studio_mode / studio_control_state / studio_transition_phase / edit_runtime_state` 可以暂时保留为诊断字段，但不得再作为 gate 的权威输入。

## 状态来源

权威输入不是 helper 缓存字段，而是 helper 每次动作前主动向插件发起的 live query。

插件 live query 响应：

```text
plugin_live_state:
  instance_id
  task_id
  state = stop | starting_play | play | stopping
  transition_id?
  runtime_mode?
  reason?
```

插件内部按自己所在 runtime 回答：

```text
if edit runtime:
    if launch 已开始但 play 控制点未 ready:
        return starting_play
    return stop

if play/runtime:
    if stop 已发出且还没回到 edit:
        return stopping
    return play
```

helper 自己只能生成连接层失败状态：

```text
if no selected plugin instance:
    return none_connected

if selected plugin instance does not answer this live query:
    return none_response
```

helper 可以保存以下诊断事实，但这些事实不能直接作为 play/stop gate：

```text
last_plugin_heartbeat_at
last_live_query_id
last_live_query_started_at
last_live_query_error
active_stop_request_id
stop_request_recorded_at
last_stop_result
```

`none_response` 与 `none_connected` 只能由 helper 根据本次 query 结果生成，插件不得上报。

## helper live query 裁决

helper 不再存一个可被任意来源覆盖的最终 `studio_mode`，也不维护可直接参与 gate 的 play/stop 缓存。helper 保存诊断事实，但有副作用动作的裁决必须先执行一次 live query。

职责边界：

- 只有 Windows helper 负责发起 live query，并把本次 query 结果作为本次动作的唯一 gate 输入。
- hub 只保存并透传 helper 最近一次上报的 live query 结果，不推导、不覆盖、不合成。
- clockp MCP 可以用 hub snapshot 做快失败，但不能把自己变成第二个状态机 owner。
- 所有有副作用命令到达 helper 后，helper 必须重新 query 插件 live state 并最终裁决，不能信任上游刚才查到的状态，也不能使用 helper 自己的旧快照代替。

伪代码：

```text
fn query_studio_session_state(task_id):
    if no selected instance:
        return none_connected

    response = send_live_state_probe(instance, timeout=LIVE_QUERY_TIMEOUT)
    if response.timeout or response.invalid:
        return none_response

    if response.task_id != task_id:
        return none_response

    if response.state in [stop, starting_play, play, stopping]:
        return response.state

    return none_response
```

约束：

- `play` 只能由本次 live query 的插件响应证明。
- `stop` 只能由本次 live query 的插件响应证明。
- `starting_play` 只能由本次 live query 的插件响应证明；不能由日志或 helper launch 记录推断生成。
- `stopping` 只能由本次 live query 的插件响应证明；不能仅因 helper 已记录 stop request 就合成。
- 多实例或 task 身份不唯一时，不合成成功态，返回明确错误。
- `/status` 可以展示最近一次 live query 结果和 age，但必须标明 `source=live_query_cache_for_observation`；动作裁决不得直接复用这个展示缓存。

## MCP 原语语义

### `launch_studio_session(mode)`

职责：只启动，不停止。

输入状态分流：

```text
state = stop:
    enter starting_play
    forward LaunchStudioSession to plugin
    wait play runtime heartbeat
    success -> play
    timeout -> non-zero / reason=launch_timeout

state = play:
    fail / reason=studio_already_playing

state = starting_play:
    fail / reason=launch_already_in_progress

state = stopping:
    fail / reason=studio_stop_in_progress

state = none_response:
    fail / reason=studio_plugin_no_response

state = none_connected:
    fail / reason=studio_plugin_not_connected
```

必须删除 helper 当前的内部自动 stop：

```text
stop_running_session_before_launch_if_needed(...)
merge_launch_response_with_helper_restart(...)
```

如果保留这些函数，也只能保留在测试删除前的历史上下文中；运行路径不得调用。

### `start_stop_play(stop)`

职责：只停止，不启动。

输入状态分流：

```text
state = play:
    create/send stop request to play runtime plugin
    wait stop
    success -> stop
    timeout -> non-zero / reason=stop_timeout

state = stop:
    success / reason=already_stopped

state = stopping:
    fail / reason=studio_stop_in_progress

state = starting_play:
    fail / reason=studio_starting_play

state = none_response:
    fail / reason=studio_plugin_no_response

state = none_connected:
    fail / reason=studio_plugin_not_connected
```

`stopping` 中的重复 stop 不应被吞成成功。稳定 `stop` 下的 stop 是无副作用查询式成功，返回 `already_stopped`；`stopping` 下再次 stop 说明上游没有遵守状态机，必须暴露出来。

## `launch-game.py` 高层编排

默认参数：

```text
restart = true
```

只有显式传 `--restart false` 时，才禁止自动 stop+launch。

伪代码：

```text
state = query_studio_session_state()

if state == play:
    if restart == false:
        fail(reason="studio_already_playing", exit=1)

    stop_result = call_start_stop_play_stop(timeout=STOP_TIMEOUT)
    if stop_result.failed:
        fail(reason=stop_result.reason, exit=1)

    state = query_studio_session_state()
    if state != stop:
        fail(reason="stop_did_not_reach_stable_stop", state=state, exit=1)

if state == stop:
    launch_result = call_launch_studio_session(mode, timeout=LAUNCH_TIMEOUT)
    if launch_result.failed:
        fail(reason=launch_result.reason, exit=1)
    wait_runtime_log_session_marker()
    success(exit=0)

fail(reason="invalid_state_for_launch", state=state, exit=1)
```

明确规则：

- 只有 `play` 状态里会调用 stop。
- 只有 `stop` 状态里会调用 start。
- `starting_play / stopping / none_response / none_connected` 全部直接失败。
- stop 超时直接失败，不继续 launch。
- launch 超时直接失败，不继续 stop。
- 失败必须输出 `reason`、`studio_session_state`、`task_id`，以及当前阶段名。

建议输出结构：

```json
{
  "success": false,
  "reason": "studio_stop_in_progress",
  "stage": "prelaunch_state_check",
  "task_id": "t_example",
  "studio_session_state": "stopping"
}
```

## 模块变动

### Windows helper

需要改：

- 新增 `query_studio_session_state(task_id)`：每次向当前选中的插件实例发起 live query。
- `/status`、task heartbeat、remote task status 暴露最近一次 live query 的 `studio_session_state`、`age_ms`、`source`、`last_query_error`。
- `LaunchStudioSession` 分支删除内部 stop-before-launch。
- `LaunchStudioSession` 分支执行前必须重新 live query；只有本次 query 返回 `stop` 才转发插件。
- `StartStopPlay` 分支执行前必须重新 live query；只有本次 query 返回 `play` 才下发 stop。
- 旧 `task_studio_mode_snapshot` gate 逻辑废弃或仅保留诊断输出。
- stop request id、last stop result、heartbeat age 只作为诊断与对账字段，不得单独合成 `stopping/play/stop`。

不得改成：

- helper 根据日志猜 `stop/play`。
- helper 根据上一次 heartbeat、上一次 live query、stop request 或超时状态猜 `stop/play/stopping/starting_play`。
- helper 在 launch 中自动 stop。
- helper 在 `stopping` 下把重复 stop 返回成功。

### Studio 插件

需要改：

- 增加无副作用 live query handler，返回 `stop | starting_play | play | stopping`。
- `refreshStatusFromHelper` 不再调用有副作用的 `currentStudioMode()` 上报全局 mode。
- 工具回包不再携带 `studio_mode` 作为 helper gate 输入。
- edit runtime live query 返回 `stop | starting_play` 局部事实。
- play runtime live query 返回 `play | stopping` 局部事实。
- `LaunchStudioSession` 进入启动链路时让 live query 能返回 `starting_play`。
- `get_studio_mode` 降级为诊断，或改名/改语义为读取 `studio_session_state`。

不得改成：

- edit 插件通过读 Studio log 推断并上报 `play`。
- edit 插件通过读 Studio log 推断并覆盖 `stop`。
- 插件反向读取 helper `/status` 来证明自己启动成功。

### clockp MCP / rbx_studio_server

需要改：

- `HubTaskRuntimeSnapshot` 增加并优先使用 `studio_session_state`。
- 删除旧 `require_stop_mode_snapshot` 作为主 gate。
- edit 类工具要求 `studio_session_state == stop`，并继续要求 service preflight ready。
- control 类工具：
  - `launch_studio_session` 要求 `stop`。
  - `start_stop_play(stop)` 允许进入 helper，但 helper 仍按状态分流；server 不应在 `stopping` 下自动重试。
- MCP 侧 gate 只负责快失败和减少无意义请求；最终状态裁决以 helper 执行动作时重新 live query 的结果为准。
- MCP 不得缓存或自行合成 `studio_session_state`，也不得把旧 `studio_mode` 和 transition 字段拼回一个本地状态机。
- tool description 改掉“launch 会先 stop”。
- `get_studio_mode` 工具描述改成诊断，不作为日常 launch 前置。

### hub / Linux task status

需要改：

- hub active task / helper task status 透传 `studio_session_state`。
- hub 是 task / helper 归属和状态账本，不是 Studio 状态机 owner；它不根据旧字段推导 `studio_session_state`。
- Linux 侧状态查询以 hub 为事实来源，不在 task-server 自己缓存 helper 状态，也不在 task-server 里二次合成 Studio 状态。
- 旧字段如果保留，只做展示诊断，不参与决策。

### `launch-game.py`

需要改：

- 增加 `--restart`，默认 true。
- 启动前并行只读 preflight 后读取 `studio_session_state`。
- 严格按上文伪代码分流。
- 所有失败非 0，并输出 JSON reason。
- 不直接调用 helper 私有 HTTP，只走 clockp MCP / 标准工具入口。

### `stop-game.py`

需要改：

- 读取 `studio_session_state`。
- `play` 时调用 `start_stop_play(stop)`。
- `stop` 时成功返回 already stopped。
- `starting_play / stopping / none_response / none_connected` 返回非 0 reason，不猜测恢复。

## 测试计划

### Rust 单元测试

- `LaunchStudioSession` 在 `play` 下返回 `studio_already_playing`，且不创建 stop request。
- `LaunchStudioSession` 在 `stopping` 下返回 `studio_stop_in_progress`，且不转发给插件。
- `LaunchStudioSession` 在 `stop` 下才转发给插件。
- `StartStopPlay(stop)` 在 `play` 下才下发 stop request。
- `StartStopPlay(stop)` 在 `stopping` 下返回 `studio_stop_in_progress`，且不递增 stop id。
- `StartStopPlay(stop)` 在 `starting_play` 下返回 `studio_starting_play`。
- 每个有副作用命令都会触发一次新的 live query；测试必须证明没有复用 `/status` 展示缓存。
- `task status` 只透传最近一次 live query 结果；不根据 stop request 或 heartbeat age 合成 `stopping/play/stop`。

### Luau / 静态测试

- `refreshStatusFromHelper` 不调用会改写 tracker 的 mode 推断函数。
- live query handler 不读 Studio log，不改 tracker，只有读当前插件内存状态。
- 工具 response 不再用 `studio_mode` 更新 helper gate 状态。
- `LaunchStudioSession` 开始后 live query 可返回 `starting_play`。
- play runtime live query 只返回 `play/stopping`。
- edit runtime live query 不返回 `play`。

### Python 测试

- `launch-game.py` 在 `stop` 下只调用 launch。
- `launch-game.py` 在 `play` 且 restart=true 时调用 stop，再确认 `stop`，再调用 launch。
- `launch-game.py` 在 `play` 且 `--restart false` 时非 0，reason=`studio_already_playing`。
- `launch-game.py` 在 `starting_play / stopping / none_response / none_connected` 下非 0，且不调用 stop 或 launch。
- stop 超时后非 0，且不调用 launch。
- launch 超时后非 0，且不调用 stop。

### Windows 本地实测

必须使用本地 hub/task-server/helper，不连 dev hub 做验证：

- stop -> launch 成功，状态到 `play`。
- play -> launch-game 默认 restart：先 stop 到 `stop`，再 launch 到 `play`。
- play -> launch-game `--restart false`：失败，不 stop。
- stopping -> launch-game：失败，不重复 stop。
- starting_play -> launch-game：失败，不 stop。
- helper/plugin 断开：`none_connected` 或 `none_response`，launch-game 失败并输出 reason。

## 完成标准

- helper 内部没有自动 stop-before-launch 运行路径。
- 有副作用动作前 helper 必须执行 live query；本次 live query 得到的 `studio_session_state` 是所有 gate 的唯一权威状态。
- launch-game 默认 restart 在高层脚本实现，且每一步可对账。
- 任一超时都返回非 0 与 reason，不继续后续有副作用动作。
- 旧状态字段不再参与决策。
- Rust / Python / Luau 相关测试覆盖上述分流。
- 本地 Windows 联调验证通过。

## 3x reviewer 终审

### Reviewer A：插件运行时边界

结论：PASS。

理由：

- edit runtime 与 play runtime 的事实来源被拆开，并通过无副作用 live query 暴露。
- edit 插件不再拥有上报 `play` 的权柄。
- 状态采样不得有副作用，避免读状态时改状态。
- 插件不再反查 helper `/status` 来证明 launch 成功。

需要实现时重点检查：

- `currentStudioMode()` 这类会读日志并改 tracker 的函数不能再出现在 status poll 主路径。
- 工具 response 里的旧 `studio_mode` 不能继续驱动 helper gate。

### Reviewer B：helper 状态与控制边界

结论：PASS。

理由：

- helper 每次动作前主动 query 插件 live state，不缓存、不猜测 play/stop 状态。
- `LaunchStudioSession` 删除内部 stop-before-launch，控制入口单一。
- `StartStopPlay(stop)` 对 `stopping` 返回错误，避免进行中的重复 stop 被吞掉；稳定 `stop` 返回 `already_stopped`。
- `none_response / none_connected` 由 helper 自己判定，插件不能伪造。

需要实现时重点检查：

- 多实例或 task 身份不唯一时不能选最新实例凑出成功态。
- stop request id 只能在本次 live query 返回 `play` 时递增。
- launch 在非 `stop` 状态下不能转发给插件。

### Reviewer C：Linux / LLM 操作入口

结论：PASS。

理由：

- 自动 restart 放到 `launch-game.py`，符合高层脚本职责。
- MCP 原语保持单步，日志和错误可以对账。
- 失败统一非 0 + reason，便于 LLM 决定下一步。
- 旧 gate 直接废弃，避免两套状态并存打架。

需要实现时重点检查：

- `launch-game.py` 必须只在 `play` 时调用 stop，只在 `stop` 时调用 start。
- `--restart false` 必须让 `play` 直接失败。
- `stopping / starting_play` 不能自动等待后继续操作，除非后续另行设计明确等待命令。

## 新 3x reviewer：进程职责复审

### Reviewer D：进程职责所有权

结论：PASS，补充约束已写入。

职责切分是贴业务的：

- Studio 插件贴近 Studio edit runtime，只负责注册、启动、注入 play actuator、上报 edit 侧事实。
- play runtime actuator 贴近 play server DataModel，只负责 heartbeat、消费 stop request、调用 `EndTest`。
- Windows helper 贴近本机 Studio 和插件连接，是唯一能发起插件 live query 并把连接失败归类为 `none_*` 的进程；它不拥有 play/stop 语义本身。
- hub 贴近 task / helper 归属，只做账本和透传，不做 Studio 状态推导。
- clockp MCP 贴近 LLM 工具协议，只做工具 schema、快失败、转发和错误归一，不拥有状态机。
- `launch-game.py` 贴近用户意图，默认 restart 放在这里合理；它不直接碰 helper 私有 HTTP。

风险点：

- 如果 MCP 继续把旧字段拼成自己的状态机，会和 helper 打架。
- 如果 hub 根据旧字段推导 `studio_session_state`，会变成第二个状态源。
- 如果 helper 承担 restart 策略，会重新变成“什么都管”的进程。

结论要求：

- helper 只管本机事实和单步动作裁决，不管高层 restart 策略。
- launch-game.py 只管高层编排，不绕过 MCP 直接打 helper 私有口。

### Reviewer E：业务贴合度

结论：PASS。

这套分层贴合业务动作：

- “我要启动游戏”是 `launch-game.py` 的业务动作，所以自动 stop+launch 放在脚本层。
- “启动 Studio play/run”是 MCP 原语，必须单步。
- “停止当前 play”是 MCP 原语，必须单步。
- “Studio 当前到底处于什么状态”是插件 live query 的回答；helper 只负责发起 query、归类无响应/未连接，并透传结果。
- “哪个 helper 拥有哪些 task”是 hub 的业务。

不贴业务的做法已被禁止：

- helper 内部做 restart。
- hub 推导 Studio 状态。
- MCP 缓存 helper 状态。
- edit 插件用日志猜 play 真相。

### Reviewer F：竞态与失败路径

结论：PASS，补充约束已写入。

必须承认 `launch-game.py` 查询状态和真正执行动作之间存在竞态。因此：

- `launch-game.py` 的状态查询只用于选择下一步。
- helper 在执行 `LaunchStudioSession` 或 `StartStopPlay(stop)` 时必须重新 query 插件 live state。
- 如果查询时是 `stop`，但执行 launch 时已经变成 `play/stopping/starting_play`，helper 必须拒绝。
- 如果查询时是 `play`，但执行 stop 时已经变成 `stop/stopping/none_*`，helper 必须按当时真实状态返回 reason。

这避免了上层脚本成为隐形状态机，也避免了状态查询结果被当作执行许可长期持有。

## 待裁决项

当前没有剩余需要裁决的核心语义。

实现过程中如果出现以下新增选择，必须重新讨论：

- 是否允许 `starting_play` 下 stop。
- 是否允许 `stopping` 下重复 stop 返回成功。
- 是否保留旧 `studio_mode` gate 兼容。
- 是否让 helper 根据日志补判 `stop/play`。

默认答案均为“不允许”。
