# runtime stop actuator 恢复方案

## 状态

本 draft 已通过本轮 3x reviewer 终审。HTTP 前置条件、fail-fast 语义与已启用 HTTP 的 `start_play -> stop` 正向实机验证均已自洽。

- Reviewer A：PASS。`EndTest` 仍只存在于 play/run server runtime actuator；HTTP 未启用时会在进入 play 前失败。
- Reviewer B：PASS。HTTP 未启用时不创建 stop request、不进入 `starting` / `stopping_requested`；缺少 fresh runtime heartbeat 的 stop 语义仍是 fail fast。
- Reviewer C：PASS。HTTP 前置没有暴露 helper 私有接口，也没有绕过高层入口；已在 HTTP enabled place 上完成正向 `start_play -> stop` 本地实测。

如果实现偏离这些条件，本 draft 自动视为不通过。

2026-06-11 本地 44750 实测新增事实：

- helper 固定 `44750`、hub `44758`、task-server `44756`、Rojo `44757` 时，Studio 可以正常打开指定 place，Rojo initial sync 可以完成，插件与 task-server 可以注册成功。
- 当前测试 place `105986423068266` 的 `HttpService.HttpEnabled` 在 edit runtime 中读出为 `false`。
- Studio 插件线程不能写 `HttpService.HttpEnabled`，也不能调用 `SetHttpEnabled` / `GetHttpEnabled`；这些调用会因 Roblox capability 限制失败。
- Rojo 项目里的 `$properties.HttpEnabled=true` 不能作为启动时自动开启 HTTP 的可靠前提。
- 因此 runtime stop actuator 不能把“插件或 Rojo 会自动启用 HTTP”作为设计假设。HTTP 未启用时必须在进入 play 前明确失败，不能制造半启动、lost/starting 或无人消费的 stop request。

## 已确认事实

Studio 的 edit 插件运行时和 play/run server DataModel 不是同一个运行时。

`StudioTestService:EndTest` 只能在正在运行的 play/run server DataModel 里调用。edit 插件直接调用会失败，线上已经出现过：

```text
EndTest: can only be called from the server DataModel of a running Studio play session
```

因此，`start_stop_play(stop)` 的正确形状不是 edit 插件直接 stop，也不是把 stop 命令排队给 edit 插件，而是：

1. clockp MCP / helper 控制面接收唯一对外 stop 命令。
2. helper 在本机控制面登记 stop intent。
3. play/run server runtime 内部控制脚本读取该 intent。
4. server runtime 控制脚本调用 `StudioTestService:EndTest`。
5. helper 等待真实 stop 证据，包括 Studio stop log、runtime control heartbeat 消失或转入停止态、edit runtime 恢复并上报 stop/ready。

关键原因：Studio 处于 playing 时，edit 插件运行时可能挂起；把 stop request 交给 edit 插件会在最需要 stop 的时候变成无人消费的请求。

### `previous_test_in_progress` 不是普通 stop

还有一种独立故障：helper / plugin 可见快照已经是 `studio_mode=stop`、`studio_transition_phase=idle`、`edit_runtime_state=ready`，但下一次 `launch_studio_session(start_play|run_server)` 调用 `StudioTestService` 时，Roblox 仍返回 previous test still in progress。

这个状态只允许在 `launch_studio_session` 收到 StudioTestService 的明确 previous-test 错误时确认。它不是新的 `studio_mode`，不是新的 `studio_transition_phase`，也不是 `controlled running` 或 `uncontrolled_play_session`。

对外稳定错误码为 `previous_test_in_progress`。`dirty stop` 只能作为解释性描述，不能作为另一个对外 code。

该场景下不得提示调用 `start_stop_play(stop)` 作为恢复手段：既然 helper 快照已经是 stop，普通 stop 会被视为 no-op；而让 edit 插件直接调用 `StudioTestService:EndTest` 又违反 runtime 边界。安全恢复动作是重启 Studio 后重新 launch。helper 只在本机链路自身异常时才需要处理；这不是 hub 管理动作，也不包含 block-helper、force release 或修改 hub state。

错误返回必须保留原始 StudioTestService 错误和当时 helper snapshot，便于区分真实 previous-test 残留和普通 `mcp_not_ready`。

## 目标

