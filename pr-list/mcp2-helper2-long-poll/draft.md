# mcp2 / helper2 long-poll command channel draft

## Objective

This draft defines the first mcp2 <-> helper2 command channel.

Status note: Phases 1-8 are obsolete historical planning context. The current roblox-agent refactor mainline starts from Phase 9 and the hubless task-agent/helper2/mcp2 direction. Keep the Phase 1-8 notes only as background for why the command-channel shape exists; do not treat them as the active implementation plan.

The old hub, task-server, helper1, mcp1 / old Rust clockp MCP server path, and old runtime-log server are deprecated. They must not be used as the implementation target, compatibility fallback, or validation gate for Phase 9+ work.

Phase 19 之后，面向 bridge2 / LLM 的日志入口统一按 helper2 日志读取理解：CLI 命令名使用 `play-mode-logs`，不再把新操作面称为 `runtime-log`。

Development location note: do not create git worktrees for the Phase 9+ helper2/task-agent/mcp2 mainline. Implement and validate follow-up phases directly in the main repository directory.

The channel is intentionally minimal:

- mcp2 pulls one command at a time from helper2.
- helper2 returns a command only in response to a pull.
- mcp2 reports command results back with an explicit `command_id`.
- mcp2 never runs the command loop in the play client VM.
- a Studio mode lifecycle change invalidates all queued work in the current global queue. After the later task-scoped command channel exists, the same lifecycle change invalidates only that task's queued work.
- helper2 recovers a managed Studio if the mcp2 channel becomes stale.

The obsolete Phase 1-8 notes validate and support exactly one active helper-managed Studio lifecycle on the global mcp2 command channel. The temporary `placeid` debug endpoints select the desired target for local testing, but they do not make this channel safe for concurrent control of multiple Studio instances. Multi-Studio concurrency belongs to the current task-scoped command channel work in Phase 9 and later.

hub and task-server are outside the current PR scope. This draft includes the first formal Studio control command kinds needed to test edit/play lifecycle switching. Later hubless task-agent, Rojo, helper2 log reading, and public-exposure work is captured separately under Roadmap.

## Execution Model

mcp2 runs inside multiple possible Studio VM contexts. Each mcp2 instance classifies only its own VM; it does not claim global Studio state.

The supported execution modes are:

```text
edit
play_server
play_client
unknown
```

mcp2 computes these modes from `RunService`:

```text
edit:
  IsStudio=true
  IsEdit=true
  IsRunning=false

play_server:
  IsStudio=true
  IsEdit=false
  IsRunning=true
  IsServer=true
  IsClient=false

play_client:
  IsStudio=true
  IsEdit=false
  IsRunning=true
  IsServer=false
  IsClient=true

unknown:
  any other combination
```

mcp2 must print the mode as a dedicated log line:

```text
[MCP2 Plugin] mode=edit
```

Only `edit` and `play_server` enter the helper2 command loop. `play_client` and `unknown` return immediately after logging.

The current validation target is Studio's normal direct Play flow, where the user or command starts play from one Studio session and returns to edit from that same session. Start Server / Start Player split-session testing is intentionally out of scope for this draft. The mode names still use `play_server` and `play_client` because those are the RunService identities observed inside direct Play.

## Lifecycle Identity

Each mcp2 process creates a numeric `mode_seq` at startup. The value should combine millisecond-level startup time with a small per-process random suffix, for example `DateTime.now().UnixTimestampMillis * 1000 + random(0..999)`. This keeps the value sortable enough for debugging while avoiding same-millisecond collisions between Studio VMs.

`mode_seq` is the lifecycle boundary used by helper2. `mode` is carried for diagnostics and logs.

When Studio enters play, the play server VM has a new `mode_seq`. When Studio returns to edit, the edit VM might reuse its previous process and therefore keep its old `mode_seq`. helper2 must still treat a newly observed `mode_seq` from another VM as a lifecycle switch.

The protocol relies on Studio's VM ownership behavior: the edit VM is suspended while Studio is in play, and the play VM is independent from the edit VM. Because of that, helper2 does not try to merge or arbitrate simultaneous edit/play activity. A real pull from a different `mode_seq` means helper2 has observed a different active VM lifecycle and should switch to it. Pings never switch lifecycle.

If mcp2 catches an unexpected command execution error while processing a delivered command, it must report that command result with the current `mode_seq` first, so helper2 can complete the matching `waiting_response_command`. After mcp2 receives HTTP 200 for that result, it should advance `mode_seq` before the next pull. This creates a new helper2 lifecycle boundary for the still-running plugin loop without making the error result look stale. For play/stop Studio API failures that happen after the pre-action command result was already posted successfully, mcp2 logs the later failure and advances `mode_seq`; it must not try to mutate the already-completed command result.

## Protocol Endpoints

helper2 exposes:

```text
GET  /plugin/mcp2/pull_command
POST /plugin/mcp2/response_result
POST /debug/command/{placeid}
POST /debug/studio/play/{placeid}
POST /debug/studio/stop/{placeid}
GET  /debug/studio/mode/{placeid}
```

The old `GET /plugin/mcp2` endpoint is removed.

### Pull Command

`GET /plugin/mcp2/pull_command`

Required query parameters:

```text
mode=edit|play_server
mode_seq=<number>
```

Optional query parameters:

```text
only_ping=1
```

`only_ping=1` is used while mcp2 is executing a long command. It refreshes liveness for the same `mode_seq`, returns immediately, and never consumes a queued command.

The response is:

```json
{
  "type": "pong"
}
```

If a command is available, helper2 returns:

```json
{
  "type": "command",
  "command_id": 123,
  "kind": "...",
  "args": {}
}
```

If no command becomes available within the long-poll window, helper2 returns:

```json
{
  "type": "should_restart_pull",
  "reason": "no_command_timeout"
}
```

`should_restart_pull` is a normal protocol response. It is not an error and does not require `response_result`.

### Response Result

`POST /plugin/mcp2/response_result`

Request body:

```json
{
  "command_id": 123,
  "mode": "edit",
  "mode_seq": 1793097583123,
  "ok": true,
  "result": {}
}
```

`response_result` is mcp2's answer to a delivered command. It is not a request for helper2 to approve the command a second time.

helper2 returns HTTP 200 when the result payload has been received and parsed by the HTTP endpoint. HTTP 200 is only a transport receipt. It does not prove any Studio action succeeded. The response body is not part of the protocol and mcp2 must not make control decisions from it.

Before completing `waiting_response_command`, helper2 must validate that the request comes from the current helper-managed Studio connection, that the canonical Studio PID matches the active channel PID, that `mode_seq` matches the active lifecycle, and that `command_id` matches the current waiting response command. Invalid, stale, unmatched, or wrong-lifecycle results are still acknowledged with HTTP 200 and logged as ignored internally; they must not clear `waiting_response_command`, wake command waiters, or call completion callbacks.

mcp2 should retry posting an ordinary command result only when it does not receive HTTP 200 because of a connection failure, timeout, server close, or other transport-level failure. Once mcp2 receives HTTP 200, it treats the result post as complete and returns to the pull loop. mcp2 does not retry based on helper2 internal command matching decisions.

### Debug Command Enqueue

`POST /debug/command/{placeid}` is a temporary local development endpoint. It inserts a simple debug command into the single global queue for the requested place.

This endpoint exists only to test the helper2/mcp2 command channel before hub/task-server integration exists.

If helper2 is in `wait_init_mode`, this endpoint fails immediately because there is no initialized mcp2 execution lifecycle. The temporary mode query endpoint is the deliberate debug exception: it may wait briefly for an mcp2 VM and return `available=false` instead of failing as a normal command enqueue.

Supported debug command kinds:

```text
debug_echo        # default; echoes args through response_result
debug_stop_loop   # test-only; plugin acknowledges then stops its command loop
```

### Formal Studio Control Commands

helper2 also supports formal Studio control commands for local lifecycle testing:

```text
studio_play
studio_stop
studio_mode_query
```

These commands are not debug commands, even though the first local entrypoints live under `/debug/studio/...`.

Go code must represent queued commands with typed structs and typed command kinds. Formal commands must not be assembled as arbitrary `map[string]any` payloads inside helper2.

A queued command has this wire shape:

```json
{
  "type": "command",
  "command_id": 123,
  "kind": "studio_play",
  "args": {
    "place_id": "105986423068266",
    "mode": "start_play"
  }
}
```

`studio_play`:

- is a formal command kind.
- initially supports only `mode=start_play`.
- may be executed only by `edit` mode mcp2.
- returns failure from `play_server` mode. The failure message must clearly say it failed because Studio is already playing.
- returns failure from any other mode.

`studio_stop`:

- is a formal command kind.
- may be executed by `play_server` mode mcp2.
- returns success from `edit` mode as an idempotent `already_stopped` result.
- returns failure from any other mode.

`studio_mode_query`:

- is a formal command kind.
- may be executed by `edit` or `play_server` mode mcp2.
- returns the mcp2-observed mode, `mode_seq`, and RunService flags.
- exists to let upstream callers verify Studio mode after play/stop, because play/stop command success means mcp2 sent its pre-action command response, not that the final mode has already been observed.
- waits at most 5 seconds for an available mcp2 VM. If no VM is available during that short window, it should return an unknown/unavailable mode result instead of blocking for the full play/stop verification window.

For `studio_play` and `studio_stop`, mcp2 must send a pre-action `response_result` before invoking the Studio transition call. This pre-action result is mcp2's command response: it means mcp2 accepted the command and intends to request the Studio transition. It is not proof that the final Studio mode has changed.

After posting the pre-action result, mcp2 waits at most 3 seconds for the HTTP post attempt to finish. This wait is only a bounded transport step; it is not a second command confirmation. Whether the post returns HTTP 200 or times out, mcp2 then invokes the Studio transition call. If the post does not return HTTP 200, mcp2 logs that the pre-action result delivery is uncertain so upstream can rely on later mode/status verification.

After command-level success, upstream LLM or bridge code should verify final state through `studio_mode_query` or task status. If the pre-action result post times out or fails, upstream should treat the command response delivery as uncertain, but the Studio transition may still be requested by mcp2.

The debug play/stop entrypoints are enqueue-only and do not serialize around a pending Studio mode transition. If a stop command is consumed by edit mode during a play transition, the command may return `already_stopped`; this is acceptable for this phase because the endpoint result is not the authoritative final Studio state. The invariant for this phase is intentionally LLM-oriented rather than perfectly serialized: mixed or concurrent play/stop requests must leave an observable final state, must be understandable through mode/status queries, and must not poison the helper2 queue, mcp2 loop, or later fresh play/stop commands.

mcp2 must wrap Studio control calls such as `StudioTestService:ExecutePlayModeAsync({})` and `StudioTestService:EndTest({})` so Studio errors do not kill the plugin loop. For ordinary commands, such errors become command failures reported with the current `mode_seq`; after mcp2 receives HTTP 200 for the failure result, it bumps `mode_seq` before the next pull. For `studio_play` and `studio_stop`, the command-level result is the pre-action `response_result`; HTTP 200 is only transport receipt for that response. A later Studio API error should be logged and reflected through task status or later mode verification failure rather than mutating any already-completed command callback. After such an error, mcp2 must bump `mode_seq` and continue with the next pull using the new `mode_seq`.

### Command Completion and Cleanup Callbacks

Every formal command should have a command timeout and a completion callback. The callback usually resolves upstream state, for example an HTTP waiter or bridge-script response. helper2 must call the callback exactly once when the command reaches a terminal state.

The callback is not tied only to `response_result`. It must also run when helper2 clears a command from task-scoped queue or waiting-response state.

Callbacks are in-memory helper2 functions in the first implementation. The design does not persist command operations. This is acceptable because helper2 is expected to restart rarely, and helper2 restart should terminate helper-owned Studio processes before fresh task-agent heartbeats recreate new task-owned Studio targets.

Default command timeouts:

```text
queued timeout before delivery: 30 seconds
ordinary response timeout after delivery: 60 seconds
play/stop final mode observation timeout: 20 seconds
play/stop pre-action result HTTP post attempt timeout: 3 seconds
single studio_mode_query wait timeout: 5 seconds
```

Command states:

```text
queued
delivered_waiting_response
terminal
```

Moving from `queued` to `delivered_waiting_response` is delivery, not cleanup. Removing a completed terminal record later for retention or log compaction is also not cleanup and must not call the callback again.

A queued command is considered cleaned when helper2 removes it before delivery because of any of these task-scoped events:

```text
lifecycle switch clears the task queue
task release
task lease expiry
mcp2 channel stale recovery
managed Studio process exits, is killed manually, or is replaced/restarted
queued command timeout before delivery
graceful helper2 shutdown
explicit future cancel API, if added
```

The callback result for these cases should identify that the command did not execute, for example:

```text
canceled_by_lifecycle
canceled_by_task_release
canceled_by_task_lease_expiry
canceled_by_mcp2_stale
timed_out_before_delivery
canceled_by_helper_shutdown
```

A delivered command is considered cleaned when helper2 removes `waiting_response_command` without receiving a matching `response_result`. The cleanup reason is determined by the event that removed it:

```text
lifecycle switch
task release
task lease expiry
mcp2 channel stale recovery
managed Studio process exits, is killed manually, or is replaced/restarted
response timeout after delivery
graceful helper2 shutdown
```

For ordinary commands, lifecycle cleanup of `waiting_response_command` is a cancellation unless a matching `response_result` already made the command terminal.

For `studio_play` and `studio_stop`, command-level success is mcp2 sending the pre-action `response_result`. mcp2 still invokes the Studio transition call after the bounded HTTP post attempt, even if the HTTP result is timeout or failure. Final Studio state must still be verified separately through `studio_mode_query` or task status after play/stop command responses. Lifecycle cleanup of a delivered play/stop command before helper2 receives a matching pre-action result is cancellation or timeout on the helper2/upstream waiter side, not proof that mcp2 did not request the Studio transition. Upstream callers should use the 20 second play/stop final mode observation timeout as their verification bound after play/stop command responses.

If the managed Studio process exits, is manually killed, or is replaced/restarted, helper2 treats the old Studio execution environment as gone. Any queued or waiting-response commands bound to that task's old execution environment must be cleaned and completed before the replacement Studio can receive new commands.

If a late `response_result` arrives after cleanup made the command terminal, helper2 should return HTTP 200, log that it was stale, and not call the callback again.

`POST /debug/studio/play/{placeid}` enqueues a `studio_play` command and returns only enqueue status:

```json
{
  "ok": true,
  "queued": true,
  "command_id": 123
}
```

`POST /debug/studio/stop/{placeid}` enqueues a `studio_stop` command and returns only enqueue status:

```json
{
  "ok": true,
  "queued": true,
  "command_id": 124
}
```

`GET /debug/studio/mode/{placeid}` enqueues a `studio_mode_query` command only after an active mcp2 lifecycle is available, waits at most 5 seconds for the matching `response_result`, and returns the observed mcp2 mode when available:

```json
{
  "ok": true,
  "available": true,
  "command_id": 125,
  "command_result": {
    "ok": true,
    "result": {
      "kind": "studio_mode_query",
      "available": true,
      "mode": "edit",
      "mode_seq": 1793097583123991
    }
  }
}
```

If no mcp2 VM answers inside 5 seconds, the endpoint returns `available=false` and does not claim final Studio state. If helper2 is still in `wait_init_mode`, the endpoint waits briefly for the first valid mcp2 pull; if none arrives, it returns `available=false` without creating a queued command.

The debug play/stop HTTP endpoints do not wait for Studio to enter play or stop. The debug mode endpoint exists only to make Phase 7 final-mode verification testable before task-scoped APIs exist.

Formal bridge/MCP play and stop entrypoints should wait for the command callback before returning to upstream. For `studio_play` and `studio_stop`, that callback is completed when helper2 receives the matching pre-action `response_result`. Therefore a successful formal play/stop response means mcp2 answered that it accepted the command and intends to request the transition; the upstream LLM must still verify final Studio mode with `studio_mode_query` or task status.

## mcp2 Loop

mcp2 runs a single sequential loop:

```text
pull_command
  command:
    execute command
    post response_result
    wait for HTTP 200 or transport failure
    if transport failure:
      retry ordinary result posts according to failure type
    pull again

  should_restart_pull:
    pull again immediately

  HTTP failure or server-side close:
    retry according to failure type
```

The generic loop above describes ordinary commands. For `studio_play` and `studio_stop`, command processing must post the pre-action `response_result`, wait up to 3 seconds for the HTTP post attempt to finish, then invoke the Studio transition call. This wait is a bounded transport step, not helper2 approval. If no HTTP 200 arrives before the timeout, mcp2 logs uncertain response delivery and still invokes the Studio transition call.

mcp2 must not create concurrent `pull_command` requests. Readiness for another command is expressed only by starting the next pull.

While a command is executing, mcp2 sends periodic ping requests through the same pull endpoint:

```text
GET /plugin/mcp2/pull_command?mode=...&mode_seq=...&only_ping=1
```

These pings do not break the single command-pull rule because they cannot receive work and cannot consume queue items. They only prove that the current VM lifecycle is still alive during long-running command execution.

Retry behavior:

```text
should_restart_pull:
  immediately start a new pull

TCP connect failure / helper not listening:
  wait 5 seconds, then retry

server-side close / other HTTP failure:
  treat as reconnect condition, then retry
```

## helper2 State

helper2 owns one global command channel:

```text
active_mode = wait_init_mode | edit | play_server
active_mode_seq = none | number
active_studio_pid = none | number
next_command_id = process-local global incrementing number
queue = one global command queue
waiting_pull = optional hanging pull HTTP request
waiting_response_command = optional command awaiting response_result
last_pull_at = timestamp of the last accepted pull
```

There are no per-mode queues. A lifecycle switch invalidates all queued commands because they were created for the previous execution environment.

This global channel is only the pre-hubless local development shape from Phases 1-8. Once hubless task sessions are introduced, the command channel state is scoped by `task_id`. The formal hubless path must not share one queue, active mode, waiting pull, or waiting response across tasks.

`next_command_id` is process-local and globally incrementing. It does not need persistence because helper2 kills Studio processes it started when helper2 exits.

Queued commands are typed internally:

```text
MCP2Command
  command_id
  type = command
  kind = debug_echo | debug_stop_loop | studio_play | studio_stop | studio_mode_query
  args = typed command args
```

The debug echo and stop-loop commands may share a debug argument struct. Formal Studio control commands must have typed args and typed constructors.

## State Transitions

### Initial State

helper2 starts in:

```text
active_mode = wait_init_mode
active_mode_seq = none
```

While in `wait_init_mode`, ordinary command enqueue attempts fail immediately. Commands must not be queued before a valid mcp2 VM has pulled. `GET /debug/studio/mode/{placeid}` is the only debug exception; it may wait briefly for a VM and report `available=false`.

After Phase 4.5 adds PID mapping, the first pull that initializes an active lifecycle must come from a validated helper-managed Studio connection. A non-managed local request, or a request whose PID cannot be mapped to a helper-managed Studio process group, must not initialize lifecycle state, refresh liveness, create a hanging pull, clear queues, or switch lifecycle state. Before Phase 4.5, local development may use the narrower protocol-only validation needed to bring up the command loop, but the Phase 4.5 gate must close that temporary gap.

### Pull With Current Lifecycle

After Phase 4.5, if an incoming pull has the current `active_mode_seq`, helper2 first validates the request source as the same canonical managed Studio PID. After validation succeeds, helper2 accepts it as the current `waiting_pull`.

If a command is already queued, helper2 returns the command immediately.

If no command is queued, helper2 holds the pull for up to 10 seconds.

### Pull With New Lifecycle

After Phase 4.5, if an incoming pull has a different `mode_seq` from `active_mode_seq`, helper2 first validates that the request source is a helper-managed Studio connection and resolves a reliable canonical Studio PID. If that validation or PID resolution fails, helper2 rejects the HTTP request and leaves channel state unchanged. After validation succeeds, helper2 performs an atomic lifecycle switch:

```text
clear queue
clear waiting_response_command
close old waiting_pull directly
active_mode = incoming mode
active_mode_seq = incoming mode_seq
active_studio_pid = canonical managed Studio PID resolved from the incoming HTTP connection
last_pull_at = now
```

The old `waiting_pull` is closed by helper2 as a connection close, not by returning `should_restart_pull`.

This close path must be tested against Roblox `HttpService:RequestAsync`; mcp2 must treat it as a normal reconnect condition.

### Ping With Current Lifecycle

After Phase 4.5, if `only_ping=1` and the incoming `mode_seq` matches the current lifecycle, helper2 first validates the request source as the same canonical managed Studio PID. After validation succeeds, it updates `last_pull_at` and returns `type=pong` immediately.

If `only_ping=1` carries a different `mode_seq`, helper2 must not treat it as a lifecycle switch. It should reject the ping as wrong-lifecycle and leave the channel state unchanged. The next real pull from a validated helper-managed Studio connection is the lifecycle switch path. A random local HTTP request must not be able to initialize lifecycle state, refresh liveness, create a hanging pull, clear queues, or switch lifecycle state by inventing or replaying a `mode_seq`.

After Phase 4.5, if helper2 cannot validate the pull or ping source as a managed Studio connection, it should reject the HTTP request instead of returning `should_restart_pull`. This lets mcp2 use its existing request-failure backoff path and avoids a hot pull retry loop when PID or process-group resolution temporarily fails.

Ping requests never consume commands and never create `waiting_pull`.

## Long-Poll Window

If no command is available, helper2 holds a pull for 10 seconds.

At 10 seconds, helper2 returns:

```json
{
  "type": "should_restart_pull",
  "reason": "no_command_timeout"
}
```

mcp2 immediately opens the next pull. This keeps exactly one pull active from the plugin side and avoids overlapping long-poll requests.

## Stale Recovery

helper2 considers the mcp2 channel stale if no pull arrives for 60 seconds.

The stale timer is reset whenever a command pull or ping arrives successfully.

A watchdog checks stale state every 5 seconds. On stale:

```text
clear queue
clear waiting_pull
clear waiting_response_command
active_mode = wait_init_mode
active_mode_seq = none
if a reliable canonical active_studio_pid exists, kill that matching managed Studio process immediately
if no reliable canonical active_studio_pid exists, do not guess a process from place_id or desired target
```

The restart reason is not the kill operation. Restart happens because StudioManager owns desired Studio targets.

Stale recovery must not kill by inference from the desired target list alone. If helper2 cannot identify the exact managed Studio process that owned the stale lifecycle, it clears the mcp2 channel state, logs the stale condition, and leaves any later restart or reconciliation to StudioManager.

StudioManager must distinguish:

```text
desired Studio targets
running Studio processes
```

helper2 startup and `POST /debug/start-roblox-studio/{placeid}` both create desired Studio targets. If a desired Studio target has no running process, StudioManager starts it again.

Stale recovery kills only the affected managed Studio process. It must not kill unrelated Studio processes or unrelated desired targets.

StudioManager must prune records for managed Studio processes that have already exited before summary generation, desired reconciliation, managed PID lookup, or stale kill. This keeps an externally killed Studio from leaving a stale PID record behind. A stale PID record must not be used later as a screenshot target or kill target. On Windows, the managed-process check should match the recorded launch identity rather than only checking that the PID still exists.

## Other helper2 Interfaces

The helper2 HTTP surface for this draft is:

```text
GET  /healthz
GET  /plugin/mcp2/pull_command
POST /plugin/mcp2/response_result
POST /debug/command/{placeid}
POST /debug/studio/play/{placeid}
POST /debug/studio/stop/{placeid}
GET  /debug/studio/mode/{placeid}
GET  /studio/summary
POST /debug/start-roblox-studio/{placeid}
GET  /debug/studio/screenshot/{placeid}
```

`GET /studio/summary` includes mcp2 channel state:

```text
active_mode
active_mode_seq
last_pull_at
stale
queued_command_count
waiting_pull
waiting_pull_count (0 or 1)
waiting_response_command
active_studio_pid
```

For Phase 7, `/studio/summary` remains a channel-state diagnostic endpoint. It is not required to aggregate recent play/stop post-action Studio API errors; local verification should use logs plus `GET /debug/studio/mode/{placeid}` until task-scoped status APIs exist.

helper2 resolves `active_studio_pid` from the HTTP peer connection, following the helper1 approach:

```text
HTTP peer addr + helper local port -> Windows TCP owner PID
```

The raw TCP owner PID is accepted as `active_studio_pid` only when it maps to a currently running helper-managed Roblox Studio process or a child process in the same helper-owned Studio process group. helper2 should canonicalize child VM PIDs back to the root managed Studio PID before recording channel state, so stale recovery still targets the managed Studio root. This child-process recognition is a safety allowance for direct Play VM/process behavior, not a requirement to support or test Start Server / Start Player split-session mode in this draft. Non-managed local processes must not initialize lifecycle state, refresh liveness, create hanging pulls, trigger lifecycle switches, or clear queued/waiting commands.

## Implementation Boundary

This draft adds the first formal Studio control command kinds, `studio_play` and `studio_stop`, plus temporary debug command kinds.

This draft does not integrate hub or task-server.

This draft does not split command queues by mode.

This draft does not make `response_result` the authority for when the next command may be issued. The plugin decides it is ready by issuing the next `pull_command`.

This draft does not add desired-mode reconciliation, last-writer-wins semantics, or timeout-based Studio kill recovery for play/stop. Concurrent play/stop requests may individually succeed or fail. The required invariant is not programmatic perfection or total serialization; it is that a final Studio state remains observable, the LLM can understand the current state through mode/status queries, and the command channel and plugin mode state remain usable afterward.

## Roadmap: Hubless Session Direction

This section is a roadmap for later PRs, not part of the Phase 1-8 helper2/mcp2 delivery. It is kept here because the Phase 1-8 protocol choices must not block the hubless direction.

The next architecture direction removes hub as a routing and ownership authority. The system keeps `task_id`, but narrows its meaning:

```text
task_id = workspace-local session identity
machine_name = explicit Windows helper2 target
helper2 = Studio/session authority
task-agent = workspace/Rojo/session lease agent
```

## Client 身份边界

Windows client 侧 helper2 自己拥有 public exposure 身份。它必须从同一个本机系统身份目录读取所有 public 身份输入：

```text
%APPDATA%\dev.clock-p.com\
  machine_name
  feishu-user_name
  feishu-token
```

`machine_name` 必须和 `feishu-user_name`、`feishu-token` 放在同一个目录。helper2 禁止通过 CLI flag 覆盖 public 身份；`--public-machine-name`、`--public-user` 和 `--clockbridge-token-file` 对 helper2 都是禁用入口。本机 helper launcher 可以为了诊断预检这些文件，但不能把它们作为 public 身份参数传给 helper2。

这条 helper2 client 身份规则必须和 task-agent 启动规则分开。task-agent 属于 project / server 侧：启动时仍必须显式接收 `--machine_name`，然后把选定机器写入 `.clock-p/session.json`。后续 bridge 指令只使用 `.clock-p/session.json`，不得再次询问 machine，也不得读取 client 侧 `machine_name` 文件。

Repository ownership is intentionally split:

```text
studio-rust-mcp-server: helper2 and the mcp2 Studio plugin
roblox-dev-infra: tools/bridge/task-agent.py and other LLM-facing bridge scripts
rojo: Rojo itself and its built release binary
deprecated: old helper, old hub, old MCP server, and runtime-log server
```

`task_id` is allocated by the project-side task-agent on every task-agent start, not by helper2. A new task-agent process means a new `task_id`. helper2 accepts and binds an explicitly registered `task_id`; it does not create workspace sessions and does not guess one from `place_id`.

The project stores one gitignored local session descriptor:

```text
.clock-p/session.json
```

This file is both the workspace-local session descriptor and the lock record for the current task-agent. It should include at least:

```json
{
  "task_id": "t_20260627_abc123",
  "machine_name": "win-a",
  "place_id": "105986423068266",
  "task_agent_pid": 12345,
  "task_agent_started_at_ms": 1780000000000,
  "task_agent_status_url": "http://127.0.0.1:39241/status",
  "environment": "local",
  "helper": {
    "base_url": "http://127.0.0.1:44750",
    "public_url": null
  },
  "rojo": {
    "pid": 23456,
    "local_url": "http://127.0.0.1:34872",
    "upstream_url": "http://127.0.0.1:34872"
  },
  "updated_at": "..."
}
```

`session.json` intentionally does not store the workspace path. The workspace is useful for diagnostics, but it is not needed for routing, locking, or helper2 task identity. The task-agent's local `GET /status` response should report its current workspace path so humans and replacement logic can inspect it without making it part of the persistent session descriptor.

`task-agent start` must be explicit:

```text
task-agent start --machine_name <machine> --place_id <place_id>
```

`--machine_name` is required. task-agent must not silently reuse a previous machine, scan helpers, fallback to another helper, or infer a helper from `place_id` or workspace history. If the caller does not know the machine, the caller must ask the user before starting task-agent. Missing `--machine_name` is a normal required-argument CLI failure, not a helper2 protocol error.

Other project commands, such as launch, stop, screenshot, log read, and MCP calls, may use `.clock-p/session.json` as their default route source. This is different from task-agent startup: once task-agent has explicitly created the session, follow-up commands should read `helper.base_url` and `task_id` from the local descriptor and call the corresponding task-scoped helper2 endpoint. For example, helper2 task APIs use paths under `/session/{task_id}/...` on `helper.base_url`. In public mode, `helper.base_url` may be derived from `machine_name`; in local development mode, it may be any explicit helper endpoint such as `http://127.0.0.1:44750` or `http://<ip>:<port>`. If the descriptor is missing or the helper2 reports that the task is not registered, the command must fail with a clear instruction to start task-agent with an explicit machine.

`helper.base_url` is the routing authority for bridge scripts in both local and public modes. `helper.public_url`, if present, is diagnostic or informational unless a later public-exposure phase explicitly makes it the same value as `helper.base_url`.

In public mode, task-agent derives the helper base URL from the explicit machine name and current user identity:

```text
https://roblox-helper-{machine_name}-{user}-user.dev.clock-p.com
```

In local development mode, task-agent must be given an explicit helper base URL. It must not guess or reuse a historical helper endpoint.

Before starting a new task-agent, the launcher must inspect `.clock-p/session.json`:

```text
no session.json:
  create a new session descriptor and start task-agent

existing agent is dead:
  clean stale process records and start task-agent with a new task_id

existing agent is alive:
  stop the old task-agent and its Rojo process first, then start the new one
```

Deployment starts are expected to be rare and human-directed. A new `task-agent start` always replaces the recorded live task-agent, even when its machine and place configuration match, because code may have changed and a clean restart is often the intended action.

The old task-agent must be checked before killing. A PID alone is not enough; the launcher should first call the recorded `task_agent_status_url`. If `localhost:<port>/status` is unreachable, the old task-agent is dead. If it is reachable, the response must match the recorded `task_id` and should match the recorded task-agent PID/start time before the launcher asks it to stop through `POST /shutdown`. The status response should also include the workspace path for diagnostics, but workspace mismatch is not a separate session identity rule because the descriptor itself is already workspace-local.

`task_id` is the task-agent's own session identity. The design does not defend against deliberate collisions in local mode; external deployment layers may provide additional authorization. helper2 should treat an incoming `task_id` as the identity chosen by the active task-agent.

task-agent is expected to be a long-running workspace process. It owns:

```text
task_id allocation
.clock-p/session.json maintenance
Rojo server startup and supervision
helper2 heartbeat-based registration / lease refresh
workspace-side artifact and log collection, if needed
```

helper2 owns:

```text
Studio launch, desired ownership, and process binding
studio_pid -> task_id binding
mcp2 plugin lifecycle
runtime log buffering
screenshot and Studio log access
play / stop / MCP command execution
Rojo proxying for Studio-side plugins
```

Studio-side plugins should only connect to local helper2. Rojo plugin must not connect directly to public HTTPS Rojo endpoints. The intended route is:

```text
Rojo plugin -> 127.0.0.1 helper2 -> task_id-bound Rojo upstream
```

helper2 resolves the Studio plugin's task by connection PID, following the helper1 style, and then maps:

```text
connection pid -> studio_pid -> task_id -> Rojo upstream
```

The same task binding rule applies to mcp2 pull and response traffic. Hubless mcp2 requests are routed to a task-scoped command channel only after helper2 maps the Studio connection PID to a managed Studio process and task. It must not route mcp2 traffic only from an explicit task hint supplied by the plugin.

There must be no fallback from `place_id` to `task_id`. If a plugin connection cannot be mapped to a helper2-managed Studio process and task binding, helper2 should reject it with a binding error.

Runtime logs do not require a separate server in this direction. Runtime code should write to local helper2, and project-side commands or task-agent can pull logs from helper2 by `task_id`. helper2 should store runtime logs on local disk rather than keeping an unbounded in-memory buffer. On helper2 shutdown it should best-effort clean its own temporary log files.

### Local-first Hubless Plan

The first hubless implementation should be local-first. Public clockbridge exposure is a later phase, not a dependency for the first task-agent/helper2 loop.