- 保持对外启动唯一入口：`launch_studio_session`。
- 保持对外停止唯一入口：`start_stop_play(stop)` / `stop-game.py`，但该入口必须由 helper / MCP 控制面执行，不依赖 edit 插件在 playing 时接收命令。
- 让 helper 启动的 play/run 可以 stop。
- 让用户手动 Play 也可以 stop，前提是 play runtime 中实际存在内部控制脚本并已经向 helper 上报 fresh heartbeat。
- 没有 fresh heartbeat 时返回明确 `uncontrolled_play_session`，不能制造无人消费的 stop request。
- stop 失败时状态必须收敛到可解释的 error/lost，不能永久卡在 `stopping_requested`。

## 非目标

- 不恢复 `run_script_in_play_mode`。
- 不恢复 `StartStopPlay(start_play)` 或 `StartStopPlay(run_server)`。
- 不允许 `run_code` 调 Studio 控制 API。
- 不恢复隐藏 play/stop retry。
- 不把 `previous_test_in_progress` 伪装成普通 `mcp_not_ready`、普通 stop 或自动 retry。
- 不把 helper 私有 HTTP 入口写进 README、skill、bridge 文档或 LLM 可调用入口。
- 不把 `/v1/mcp/plugin/stop-request` 暴露为对外调试脚本入口。
- 不恢复简单 `/stop-ack` 作为完成语义；旧 ack 只能证明 runtime 看到了请求，不能证明 stop 成功。

## 设计原则

### 对外入口唯一

外部只允许两类控制动作：

- `launch_studio_session(task_id, mode=start_play|run_server)`
- `start_stop_play(task_id, mode=stop)`

`start_stop_play` 在 server schema、插件 dispatcher、类型定义和文档里都必须保持 stop-only。

### actuator 是内部执行器

`MCPStudioSessionControl` 是内部 server script，不是 MCP tool。它可以通过 helper 私有接口读取 stop intent，但不能出现在：

- clockp MCP `tools/list`
- 插件 `ToolDispatcher`
- `Types.ToolArgs`
- README 安装提示
- clock-p-platform skill / docs 的人工操作入口

### stop intent 由 helper 控制面登记

`start_stop_play(stop)` 的 tool schema 仍然对外存在，但实现路径必须绕过 edit 插件：

- clockp MCP 收到 `start_stop_play(stop)` 后，把请求交给 Windows helper 的 Studio control handler。
- helper 读取当前 task / instance 快照，只有在 play/run session 有 fresh runtime control heartbeat 时才登记 stop intent。
- helper 登记 stop intent 后，等待 server runtime actuator 消费该 intent 并让 Studio 回到 stop/edit。
- edit 插件只在 stop/edit ready 时参与 actuator 预安装、刷新、状态上报和失效清理；playing 期间不得作为 stop command 的必经消费者。

这个约束同样适用于 `launch_studio_session` 内部的 stop-before-relaunch：已有 play/run 会话没有 fresh runtime control heartbeat 时必须 fail fast 为 `uncontrolled_play_session`，不能把 stop 命令排给 edit 插件等待。

### 手动 Play 通过预安装脚本支持

为了让用户手动 Play 也能被 stop，edit 插件在 stop/edit ready 时必须确保 `MCPStudioSessionControl` 已安装在 edit DataModel 的 `ServerScriptService`。

当用户手动 Play 时，该脚本会随 DataModel 进入 play server runtime，并开始：

- 推断当前 runtime mode。
- 上报 `/v1/mcp/plugin/control-heartbeat`。
- 轮询 helper 私有 stop request。
- 在 server DataModel 内调用 `StudioTestService:EndTest`。

如果 helper 没有看到 fresh control heartbeat，说明当前 running session 没有可用 actuator。此时 stop 必须失败为 `uncontrolled_play_session`。

### HttpService / HTTP enabled 前置

`MCPStudioSessionControl` 依赖 play/run server DataModel 内的 `HttpService:RequestAsync` 访问 helper 私有接口。因此它只有在 Studio play/run runtime 允许 HTTP 请求时才可用。

硬规则：