- helper2 endpoints must support local IP/port use. Public helper URLs are optional session fields until the public phase.
- Rojo server also needs local port support. task-agent reports a concrete `rojo.upstream_url` to helper2 through every heartbeat. In local mode this can be a local IP/port. In public mode, Rojo should expose itself with a `task_id`-based domain or path and report that URL to helper2.
- Rojo remains a server in the first hubless phase. The Studio Rojo plugin still connects only to local helper2; helper2 proxies to the task-agent-reported Rojo upstream.
- helper2's Rojo proxy should follow helper1's shape: a local config endpoint returns a helper-local Rojo forward base URL, normal HTTP requests preserve path and query, and `/api/socket/{cursor}` is proxied as a WebSocket session to the configured Rojo upstream.
- task-agent owns Rojo supervision. If Rojo exits, task-agent restarts it at the same helper-visible Rojo upstream. `rojo.upstream_url` is a task-agent lifetime invariant. The URL is repeated in heartbeat to confirm the session contract, not to update it. There is no task-agent proxy indirection in this design.
- task-agent death must also stop its Rojo server. Rojo is not an independent long-lived service in the hubless path. On Windows, task-agent should launch Rojo under a Job Object or equivalent process-tree cleanup mechanism; on Unix-like hosts, it should use a process group plus bounded shutdown/kill fallback. Normal `/shutdown` and Ctrl-C still stop Rojo explicitly, but forced task-agent death must not leave Rojo orphaned as the expected path.
- Runtime-log server is not part of the first hubless phase. Runtime logs go to local helper2, and scripts or task-agent pull logs from helper2 by `task_id`.
- task-agent must expose a random local status port and write `task_agent_status_url` into `.clock-p/session.json`. Liveness is based on `GET /status` on that local port, not on a stop marker in the file. `/status` reports the task id, task-agent PID/start time, place id, machine name, workspace path, Rojo status, helper registration status, and heartbeat timing.
- task-agent must expose `POST /shutdown` for graceful replacement. Ctrl-C should follow the same bounded graceful path.
- `task-agent stop`, `/shutdown`, and Ctrl-C should best-effort release the helper2 session, then stop Rojo after a one second grace window. Release and Rojo shutdown may run in parallel, but the whole shutdown must finish within 5 seconds; if it times out, task-agent should force its own exit instead of waiting forever. None of these paths should rely on writing a stopped marker. Crashes may leave the file unchanged, so the next start must use the status URL to detect whether the agent is alive.
- helper2 needs explicit endpoints for heartbeat, release, status, and task-scoped log reads by `task_id`. Heartbeat is registration: the first heartbeat creates or refreshes the task session; later heartbeats are idempotent refreshes.
- When helper2 accepts a graceful task release, it records that `task_id` as ended for the lifetime of the helper2 process and rejects later heartbeats for that ended task. This prevents a shutting-down task-agent from resurrecting its released task if a late heartbeat leaks through.
- task_id is enough to identify the task-agent session. helper2 does not need a separate workspace identity hash in the first implementation.
- task-agent keeps sending heartbeat attempts while running. Heartbeat cadence is 5 seconds. If helper2 is down or the network is unavailable, task-agent remains alive and keeps retrying; once helper2 returns, the next heartbeat should recreate or refresh the same session.
- A task-agent heartbeat has immutable contract fields for that task-agent lifetime. For the same `task_id`, helper2 should treat heartbeat fields as stable by default and reject attempts whose core fields change without an explicit future design allowing that field to change. The first implementation treats `machine_name`, `place_id`, task-agent PID/start time, and `rojo.upstream_url` as immutable.
- A live task-agent heartbeat is also the desired-Studio signal. helper2 should use the heartbeat's `place_id` and task identity to create or reconcile a task-owned desired Studio target. There is no separate "start Studio" task-agent API in the first design.
- After helper2 restarts, task-agent heartbeat is the recovery mechanism. helper2 should not adopt pre-existing Studio processes that it cannot prove are current managed processes. The first implementation relies on helper2's own process/job ownership to terminate Studio processes it started when helper2 exits, then recreates task-owned desired Studio targets from fresh heartbeats.
- helper2 treats a task-agent lease as expired after 31 seconds without heartbeat.
- Lease expiry is not an ended tombstone. A still-running task-agent may heartbeat again with the same immutable task fields and recreate the same task session after lease expiry. The old task-owned Studio and command state are gone, so the next heartbeat reconciles a fresh task-owned desired Studio target.
- On lease expiry, helper2 keeps a lightweight expired identity record for that `task_id`, containing only immutable task-agent contract fields needed to validate a later recovery heartbeat. It must not keep old Studio PIDs, queues, waiting pulls, waiting-response commands, callbacks, or desired targets in this record.
- helper2 desired Studio targets must carry an owner. Debug/manual desired targets may restart while their owner still exists. Task-agent-owned desired targets exist only while that task lease is live.
- On task-agent release or lease expiry, helper2 must first atomically remove the task-owned desired target, live task session, queues, Studio bindings, and other live task-scoped data from its in-memory maps, then clean up the bound Studio processes. For lease expiry only, the lightweight expired identity record remains so a later same-agent heartbeat can be validated without resurrecting old Studio/command state. This prevents late plugin traffic from seeing a half-released task.
- helper2 lease expiry kills only the Studio processes bound to the expired task session. It must not kill other tasks, other Studio processes with the same `place_id`, or unbound Studio processes. This is intentional recovery behavior, not just a diagnostic state.
- If Studio is killed because task-agent released or timed out, helper2 must not automatically restart that Studio at cleanup time. For lease expiry only, a later heartbeat from the same task-agent identity is a new explicit desired signal and may recreate the task-owned Studio. For graceful release, the ended tombstone rejects later heartbeats, so no recreation happens.
- helper2 should be implemented with multi-task data structures. Each task owns its own mcp2 command channel state, including queue, active mode, waiting pull, waiting response command, and lifecycle timestamps. The first tests may cover only one active task, but the design must not hard-code a single active task or share one command channel across tasks.
- helper2 should not adopt a manually opened Studio or an old Studio process. First implementation only binds Studio processes launched or reconciled by helper2 from a live task-agent heartbeat.
- local development mode is intentionally unauthenticated. Public deployment security is a later review gate rather than a local-mode requirement.
- concurrent `task-agent start` for the same workspace is not handled by the first design. Human operators should not start two deployment commands for the same workspace at the same time.
- Public exposure should be a later phase. At that point helper2 can embed clockbridge/https-proxy as a library, and Rojo can expose a `task_id`-based public endpoint for helper2 to use.
- bridge default routing remains read-only with respect to session selection. Launch, stop, screenshot, log, and MCP scripts may read `.clock-p/session.json`, but they must not create sessions, pick machines, or fallback to historical helpers.
- migration boundary: old hub, old MCP server, and runtime-log server removal should be phased. The first hubless phase should keep Rojo server semantics intact and route Rojo through helper2 before attempting any Rojo client rewrite.

### Minimal Task-scoped Helper APIs

The first hubless helper2 API surface should define concrete task-scoped endpoints before bridge scripts depend on them:

```text
POST /session/{task_id}/heartbeat
POST /session/{task_id}/release
GET  /session/{task_id}/status
POST /session/{task_id}/studio/play
POST /session/{task_id}/studio/stop
GET  /session/{task_id}/studio/mode
GET  /session/{task_id}/studio/screenshot
GET  /session/{task_id}/runtime-log
```

`POST /session/{task_id}/studio/play` and `POST /session/{task_id}/studio/stop` wait for the in-memory command callback. For play/stop, success means mcp2 answered that it accepted the command and intends to request the transition, not final mode. The response should include:

```json
{
  "ok": true,
  "task_id": "t_...",
  "command_id": 123,
  "accepted": true,
  "final_mode_verified": false,
  "next_action": "poll_studio_mode"
}
```

`GET /session/{task_id}/studio/mode` issues or observes `studio_mode_query`, waits at most 5 seconds, and returns:

```json
{
  "ok": true,
  "task_id": "t_...",
  "mode": "edit",
  "mode_seq": 1793097583123,
  "available": true,
  "run_service": {
    "is_edit": true,
    "is_running": false,
    "is_server": true,
    "is_client": true,
    "is_run_mode": false,
    "is_studio": true
  }
}
```

If no mcp2 VM is available within 5 seconds, `available=false` and `mode="unknown"` is a valid response. Upstream play/stop verification should poll this endpoint for at most 20 seconds.

`GET /session/{task_id}/status` returns task session state, lease freshness, desired Studio owner state, known Studio PIDs, mcp2 channel summary, Rojo upstream status, and recent command terminal summaries. It is the script-friendly status endpoint for bridge tooling.

### Minimal Runtime Log Protocol

Runtime logs use helper2 directly in hubless mode. There is no runtime-log server in the local hubless path.

Studio-side runtime log upload:

```text
POST /runtime-log/upload
```

The upload endpoint does not trust a task id from the runtime body. helper2 maps the HTTP peer PID to a managed Studio process and task. If the sender cannot be mapped to a task, helper2 rejects the upload with a binding error.

Minimal upload body:

```json
{
  "runtime_id": "server",
  "mode": "play_server",
  "timestamp_ms": 1793097583123,
  "level": "info",
  "message": "...",
  "fields": {}
}
```

Task-side log read:

```text
GET /session/{task_id}/runtime-log?cursor=<opaque>&limit=200
```

Minimal read response:

```json
{
  "ok": true,
  "task_id": "t_...",
  "entries": [],
  "next_cursor": "..."
}
```

helper2 stores logs on local disk by `task_id` and runtime identity. The first implementation should cap retained logs per task to a bounded size or line count and should best-effort clean helper-owned temporary logs on helper2 shutdown.

### Hubless Error Responses

Error codes should be introduced when a process boundary actually needs them, not predeclared for argument validation or purely local CLI errors. For example, missing `--machine_name` is a required CLI argument failure and does not need a protocol code.

When helper2 or bridge-facing APIs return structured failures, they should use this shape:

```json
{
  "ok": false,
  "code": "task_not_registered",
  "message": "helper2 has no registered session for task_id t_xxx",
  "action": "restart_task_agent"
}
```

Go implementations should model these errors with typed constants or typed error structs. Python 3 bridge/task-agent code should use type annotations for the same response shape.

## Testability

helper2 tests should use local Roblox Studio processes and local helper2 ports. They must not depend on online hub/task-server.

Roblox Studio CLI observations on Windows:

```text
RobloxStudioBeta.exe -task EditPlace -placeId <place_id> -universeId <universe_id>
```

This is stable enough for helper2 startup, restart, stale recovery, and edit-mode mcp2 tests. helper2 already uses this launch shape.

Studio Start Server / Start Player split-session mode can create additional Studio processes with CLI tasks such as:

```text
RobloxStudioBeta.exe -task StartServer ...
RobloxStudioBeta.exe -task StartClient ...
```

These observations are kept only as future investigation notes. Their arguments include session-specific values such as edit PID, target id, transport settings, session guid, and user metadata. Start Server / Start Player split-session mode is not a Phase 1-8 acceptance fixture. Current acceptance tests must use direct Studio Play.

The Phase 1-8 test plan must cover:

- edit VM connects, pulls, receives `should_restart_pull(no_command_timeout)` when no command is available, and continues.
- `/debug/command/{placeid}` enqueues a command and mcp2 returns `response_result`.
- long command execution sends `only_ping=1` and prevents false stale.
- `debug_stop_loop` stops mcp2 pull traffic so stale recovery can be tested without editing Studio manually.
- direct Play `play_client` logs mode and does not call helper2.
- direct Play `play_server` enters the same sequential pull loop as edit.
- `mode_seq` switch clears the queue and closes the old `waiting_pull`.
- server-side close of a hanging pull is observed by Roblox `HttpService:RequestAsync` as a retryable failure.
- 60 second stale detection kills only the matching managed Studio when helper2 has a reliable canonical PID, and StudioManager restarts it because the desired target remains.
- 60 second stale detection without a reliable canonical PID clears channel state, logs the stale condition, and does not guess a Studio process to kill.
- `/debug/studio/play/{placeid}` enqueues `studio_play` and returns without waiting for play completion.
- `/debug/studio/stop/{placeid}` enqueues `studio_stop` and returns without waiting for stop completion.
- `edit + studio_stop` returns success with an `already_stopped` result.
- `play_server + studio_play` returns failure with an error that clearly identifies `already_playing`.
- concurrent play/stop requests may produce mixed command results, but afterward a fresh play or stop command must still leave mcp2 in a coherent mode and helper2 with no stale queue or waiting-response residue.

### Current Mainline Test Layers

The Phase 9+ mainline should use these layers. They are not Phase 1-8 acceptance requirements. Do not start with full Studio integration when a state-machine or process-level fake can prove the same behavior faster.

Layer 1: pure local fakes.

```text
helper2 state tests
task-agent unit tests
fake mcp2 pull/response/ping client
fake Rojo HTTP/WebSocket server
fake runtime-log uploader
```

Layer 2: local process integration.

```text
real helper2 process
real task-agent process
fake Rojo process when Rojo behavior is not under test
local random ports
PowerShell process start/stop/kill checks
```

Layer 3: real Roblox Studio integration.

```text
RobloxStudioBeta.exe edit launch
real mcp2 plugin
real Studio PID binding
real Studio play/stop transitions
real screenshot/log surfaces
```

### Current Mainline Test Gates

Each gate below is a must-have passing condition for the relevant Phase 9+ mainline phase before moving to the next gate. These gates are not current Phase 1-8 acceptance requirements.

Task-agent and session gate:

- Starting task-agent creates a new `task_id` every time.
- `.clock-p/session.json` contains task id, machine name, place id, task-agent PID/start time/status URL, helper base URL, and Rojo URLs.
- `.clock-p/session.json` does not store workspace path.
- `GET /status` reports workspace path, task id, PID/start time, Rojo status, helper registration status, and heartbeat timing.
- Missing `--machine_name` fails through CLI argument parsing.
- Public mode derives `helper.base_url` from `https://roblox-helper-{machine_name}-{user}-user.dev.clock-p.com`.
- Local mode requires an explicit helper base URL and does not guess or reuse history.
- Existing live task-agent is replaced through `/shutdown`; dead status URL is treated as stale and volatile fields are cleaned.
- Bridge scripts with missing session descriptor fail with a clear task-agent start instruction.

Rojo lifecycle gate:

- task-agent starts Rojo in server mode and records a stable helper-visible `rojo.upstream_url`.
- Rojo crash is followed by restart on the same `rojo.upstream_url`.
- Changing `rojo.upstream_url` for the same task is rejected by helper2 heartbeat validation.
- task-agent graceful shutdown releases helper2, waits one second, stops Rojo, and exits within 5 seconds.
- task-agent crash or forced kill also stops its Rojo server.

Heartbeat, lease, and release gate:

- First heartbeat creates or refreshes task session.
- Repeated heartbeat from the same task-agent is idempotent.
- Heartbeat after helper2 restart recreates or refreshes the same task session.
- Heartbeat after network/helper outage resumes without task-agent restart.
- Mutating immutable heartbeat fields is rejected, including machine name, place id, task-agent PID/start time, and `rojo.upstream_url`.
- 31 second lease expiry atomically removes task session, task-owned desired Studio, queues, waiting response, and Studio bindings before killing bound Studio.
- Lease expiry does not create an ended tombstone; later heartbeat from the same task-agent identity may recreate the task session and desired Studio.
- Graceful release creates an ended tombstone for helper2 process lifetime; later heartbeat for that task id is rejected.
- Release or lease expiry for one task does not affect another task, same-place unrelated task, or manually opened unbound Studio.

mcp2 protocol gate:

- Long poll with no command returns `should_restart_pull(no_command_timeout)` after 10 seconds and the plugin opens the next pull without concurrent pulls.
- `only_ping=1` refreshes liveness during long commands, does not consume queued commands, and prevents false stale.
- New `mode_seq` clears only that task's queue and waiting response, closes the old hanging pull, and is retryable by Roblox `HttpService:RequestAsync`.
- Invalid pull/ping source is rejected at HTTP level and mcp2 backs off instead of hot-looping `should_restart_pull`.
- `play_client` logs mode and never enters the helper2 command loop.
- `edit` and `play_server` use the sequential pull loop.
- mcp2 pull and response traffic is routed to task by Studio PID binding, not by plugin-supplied task hint.
- `response_result` from an invalid source, wrong canonical Studio PID, wrong `mode_seq`, or non-waiting `command_id` receives HTTP 200 but is ignored internally and must not complete a command.
- Cross-task queues, waiting pulls, waiting responses, active modes, and command ids do not contaminate each other.

Command callback and cleanup gate:

- Every formal command has queued timeout, response timeout, and exactly-once in-memory callback.
- Callback fires exactly once for normal response.
- Callback fires exactly once for lifecycle cleanup.
- Callback fires exactly once for task release and task lease expiry cleanup.
- Callback fires exactly once for queued timeout and response timeout.
- Callback fires exactly once when Studio exits, is manually killed, or is replaced/restarted.
- Callback fires exactly once on helper2 shutdown.
- Late `response_result` after terminal state receives HTTP 200, is logged as stale, and does not call the callback again.
- Cleanup is tested while command is queued, while command is waiting response, after play/stop pre-action HTTP 200, after play/stop pre-action HTTP timeout without executing the Studio transition, and during stale recovery.
- Replacement Studio never consumes old queued or waiting-response commands.

Task-scoped Studio API gate:

- `POST /session/{task_id}/studio/play` waits for command callback and returns mcp2's pre-action command response, not final mode proof.
- `POST /session/{task_id}/studio/stop` waits for command callback and returns mcp2's pre-action command response, not final mode proof.
- mcp2 posts play/stop pre-action `response_result`, waits at most 3 seconds for the HTTP post attempt to finish, then invokes Studio transition calls even if the HTTP result is timeout or failure.
- Later Studio API error after the pre-action command response is reflected through logs/status/mode verification failure and does not mutate an already completed command callback.
- `GET /session/{task_id}/studio/mode` returns edit mode in edit VM.
- `GET /session/{task_id}/studio/mode` returns play_server mode in play server VM.
- If no VM is available, mode query returns `available=false` and `mode=unknown` within 5 seconds.
- Upstream play/stop verification polls mode or task status for at most 20 seconds.
- Repeated play -> stop -> play -> stop works through task-scoped APIs.
- Concurrent play/stop may produce mixed command outcomes, but afterward mode/status queries expose an understandable current state, a fresh play or stop works, and task status has no stuck queue, stale waiting response, or incoherent active mode.
- These gates use direct Studio Play only. Start Server / Start Player split-session mode is explicitly not a Phase 1-8 or first task-scoped API acceptance requirement.

Studio process gate:

- Stale recovery kills only the matching managed Studio and does not kill unrelated Studio processes.
- Debug/manual desired Studio may restart while its owner exists.
- Task-owned Studio does not restart at release or lease-expiry cleanup time.
- Later heartbeat after lease expiry may recreate task-owned Studio.
- Helper2 shutdown uses helper-owned process cleanup so old Studio does not contaminate fresh heartbeat recovery.
- Real Studio PID binding is verified with the real mcp2 plugin in direct Play mode, not only fake mcp2. Server/client split-session process binding is left for a later phase.

Rojo proxy gate:

- Rojo plugin connects only to local helper2.
- helper2 config endpoint returns the helper-local Rojo forward base URL.
- helper2 forwards normal Rojo HTTP paths and query strings to the task-bound upstream.
- helper2 proxies `/api/socket/{cursor}` as a WebSocket session to the task-bound upstream.
- Plugin traffic without task binding is rejected.
- Cross-task Rojo traffic is rejected or isolated.
- Same-place multi-task traffic does not fall back from `place_id` to task.

Runtime log gate:

- Studio-side runtime log upload uses `POST /runtime-log/upload`.
- Upload from an unmapped Studio connection is rejected.
- Upload is bound to task by connection PID, not by a trusted body task id.
- Logs are stored by task id and runtime identity.
- `GET /session/{task_id}/runtime-log` reads only that task's logs.
- Cursor pagination works.
- Per-task retention cap by size or line count is enforced.
- helper2 shutdown best-effort cleans helper-owned temporary logs.
- The local hubless path has no dependency on the old runtime-log server.

Bridge and cross-repo gate:

- `../roblox-dev-infra` task-agent and bridge scripts are tested as real scripts, not only mocked from helper2 tests.
- launch, stop, status, screenshot, log, and MCP scripts read `.clock-p/session.json`.
- Bridge scripts do not create sessions, choose machines, scan helpers, or fallback to historical helpers.
- Missing descriptor, unregistered task, stale task, and unreachable helper produce clear failures.
- Local descriptor and public descriptor both route through `helper.base_url`.
- `helper.public_url` is not used as a routing authority unless a later public phase makes it equal to `helper.base_url`.
- Local hubless tests do not depend on old hub, old MCP server, or old runtime-log server.

## Obsolete Phase 1-8 Notes

Phases 1-8 below are obsolete and are retained only as historical context for the early global command-channel design. Do not use them as the current implementation sequence for the roblox-agent refactor. The active mainline begins at Phase 9.

### Phase 1: Protocol Shape

- Add `mode_seq` generation in mcp2.
- Send `mode` and `mode_seq` on `pull_command`.
- Handle `type=command` and `type=should_restart_pull`.
- Keep the plugin loop strictly sequential.
- Keep `play_client` as log-and-return.
- Verify edit mode with `RobloxStudioBeta.exe -task EditPlace`.

### Phase 2: helper2 Command Channel

- Replace the immediate noop pull with a real single global queue.
- Add `POST /debug/command/{placeid}` to enqueue a simple debug command.
- Use process-local global incrementing `command_id`.
- Track `waiting_response_command` for diagnostics.
- Add mcp2 channel fields to `/studio/summary`.
- Verify debug command delivery and response locally.

### Phase 3: Long-Poll Semantics

- Hold `waiting_pull` for up to 10 seconds.
- Return `should_restart_pull(no_command_timeout)` when no command arrives.
- Wake the current pull when a queued command becomes available.
- Ensure one pull returns at most one command.

### Phase 4: Lifecycle Switch

- Treat incoming `mode_seq` changes as lifecycle switches.
- Clear queue and `waiting_response_command`.
- Close the old `waiting_pull` directly instead of returning a protocol message.
- Test Roblox `HttpService:RequestAsync` behavior when helper2 closes the HTTP connection.
- Use direct Studio Play to verify `edit -> play_server -> edit` lifecycle changes. Do not use Start Server / Start Player split-session fixtures for Phase 4 acceptance.

### Phase 4.5: Ping and PID Mapping

- Add `only_ping=1` handling to `pull_command`.
- Return `type=pong` immediately for pings.
- Send pings from mcp2 while a command is executing.
- Reject pings that carry a non-current `mode_seq`; lifecycle switches happen through real pulls, not pings.
- Resolve the peer PID for pull and ping requests using the helper1 TCP owner lookup approach.
- Add `active_studio_pid` to `/studio/summary`.
- Verify a long-running debug command can keep the channel fresh with pings.

### Phase 5: Stale Recovery

- Add `last_pull_at` and 60 second stale detection.
- Run a watchdog every 5 seconds.
- On stale, clear mcp2 channel state and close `waiting_pull`.
- Kill only the matching managed Studio process when helper2 has a reliable canonical PID; otherwise clear channel state and do not guess a process to kill.
- Let StudioManager restart only because the desired target remains desired.
- Verify by preventing pull traffic or killing the mcp2 side and watching helper2 recover the desired Studio target when a reliable PID exists.
- Verify the no-reliable-PID stale path clears channel state and does not kill by `placeid` or desired target inference.

### Phase 6: StudioManager Desired Targets

- Make helper2 startup auto-launch create a desired Studio target.
- Make `POST /debug/start-roblox-studio/{placeid}` create a desired Studio target.
- Track desired targets separately from running processes.
- Restart a desired target when its managed process exits or is killed by stale recovery.

### Phase 7: Formal Studio Play/Stop Commands

- Replace untyped helper2 command construction with typed Go command structs and typed command kinds.
- Add `studio_play`, `studio_stop`, and `studio_mode_query` command kinds.
- Add command timeout and exactly-once completion callback support.
- Make queue cleanup and waiting-response cleanup complete commands through callbacks.
- Add `POST /debug/studio/play/{placeid}` and `POST /debug/studio/stop/{placeid}`.
- Add temporary `GET /debug/studio/mode/{placeid}` for Phase 7 final-mode verification before task-scoped mode APIs exist.
- Keep the debug play/stop endpoints enqueue-only; they must not wait for final Studio mode.
- Implement mcp2 `studio_play` in edit mode by posting pre-action `response_result`, waiting at most 3 seconds for the HTTP post attempt to finish, then calling `StudioTestService:ExecutePlayModeAsync({})`.
- Implement mcp2 `studio_stop` in play_server mode by posting pre-action `response_result`, waiting at most 3 seconds for the HTTP post attempt to finish, then calling `StudioTestService:EndTest({})`.
- Implement mcp2 `studio_mode_query` to return current mcp2 mode, `mode_seq`, and RunService flags.
- Verify play/stop still requests the Studio transition after the 3 second pre-action result HTTP post attempt timeout.
- Make `edit + studio_stop` return success with `already_stopped`.
- Make `play_server + studio_play` return failure with an `already_playing` error.
- Treat play/stop command success as mcp2's pre-action command response, not final mode proof.
- Verify final play/stop mode through `studio_mode_query` or task status after command success.
- Mark commands canceled or timed out when queue/waiting-response cleanup removes them before terminal response.
- Return HTTP 200 but ignore late `response_result` after a command is already terminal.
- Clean and callback commands when the managed Studio process exits, is manually killed, or is replaced/restarted.
- Run repeated play -> stop -> play -> stop tests locally.
- Run concurrent play/stop enqueue tests locally.
- After concurrency tests, verify a fresh play or stop command still works and `/studio/summary` has no stuck queue, no stale waiting response command, and a coherent `active_mode`.
- Verify callbacks fire exactly once for normal response, lifecycle cleanup, mcp2 stale cleanup, Studio process replacement, timeout, and late response.
- Do not treat Start Server / Start Player split-session mode as part of Phase 7 acceptance. Local tests should use direct Studio Play.

### Phase 8: Local Studio Screenshot

- Add `GET /debug/studio/screenshot/{placeid}` to helper2.
- Resolve the managed Studio PID for the requested place id.
- If multiple managed Studio processes exist for the same place id, fail instead of guessing.
- On Windows, enumerate visible top-level windows and prefer a Roblox Studio window with the exact managed PID.
- If no exact PID window is found but exactly one visible Roblox Studio window exists, allow a fallback and mark it in the response. This intentionally follows helper1's screenshot behavior.
- Enumerate child windows to find the Studio viewport, following helper1's size-based heuristic.
- Capture the selected window with PowerShell and `PrintWindow(hwnd, hdc, 2)`.
- If the first capture fails or looks black, restore/foreground the Studio window, wait briefly, and retry once.
- Save the PNG under the helper2 data directory and return the path, selected PID, window title, dimensions, byte count, and fallback flag.
- Verify screenshot capture locally in edit mode and play mode.

## Current Mainline Phases

The phases below are the current roblox-agent refactor mainline.

Phase completion rule: a phase is not complete when code exists. A phase is complete only when its functionality is implemented, its listed test gate passes, and the exact local commands or real Studio runs used for that gate are recorded. Fake/unit tests may guard logic, but they never replace a required real Studio or real process lifecycle gate.

### Phase 9: Task Agent Local Session and Rojo

- Add a project-local `.clock-p/session.json` schema.
- Make `.clock-p/session.json` gitignored in project workspaces.
- Do not store the workspace path in `.clock-p/session.json`.
- Implement `task-agent start --machine_name <machine> --place_id <place_id>`.
- Let the CLI argument parser fail when `--machine_name` is missing.
- Do not read a historical machine as a startup default.
- In public mode, derive `helper.base_url` as `https://roblox-helper-{machine_name}-{user}-user.dev.clock-p.com`.
- In local development mode, require an explicit helper base URL instead of guessing.
- Generate a new `task_id` on every task-agent start.
- Allocate a random local task-agent status port.
- Start and supervise Rojo in server mode on a helper-visible `rojo.upstream_url`.
- If Rojo exits, restart it on the same helper-visible `rojo.upstream_url`.
- If task-agent exits, crashes, or is killed, its Rojo server must also stop.
- Write `task_agent_pid`, `task_agent_started_at_ms`, `task_agent_status_url`, `helper.base_url`, `rojo.local_url`, and `rojo.upstream_url` into the session descriptor.
- Implement `GET /status` on the task-agent local port, including the task id, task-agent PID/start time, place id, machine name, current workspace path, Rojo status, helper registration status, and heartbeat timing.
- Implement `POST /shutdown` on the task-agent local port.
- Make Ctrl-C follow the same bounded graceful shutdown path as `/shutdown`.
- During graceful shutdown, release helper2 first, then stop Rojo after a one second grace window; keep the whole operation bounded to 5 seconds and force exit on timeout.
- Before starting, inspect existing `.clock-p/session.json`.
- If the recorded `task_agent_status_url` is reachable and reports the recorded task id, request old task-agent shutdown before starting the new one without comparing configuration.
- If it is unreachable, treat the old task-agent as dead and clean volatile process fields.
- Test Gate:
  - Unit-test descriptor load/save, public/local helper URL derivation, required `--machine_name`, required local `--helper-base-url`, and old descriptor identity handling.
  - Process-test task-agent start with the real configured Rojo binary. Verify `.clock-p/session.json`, random status URL, `GET /status`, helper registration state, and stable `rojo.upstream_url`.
  - Kill the Rojo child process and verify task-agent restarts Rojo on the same helper-visible upstream URL.
  - Start a second task-agent in the same workspace and verify the first live agent is shut down through its status URL before the new one writes a descriptor.
  - Stop task-agent gracefully and verify helper release is observed by the helper endpoint, the Rojo PID exits, and the Rojo upstream port closes.
  - Force-kill task-agent and verify the original Rojo PID exits, the upstream port closes, and no child Rojo process remains after the cleanup timeout.
  - Phase 9 cannot pass if Rojo is only observed as "started"; restart and no-orphan behavior must be observed.

### Phase 10: helper2 Task Session and Desired Studio

- Add helper2 session state keyed by `task_id`.
- Use multi-task maps even if only one task is tested at first.
- Add `POST /session/{task_id}/heartbeat`.
- Add `POST /session/{task_id}/release`.
- Add `GET /session/{task_id}/status`.
- Heartbeat accepts `task_id`, `machine_name`, `place_id`, task-agent PID/start time, and `rojo.upstream_url`.
- Treat heartbeat as registration. The first heartbeat creates or refreshes the task session, and later heartbeats refresh it idempotently.
- Treat `task_id` as the session identity supplied by the live task-agent.
- Reject heartbeat attempts for an already registered `task_id` if immutable task-agent contract fields do not match.
- Store `task_id -> rojo.upstream_url`.
- Reject heartbeat for an existing `task_id` if `rojo.upstream_url` changes.
- Accept repeated heartbeat attempts from the same live task-agent after helper2 restart or network recovery.
- task-agent sends heartbeats every 5 seconds.
- Use each live heartbeat as the desired-Studio signal for that task's `place_id`.
- Create or reconcile a task-owned desired Studio target from heartbeat; do not add a separate task-agent start-Studio API in this phase.
- Track desired Studio ownership so task-owned desired targets and debug/manual desired targets have different restart semantics.
- Mark task sessions stale when no heartbeat arrives for 31 seconds.
- Treat lease expiry as recoverable by a later heartbeat from the same task-agent identity; do not record an ended tombstone for lease expiry.
- On lease expiry, keep only a lightweight expired identity record with immutable task-agent contract fields, so a later same-agent heartbeat can be validated without keeping old Studio or command state.
- On graceful task release, record the task as ended for the helper2 process lifetime and reject later heartbeats for that `task_id`.
- On task session release or lease expiry, first atomically remove the task-owned desired target, task session, known Studio ownership/binding state, and Phase 10 task-scoped state from helper2 maps.
- After the atomic removal, kill Studio processes bound to that task.
- Do not auto-restart Studio after task-agent release or lease expiry.
- Same-place multi-task is legal. Desired Studio state must be keyed by task ownership, not by `place_id`.
- Test Gate:
  - Unit-test heartbeat registration, immutable contract rejection, `rojo.upstream_url` immutability, release tombstones, recoverable lease expiry, and same-contract recovery.
  - Unit-test same-place tasks producing independent desired Studio targets.
  - Unit-test release and lease expiry removing only the matching task's desired target and state.
  - Local helper2 process-test heartbeat, status, release, stale expiry, and re-heartbeat recovery.
  - Local helper2 restart test: helper-owned processes are cleaned up by helper exit, and the next task-agent heartbeat restores only that task's desired Studio.
  - Multi-task process-test: releasing or expiring task B must not mutate task A, including when A and B use the same `place_id`.

### Phase 11: Task-scoped mcp2 and Studio APIs