- HTTP enabled 是 experience / place 级前置条件，不是插件、helper 或 Rojo 在 launch 时可以可靠修复的运行时状态。
- edit 插件在安装 runtime actuator 前只能读取 `HttpService.HttpEnabled` 并做硬门控；如果读到 false 或无法确认 true，`launch_studio_session(start_play|run_server)` 必须 fail fast，且不得调用 `ExecutePlayModeAsync` / `ExecuteRunModeAsync`。
- 失败信息必须直接说明 HTTP 未启用，并说明需要在 Roblox Game Settings / Experience Settings 中启用；不能提示 LLM 手工调用 helper 私有接口，也不能暗示 Rojo 会自动修复。
- actuator 只有在成功向 helper 上报 fresh `control-heartbeat` 后，才算可接管当前 play/run session。
- 如果 `HttpService:RequestAsync` 不可用、HTTP requests 未启用、请求 helper 失败或 JSON 解析失败，actuator 不得伪造 heartbeat，也不得让 helper 进入 `ready/running`。
- helper 没有 fresh heartbeat 时，`POST stop-request` 必须返回 `uncontrolled_play_session`，不得递增 `stop_request_id`。
- edit 插件可以把 HTTP/actuator 初始化失败写入诊断日志，但不能把它当作已安装且可控的 actuator。
- 实机测试必须覆盖 HTTP 不可用或 helper 请求失败时的语义：launch 应在 play 前 fail fast，或 running session 应保持 lost/uncontrolled；stop fail fast，不能进入永久 `stopping_requested`。

该前置不改变对外入口：LLM / bridge 仍只能走 `start_stop_play(stop)` / `stop-game.py`，不能直接调用 helper 私有 HTTP 口。

### 预安装脚本非污染生命周期

`MCPStudioSessionControl` 是平台临时控制脚本，不是用户工程资产。预安装到 `ServerScriptService` 时必须满足非污染规则：

- 使用唯一固定名称 `MCPStudioSessionControl`，只管理平台自己创建的同名脚本；如果存在非平台同名对象，必须明确失败或先安全隔离，不能覆盖用户资产。
- 脚本内容必须带平台标记，例如 attribute / 注释签名，用于判断对象是否由插件创建。
- helper URL、`instance_id`、`task_id` 或脚本版本变化时，edit 插件只能替换带平台标记的旧脚本。
- Studio 退出插件、helper 断开、task release、插件 unload 或当前预安装脚本不再对应有效 helper/task 时，应尽力清理平台脚本；清理失败只能作为诊断错误，不得假装 stop 成功。正常 stop/edit ready 状态仍需要保留或刷新当前 task 的预安装脚本，以支持用户后续手动 Play。
- 该脚本必须包含 `RunService:IsStudio()` / server runtime guard，避免发布后在真实线上 server 中执行 helper 请求。
- 文档和安装提示不得要求用户手工创建、保存或发布该脚本；它也不得出现在 clockp MCP `tools/list`、插件 dispatcher 或 LLM 操作入口里。
- 与 Rojo / syncback 的交互必须按“平台临时对象”处理：不能要求用户把它纳入工程源文件，也不能让它成为发布内容或代码评审里的业务文件。

## 协议边界

### POST `/v1/mcp/plugin/stop-request`

用途：helper 内部控制面登记一次 stop intent。该接口是 helper 私有接口，不是对外调试入口；调用方应是 helper 的 Studio control handler，而不是 playing 时可能挂起的 edit 插件。

必须满足：

- instance 存在。
- 当前 helper 快照是 `start_play` 或 `run_server`。
- 当前 instance 有 fresh control heartbeat。
- 当前不是 `stopping_requested`。

成功时：

- `stop_request_id += 1`
- `studio_control_state = "stopping"`
- `studio_transition_phase = "stopping_requested"`
- 返回 `{ stop_requested: true, stop_request_id }`

失败时：

- 没有 fresh heartbeat：返回 `uncontrolled_play_session`，不得递增 `stop_request_id`。
- 已在 stop：返回 `studio_stop_in_progress`，不得递增 `stop_request_id`。

### GET `/v1/mcp/plugin/stop-request`

用途：play/run server runtime 内部控制脚本读取 stop intent。

这是私有 runtime command channel，不是对外入口。

查询参数：

- `instance_id`
- `after_id`

返回规则：

- 只有当前 `studio_transition_phase == "stopping_requested"` 且 `stop_request_id > after_id` 时，才返回 `stop_requested=true`。
- 如果 helper 已回到 `idle`、进入 `error/lost`，或当前不是 stopping phase，即使 `stop_request_id > after_id`，也必须返回 `stop_requested=false`。
- 这样可以避免旧 stop request 被新 runtime 重放，导致新 session 刚启动就被杀掉。