- Scope mcp2 command channel state by `task_id`; do not share queue, active mode, waiting pull, or waiting response command across tasks.
- Route hubless mcp2 pull and response traffic by Studio PID binding to `task_id`, not by a plugin-supplied task hint.
- Add the minimal task-scoped helper2 Studio API paths:
  - `POST /session/{task_id}/studio/play`
  - `POST /session/{task_id}/studio/stop`
  - `GET /session/{task_id}/studio/mode`
  - `GET /session/{task_id}/studio/screenshot`
- Make task-scoped play/stop wait for the in-memory command callback and return mcp2's pre-action command response, not final mode proof.
- Make task-scoped mode query wait at most 5 seconds and return `available=false`, `mode=unknown` when no VM is available.
- Verify final play/stop mode by polling task-scoped mode or task status for at most 20 seconds.
- Promote screenshot from debug place routing to task-scoped Studio PID routing.
- Same-place multi-task command routing must be PID/task-bound, not place-bound.
- Test Gate:
  - Unit-test independent command brokers per task, cleanup of one task without touching another, stale channel cleanup, late response rejection, and wrong PID/mode sequence rejection.
  - Unit-test task-scoped API errors for missing, unknown, ended, expired, and Studio-not-available tasks.
  - Real Studio test: one task can run mode, play, stop, and screenshot through task-scoped APIs.
  - Real Studio evidence must include helper2 resolving pull/result traffic from an actual Roblox Studio process PID and a real screenshot output. Injected responses or non-Studio HTTP clients do not satisfy this gate.
  - Real Studio repeated test: play -> stop -> play -> stop returns to a coherent edit state and leaves no stuck queue or waiting response.
  - Real Studio same-place two-task test: both task-owned Studios can coexist, both can be controlled independently, and concurrent play/stop/mode/screenshot does not cross task boundaries.
  - Real Studio lease/release test: task B expiry or release must not disturb task A's Studio, mode control, screenshot, or command broker.

### Phase 12: Local Rojo Through helper2

- Rojo remains a Studio-side plugin/client flow. This phase is not complete until the Studio-side Rojo client actually uses helper2.
- Phase 12 depends on Phase 9's task-agent Rojo server and Phase 10's helper2 task session. It does not re-own task-agent startup or Rojo supervision.
- Studio-side Rojo traffic connects only to local helper2. It must not connect directly to public Rojo URLs.
- Add helper2 local Rojo config endpoint for Studio Rojo plugin traffic:
  - `GET /v1/rojo/config?place_id=<place_id>`
  - optional `task_id` for newer clients
  - response `base_url` points to `/rojo-forward/{place_id}/task/{task_id}`
- helper2 resolves the caller by peer process -> managed Studio PID -> live task session.
- helper2 must reject traffic when caller is not a helper-managed task-owned Studio, task is not live, `place_id` mismatches, or provided `task_id` mismatches.
- Add helper2 task-bound local Rojo forwarding:
  - normal HTTP paths and query strings forward to that task's `rojo.upstream_url`
  - `/api/socket/{cursor}` WebSocket forwards to that task's `rojo.upstream_url`
  - every forward request re-checks caller Studio binding and requested `{place_id, task_id}`
- Do not fallback from `place_id` to `task_id`, and do not use place-only lookup when same-place multi-task exists.
- Bridge scripts must not validate local helper2 Rojo by directly calling `/rojo-forward` as an external process. External direct calls bypass the Studio peer-PID binding and must fail.
- Test Gate:
  - Unit-test config URL construction, HTTP target URL construction, WebSocket target URL construction, hop-by-hop header stripping, and task/place mismatch errors.
  - Fake upstream test: helper2 forwards normal HTTP and WebSocket traffic to the task-bound Rojo upstream and preserves path/query.
  - Real Studio test: Studio-side Rojo client requests `/v1/rojo/config` through helper2 and receives the task-bound helper-local `base_url`.
  - Real Studio test: Studio-side Rojo client uses the returned `base_url` for HTTP sync against the real task-agent Rojo server.
  - Real Studio test: WebSocket sync is covered, or the phase remains incomplete with WebSocket explicitly listed as unpassed.
  - Same-place real Studio negative test: task A's Studio cannot use task B's `task_id`, and task A cannot use a mismatched `place_id`.
  - External negative test: direct Python/shell/browser calls to local helper2 `/v1/rojo/config` and `/rojo-forward/...` are rejected as non-Studio traffic.
  - Phase 12 cannot pass with only external Python/shell Rojo requests.

### Phase 13: Runtime Logs via helper2

- Remove runtime-log server from the local hubless path.
- Runtime sends logs to local helper2 through `POST /runtime-log/upload`.
- helper2 persists logs by `task_id` and runtime identity under a helper-owned local directory.
- task-agent or bridge scripts pull logs from helper2 by `GET /session/{task_id}/runtime-log`.
- Bind incoming runtime log writes through helper2's connection/PID-to-task mapping when the sender is Studio-side; task-agent-side reads use explicit `task_id`.
- Define minimal runtime log body fields: `runtime_id`, `mode`, `timestamp_ms`, `level`, `message`, and `fields`.
- Add a bounded per-task retention policy by size or line count.
- helper2 cleans helper-owned temporary log files best-effort on shutdown.
- Add slow upload or no-result logging only inside helper2 where the result can be observed.
- Runtime log upload is a Studio-side function when the sender is Studio/runtime code. External bridge scripts can read task logs but must not upload runtime logs as a substitute for Studio-originated upload.
- Test Gate:
  - Unit-test minimal log body validation, cursor reads, per-task retention, task separation, and cleanup of helper-owned temp files.
  - Unit-test runtime-log upload binding errors for non-managed callers, unbound Studio callers, non-live tasks, and invalid payloads.
  - Real Studio test: Studio/plugin/runtime code posts one or more log entries to local helper2.
  - Real Studio test: `GET /session/{task_id}/runtime-log` returns the uploaded entries for the correct task.
  - External negative test: direct Python/shell/browser upload to `/runtime-log/upload` is rejected as non-Studio traffic.
  - Same-place real Studio negative test: task B cannot read task A's logs.
  - Phase 13 cannot pass if logs are only inserted directly into the store by tests or read as empty data.

### Phase 14: Non-MCP Bridge Scripts Read Session Defaults

- Update launch, stop, screenshot, status, and runtime-log read scripts to read `.clock-p/session.json`.
- MCP script migration belongs to Phase 15, after helper2 MCP replacement exists.
- Scripts use `helper.base_url` plus the documented task-scoped helper2 path, such as `/session/{task_id}/...`.
- Use the minimal task-scoped helper2 API paths for play, stop, mode, screenshot, status, and runtime log reads.
- Scripts must not create sessions, choose machines, scan helpers, or fallback to historical helpers.
- If session descriptor is missing, fail with a clear task-agent start instruction.
- If helper2 reports `task_not_registered` or stale task, fail with a clear task-agent restart instruction.
- Rojo bridge/readiness scripts must follow the Phase 12 boundary. They must not treat external calls to helper2 local `/rojo-forward` as valid Studio-side Rojo validation.
- Test Gate:
  - Python unit-test descriptor parsing, missing descriptor errors, stale/not_registered errors, and no fallback to old hub/helper1/mcp1/runtime-log.
  - Python unit-test every bridge script uses `helper.base_url` and explicit `task_id`.
  - Local script E2E uses a real generated task-agent descriptor and live helper2/Studio for Studio-affecting actions. Mock HTTP servers do not satisfy this gate.
  - Local script E2E: status, play, stop, mode, screenshot, and runtime-log read work from `.clock-p/session.json`.
  - Rojo script E2E: backend Rojo health is checked through an allowed task-agent/backend route, and Studio-side Rojo readiness is not claimed unless Phase 12 Studio-side route passed.
  - Phase 14 cannot pass while any active bridge script depends on legacy hub routes or external helper2 `/rojo-forward` calls for local Studio-side Rojo validation.

### Phase 15: helper2 MCP Server Replacement

- Move the public MCP surface into helper2.
- Keep mcp2 plugin on localhost helper2.
- Route MCP commands by explicit `task_id`.
- Enforce `task_id -> studio_pid` binding for Studio-affecting commands.
- Remove dependency on the old Linux MCP server in local hubless mode.
- Update MCP request scripts to read `.clock-p/session.json`, use helper2 MCP at `helper.base_url`, pass explicit `task_id`, and avoid old MCP routes.
- Expose only helper2 task-scoped MCP tools. Do not preserve old helper1/mcp1 tool aliases as compatibility fallbacks.
- Test Gate:
  - Unit-test `initialize`, `notifications/initialized`, `tools/list`, unknown tools, explicit `task_id` requirements, and removal of old tool aliases.
  - Unit-test MCP status, mode, play, stop, screenshot, and runtime-log tool payloads and error shapes.
  - Real Studio local MCP test: status, play, stop, mode, and screenshot work through helper2 MCP.
  - Real Studio runtime-log MCP test: `helper2_runtime_log` returns entries created by Phase 13's real Studio upload path.
  - Same-place real Studio MCP negative test: commands for task A cannot fall back to task B's open Studio.
  - Phase 15 cannot pass while MCP log is only tested against fake/empty store data.

### Phase 16: Public Exposure

- Add helper2 public exposure through clockbridge/https-proxy.
- Keep shelling out to `clockbridge-cli` only as an optional early-development fallback.
- Add public `helper.public_url` to `.clock-p/session.json`.
- helper2 owns the public exposure lifecycle. task-agent writes descriptor and heartbeat only; it must not start, stop, or supervise the public proxy process.
- The current Phase 16 public target is helper2 public exposure for helper2 task/MCP APIs. Studio-side Rojo traffic remains local helper2 traffic from Studio and is covered by Phase 12.
- Keep Studio-side plugins on local helper2 even in public mode.
- Test Gate:
  - Unit-test public host derivation, public command construction, token redaction, required public arguments, public manager start/stop/restart state, and job/process cleanup.
  - dev-infra Python unit-test public helper startup command, public readiness validation, bearer token injection only for `https://*.dev.clock-p.com`, and public URL mismatch rejection.
  - Real public smoke test must hit the real `https://*.dev.clock-p.com` helper2 URL, not localhost or a mock. Public `/status` and MCP `initialize` must work with a valid bearer token and fail with missing or invalid bearer token.
  - Real public route matrix: using `.clock-p/session.json` whose `helper.base_url` is the real public helper2 URL, run status, mode, play, stop, screenshot, runtime-log read, and helper2 MCP status/play/stop/mode/screenshot/runtime-log through helper2.
  - Public route matrix must use the same task-scoped semantics as local mode. It must not revive old hub/task-server/helper1/mcp1/runtime-log routing.
  - Phase 16 cannot pass with only public `/status` and MCP `initialize`.

### Phase 17: Deprecated

- Phase 17 as a standalone "Multi-task Hardening" phase is deprecated.
- Do not implement or validate a separate Phase 17.
- The work that was previously listed under Phase 17 has been moved back into the proper earlier phases:
  - task-agent/Rojo process lifecycle belongs to Phase 9
  - multi-task desired state and lease/release isolation belong to Phase 10
  - task-scoped mcp2/Studio API isolation belongs to Phase 11
  - Studio-side Rojo routing belongs to Phase 12
  - Studio-side runtime log upload and task isolation belong to Phase 13
  - descriptor-driven bridge script behavior belongs to Phase 14
  - helper2 MCP task routing belongs to Phase 15
  - public helper2 route parity belongs to Phase 16
- A later hardening phase may be created only after Phases 9-16 have passed their own test gates. It must not be used to hide incomplete functionality from earlier phases.

### Phase 18: Ephemeral Official CLI Generation Bridge

- Add a task-scoped helper2 bridge for selected official Roblox Studio CLI generation capabilities.
- Do not run a persistent official CLI sidecar. Each helper2 request starts one fresh `StudioMCP.exe` process, binds it to the request's task-owned Studio, performs exactly one allowed action, returns the CLI result or CLI error upstream, and then closes the CLI process.
- The helper2-owned CLI process must be tied to the helper2/request lifetime. If helper2 exits or the upstream HTTP/MCP caller disconnects before a response can be returned, helper2 must kill that request's CLI process.
- Do not add a helper2 concurrency lock around official CLI requests. Concurrent requests start independent CLI processes. If the official CLI or Studio reports a concurrent-use error, return that error upstream instead of serializing or retrying inside helper2.
- Do not add helper2-defined business timeouts for official CLI startup, initialization, studio listing, active-studio binding, generation, or job waiting. Trust the official CLI to return success or a structured error. Upstream caller cancellation still cancels and kills the request's CLI process.
- The only allowed actions in the first version are:
  - `generate_mesh`
  - `store_image`
  - `generate_procedural_model`
  - `wait_job_finished`
- Do not expose a generic tool-name passthrough. Helper2 must provide strongly typed Go request and response structs for each allowed action and map them internally to the official CLI JSON-RPC tool call.
- Use the official CLI protocol shape:
  - `initialize`
  - `notifications/initialized`
  - `tools/list` only for diagnostics or schema verification, not for exposing arbitrary tools
  - `tools/call list_roblox_studios`
  - `tools/call set_active_studio`
  - `tools/call <allowed generation action>`
- Official CLI `tools/call` returns the MCP tool response envelope. Helper2 must decode `isError` plus `content[]`; when a tool's business payload is JSON text inside `content[].text`, parse that text explicitly. Do not parse `list_roblox_studios` or generation business fields directly from the top-level JSON-RPC `result`.
- Helper2's public Go structs may use idiomatic field names, but the internal official CLI argument mapping must use the official camelCase fields:
  - `generate_mesh`: `textPrompt`, optional `size {x,y,z}`, optional `maxTriangles`
  - `store_image`: `filePath` after any helper2 base64-to-temp-file conversion
  - `generate_procedural_model`: `prompt`, optional `attachedImageUri`
  - `wait_job_finished`: `generationId`, optional `timeout` only when the upstream request explicitly provides it
- Bind the official CLI to Studio by `place_id` only, because `list_roblox_studios` exposes `place_id` plus `studio_id` but not helper2's managed Studio PID.
- For a task request, helper2 first resolves the live task session and task-owned managed Studio, then reads the task's `place_id`.
- After `list_roblox_studios`, the task `place_id` must match exactly one listed Studio:
  - zero matches returns `cli_studio_not_found`
  - more than one match returns `cli_studio_ambiguous`
  - exactly one match allows `set_active_studio(studio_id)`
- Same-place multi-open Studio is legal for helper2, but official CLI generation is unavailable in that state. Do not guess by window title, active flag, launch order, PID, or helper2 desired target.
- `store_image` may accept either a Windows helper-local absolute image path or base64 image bytes with MIME type. If base64 is accepted, helper2 writes a task-scoped temporary file, passes its local path to official CLI `store_image`, and removes the temporary file after the request completes or fails.
- Return official CLI tool success as a successful helper2 response with the raw official CLI tool result preserved in a typed `result` field.
- Return official CLI `isError`, JSON-RPC errors, binding errors, process start errors, or process exit errors as helper2 error responses with typed error codes and the official CLI error content preserved where available. Do not convert CLI business errors into helper2 retries or alternate fallback behavior.
- Add task `/status` observability for official CLI generation that shows only safe operational counters such as active request count, last action, and last error code. Do not log or expose prompts, image bytes, image paths beyond necessary debug-safe summaries, tokens, or full raw request bodies.
- Verify with fake CLI process tests before real Studio tests:
  - allowed actions use `initialize -> list_roblox_studios -> set_active_studio -> action -> cleanup`
  - unsupported actions are rejected before starting CLI
  - CLI `isError` returns upstream and still cleans up
  - JSON-RPC/process errors return upstream and still cleans up
  - upstream disconnect kills the request CLI process
  - helper2 shutdown or crash kills any in-flight request CLI process through helper-owned process cleanup
  - zero and multiple same-place Studio matches return the documented binding errors
  - concurrent requests start independent CLI processes without a helper2 lock
  - status and logs do not contain prompt text, image bytes, token-like strings, or full raw request bodies
- Verify with real Studio only after fake CLI coverage passes:
  - `list_roblox_studios` plus `set_active_studio` binds to the task's Studio when the task `place_id` appears exactly once
  - same-place multi-open returns `cli_studio_ambiguous`
  - at least one real allowed generation action returns through helper2 and closes the CLI process afterward

### Phase 19: bridge2 本地 CLI 与编辑态原语

- Phase 19 单独承载 bridge2、本地 CLI 编排、编辑态代码执行与日志读取；不塞回 Phase 14 / 15 / 18。
- 独立设计稿见 `pr-list/bridge2-helper2-edit-official/design.md`。后续实现以该 design 为权威入口，本长路线图只保留摘要。
- Phase 19 拆分为：
  - Phase 19A：本地 bridge2 CLI、`ensure-edit`、现有 helper2 task API 薄封装、`run-code-direct`、`run-code`、`play-mode-logs`。
  - Phase 19B：等 Phase 18 helper2 官方原语落地后，bridge2 再接 official 命令。
  - Phase 19C：helper2 Windows 本机 `read-studio-log`。
- Phase 19A 只做本地验证，不做公网路由验收。
- Phase 19A 新增 `tools/bridge2`、`clockp-roblox-cli.cmd`、`clockp-roblox-cli.sh`。bridge2 只读 `.clock-p/session.json`，不读本机 `machine_name` 文件，不猜 helper。
- bridge2 所有命令只输出 JSON：stdout 只输出一个 JSON 对象，stderr 默认空；argparse 参数错误、help 和异常都必须转成 JSON 失败结果。
- helper2 只暴露稳定原语。`ensure-edit -> run-code-direct` 这种流程编排放在 bridge2 Python 侧，不放进 helper2 primitive handler。
- Phase 19A 新增编辑态 helper2 / mcp2 原语：
  - `POST /session/{task_id}/studio/run-code-direct`
  - helper2 MCP tool `helper2_studio_run_code`
  - mcp2 command kind `studio_run_code`
- `*-direct` 命令绝不停止 Studio、等待 edit 或做流程编排。当前 task-bound mcp2 模式不是 `edit` 时直接失败。
- Phase 19A bridge2 子命令行为：
  - `status`、`mode`、`play`、`stop`、`screenshot`：不做 `ensure-edit`，只薄封装现有 helper2 task-scoped API。
  - `run-code-direct`：不做 `ensure-edit`，只调用 helper2 direct 原语。
  - `run-code`：先 `ensure-edit`，再调用 `run-code-direct`。
  - `play-mode-logs`：不做 `ensure-edit`，可以调用 helper2 现有 task-scoped log API，但不得暴露旧 `runtime-log` CLI 名称。
- `ensure-edit` 是 bridge2 子命令 / Python 工具函数，不是 CLI 全局选项。每个子命令必须显式决定是否调用它，CLI 层默认值不能隐式增加或移除编辑态编排。
- `ensure-edit` 只使用当前 helper2 task-scoped 原语：
  - 读取 `.clock-p/session.json`
  - 要求 helper2 task status 为 live
  - 查询 `/session/{task_id}/studio/mode`
  - 如果 mode 是 `edit`，直接成功
  - 如果 mode 是 `play_server`，调用 `/session/{task_id}/studio/stop`，然后轮询 mode 直到 `edit`
  - 如果 mode 是 unavailable、unknown、play_client 或任何未支持状态，返回结构化错误，不猜测
  - stop 返回成功不代表已进入 edit；stop 后必须重新查到新鲜的 `edit` mode 才算成功
- `ensure-edit` 成功结果必须用 `details.reason` 表示 `already_edit` 或 `stopped_play`，并在 `details.last_mode` 带上最后一次 mode payload。
- `run-code-direct` 与 `run-code` 都支持 `--code` 与 `--file`，必须且只能提供一个。
- `run-code-direct` 及其 mcp2 命令应保留历史安全意图，但不能依赖旧代码：任意 Luau 中禁止 Studio 生命周期控制 API，例如 `StudioTestService`、`ExecutePlayModeAsync`、`ExecuteRunModeAsync`、`EndTest`。这是本地可信场景下的危险调用拦截，不是强安全沙箱。
- 面向 CLI 的日志命令改名为 `play-mode-logs`。不要把 bridge2 命令叫做 `runtime-log`。当前语义是“LLM 从 helper2 读取 task / play 日志”；文档不得描述 bridge2 对接独立 runtime-log server。`play-mode-logs` 可以调用 helper2 内部现有 task-scoped log API。
- Phase 19A 不实现旧 marketplace 关键词式 `insert_model`，也不实现 official 命令。official 命令等 Phase 19B，Creator Store search / insert 需要先在 Phase 18B 或后续 official Creator Store phase 中定 helper2 原语。
- `read-studio-log` 是 Phase 19C 能力，不进入 Phase 19A。
- Test Gate:
  - Python 单测覆盖 bridge2 session 读取、JSON-only 成功 / 失败输出、stderr 默认为空、命令路由、`--code` / `--file` 互斥，以及不 fallback 到 helper1 / mcp1 / 旧 hub / 旧独立 runtime-log server。
  - Python 单测覆盖 `ensure-edit`：already-edit、play-server-stop-then-edit、stop 超时、unknown / unavailable mode、stale task。
  - Go 单测覆盖 helper2 direct 原语拒绝非 edit 模式，并要求显式 task-bound mcp2 路由。
  - Go 单测覆盖 helper2 MCP `helper2_studio_run_code` 的 tools/list schema、必填 `task_id` / `code`、无旧 `run_code` 别名。
  - 真实 Studio 本地测试：`status`、`mode`、`play`、`stop`、`screenshot` 通过 bridge2 调现有 helper2 task API。
  - 真实 Studio 本地测试：`run-code-direct` 在 edit 模式成功，并返回 print / return 输出。
  - 真实 Studio 本地测试：`run-code` 在 play 模式下先停止、通过新鲜 mode 查询等待 edit，再执行代码。
  - 真实 Studio 本地测试：`run-code-direct` 在 play 模式失败，且不会停止 Studio。
  - 真实 Studio 本地测试：helper2 MCP `helper2_studio_run_code` 在 edit 模式返回结构化结果。
  - 真实 Studio 本地测试：`play-mode-logs` 从 helper2 读取 task/play 日志，不使用旧独立 runtime-log server。