### 不恢复 `/stop-ack`

最小方案不恢复简单 `/v1/mcp/plugin/stop-ack`。

原因：

- ack 只能说明 runtime 看到了 stop request。
- ack 不能证明 `EndTest` 成功。
- 真正完成条件仍然是 Studio stop log / edit runtime status 回到 stop/idle。

如果后续确实需要快速失败信号，应设计为 stop result/error，而不是简单 ack。

## 插件行为

### edit runtime

edit 插件负责：

- 注册 helper instance。
- 在 stop/edit ready 时确保 `MCPStudioSessionControl` 安装在 `ServerScriptService`。
- helper URL、instance_id、task_id 变化时重装控制脚本。
- 在 stop/edit ready 时上报 `studio_mode=stop` 与 `edit_runtime_state=ready`。
- stop 成功后清理 tracker，并保留或刷新当前 task 的平台预安装脚本；只有 helper/task 失效、插件卸载或脚本不再对应当前 task 时才删除该脚本。

edit 插件禁止：

- 直接调用 `StudioTestService:EndTest`。
- 在 play/run 状态下作为 `start_stop_play(stop)` 的命令接收者或 stop intent 登记者。
- 在 `start_stop_play` 里启动 play/run。
- 在失败后隐藏 retry play/stop。

### helper / MCP control runtime

helper / MCP 控制面负责：

- 接收 `start_stop_play(stop)` 和 `launch_studio_session` 内部 stop-before-relaunch。
- 只在 fresh runtime control heartbeat 存在时登记 stop intent。
- 登记后把状态推进到 `stopping_requested / stopping`。
- 等待 Studio stop log、runtime control heartbeat 停止或切换、edit runtime 恢复 ready。
- stop 超时时把状态收敛到 error/lost，并返回明确错误；不得永久卡在 `stopping_requested`。

### play/run server runtime

`MCPStudioSessionControl` 负责：

- 在 server runtime 内推断当前 mode，不能依赖 launch 时固定传入的 target mode。
- 定期向 helper 上报 `control-heartbeat`。
- 轮询 GET stop-request。
- 收到新 stop request 后，在 server DataModel 内调用 `StudioTestService:EndTest({ stopped_by = "mcp_session_control", stop_request_id = id })`。
- 使用 `after_id` 防止处理旧请求。

## 状态机

正常 stop：

```text
running / ready
  -> start_stop_play(stop)
  -> helper control handler validates fresh heartbeat
  -> helper records stop intent
  -> stopping_requested / stopping
  -> runtime actuator calls EndTest
  -> Studio stop log observed
  -> edit plugin status update stop
  -> idle / none
```

没有 actuator：

```text
start_play|run_server / lost
  -> start_stop_play(stop)
  -> uncontrolled_play_session
  -> state unchanged
```

重复 stop：

```text
stopping_requested / stopping
  -> start_stop_play(stop)
  -> studio_stop_in_progress
  -> state unchanged
```

stop 超时：

```text
stopping_requested / stopping
  -> edit plugin wait stop log timeout
  -> helper records control error/lost
  -> later stop may be attempted only after fresh heartbeat or explicit upstream decision
```

## 测试要求

### 静态测试

- `RunScriptInPlayMode.luau` 不存在。
- `run_script_in_play_mode` 不出现在 server schema、dispatcher、types、README、install 输出。
- `start_stop_play` schema 只允许 stop。
- `RunCode` 继续拒绝 `StudioTestService`、`ExecutePlayModeAsync`、`ExecuteRunModeAsync`、`EndTest`。
- `StudioSessionControl.stop()` 不包含 `StudioTestService:EndTest`。
- `StudioSessionControl.handleStartStopPlay()` 不登记 stop intent，不等待 stop log；playing stop 不依赖 edit 插件 long-poll。
- MCP server 的 `launch_studio_session` / `start_stop_play(stop)` control preflight 在 running 且非 fresh ready heartbeat 时必须 fail fast 为 `uncontrolled_play_session`。
- `previous_test_in_progress` 文案必须明确要求重启 Studio，不得建议 `start_stop_play(stop)`。
- helper / MCP control handler 直接登记 stop intent，并在 fresh heartbeat 缺失时 fail fast。
- `installSessionControlScript()` 生成的 server script 包含内部 stop actuator。
- 生成的 server script 不包含启动 API：`ExecutePlayModeAsync` / `ExecuteRunModeAsync`。
- 生成的 server script 包含 Studio/server guard，且 helper HTTP 失败时不会上报 ready heartbeat。
- edit 插件只替换带平台标记的 `MCPStudioSessionControl`，不得覆盖用户同名对象。
- plugin unload / helper disconnect / task release 路径会尽力清理平台预安装脚本。
- helper 只在 fresh heartbeat 下接受 stop POST。
- helper GET stop-request 只在 `stopping_requested` 下返回 active stop。
- helper 不暴露 `/stop-ack`。

### Rust 单元测试

- lost/uncontrolled running session 的 stop POST 返回 `uncontrolled_play_session`，且不递增 `stop_request_id`。
- MCP server control preflight 对 lost/uncontrolled running session 返回 `uncontrolled_play_session`，不把请求继续派发给 helper。
- bridge 错误映射必须把 StudioTestService previous-test 错误归类为 `previous_test_in_progress` / `restart_studio` / `retryable=false`，并且该判断必须位于普通 `mcp_not_ready` 兜底之前。
- 普通 MCP / Studio mode 失败不得被误判为 `previous_test_in_progress`。
- fresh heartbeat running session 的 stop POST 成功进入 `stopping_requested`。
- duplicate stop 返回 `studio_stop_in_progress`。
- GET stop-request 在 idle/error/lost 时不重放旧 stop。
- status update `stop` 收敛为 `idle / none`。
- failed control tool response 能把 `stopping_requested` 收敛到 error/lost，不能永久阻塞。

### Studio 实机测试

必须在 Windows 本机验证：

- helper 启动后，Studio 处于 stop/edit，`edit_runtime_state=ready`。
- HTTP 未启用的 place 上，`launch_studio_session(start_play)` 必须在进入 play 前明确失败，helper / hub / task-server 快照保持 `stop / none / idle`，不得进入 `starting` 或 `stopping_requested`。
- `launch_studio_session(start_play)` 成功，helper 看到 `play_control / ready / running`。
- `start_stop_play(stop)` 成功，回到 `stop / idle / edit_runtime_ready`。
- 用户手动 Play 后，helper 能看到 fresh heartbeat；随后 `start_stop_play(stop)` 成功。
- 如果构造无 heartbeat running session，stop 明确失败为 `uncontrolled_play_session`，不得进入永久 `stopping_requested`。
- stop 超时场景不会触发自动 play/stop retry。

## TODO

- [x] 恢复内部 runtime stop actuator，但不恢复任何 MCP tool。
- [x] 让 edit ready 时预安装 `MCPStudioSessionControl`。
- [x] 让 runtime 控制脚本自行推断 mode 并上报 heartbeat。
- [x] 恢复 GET stop-request 私有读取接口。
- [x] POST stop-request 恢复 fresh heartbeat 门槛。
- [x] GET stop-request 增加 phase 限制，禁止旧 stop 重放。
- [x] 删除 edit 插件直接 `EndTest`。
- [x] 从 edit 插件 `StartStopPlay` 路径移除 stop intent 登记与 stop 等待。
- [x] helper / MCP control handler 直接登记 stop intent，并等待 stop/idle/edit ready。
- [x] 增加 control tool 失败后的 helper 状态收敛。
- [x] 更新静态测试和 Rust 测试。
- [x] 完成 Studio 本地端口 44750 的 stop/edit 链路实机测试。
- [x] 验证 HTTP 未启用 place 的 launch fail-fast 语义。
- [x] 使用已启用 HTTP 的 place 完成 start_play -> stop 成功链路实机测试。

## 通过检查清单

- [x] Reviewer A 同意。
- [x] Reviewer B 同意。
- [x] Reviewer C 同意。
- [x] 明确禁止恢复 `run_script_in_play_mode`。
- [x] 明确禁止恢复 `start_stop_play(start_play/run_server)`。
- [x] 明确 `EndTest` 只能在 play/run server runtime 内执行。
- [x] 明确手动 Play 的 stop 依赖预安装 actuator 和 fresh heartbeat。
- [x] 明确没有 heartbeat 时返回 `uncontrolled_play_session`，不创建 stop request。
