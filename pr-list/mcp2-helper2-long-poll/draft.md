# mcp2 / helper2 long-poll command channel draft

## Objective

This draft defines the first mcp2 <-> helper2 command channel.

The channel is intentionally minimal:

- mcp2 pulls one command at a time from helper2.
- helper2 returns a command only in response to a pull.
- mcp2 reports command results back with an explicit `command_id`.
- mcp2 never runs the command loop in the play client VM.
- a Studio mode lifecycle change invalidates all queued work in the current global queue. After the later task-scoped command channel exists, the same lifecycle change invalidates only that task's queued work.
- helper2 recovers a managed Studio if the mcp2 channel becomes stale.

hub and task-server are outside the current PR scope. This draft includes the first formal Studio control command kinds needed to test edit/play lifecycle switching. Later hubless task-agent, Rojo, runtime-log, and public-exposure work is captured separately under Roadmap.

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

The protocol relies on Studio's VM ownership behavior: the edit VM is suspended while Studio is in play, and the play VM is independent from the edit VM. Because of that, helper2 does not try to merge or arbitrate simultaneous edit/play activity. A pull or ping from a different `mode_seq` means helper2 has observed a different active VM lifecycle and should switch to it.

If mcp2 catches an unexpected command execution error while processing a delivered command, it must report that command result with the current `mode_seq` first, so helper2 can complete the matching `waiting_response_command`. After helper2 explicitly records that result, mcp2 should advance `mode_seq` before the next pull. This creates a new helper2 lifecycle boundary for the still-running plugin loop without making the error result look stale. For play/stop Studio API failures that happen after the pre-action command result was already recorded, mcp2 logs the later failure and advances `mode_seq`; it must not try to mutate the already-completed command result.

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

helper2 returns HTTP 200 when the result payload has been acknowledged by the HTTP endpoint. HTTP 200 does not mean the command was completed. The response body must distinguish this with a recorded flag:

```json
{
  "ok": true,
  "command_id": 123,
  "recorded": true,
  "ignored": false
}
```

Before completing `waiting_response_command`, helper2 must validate that the request comes from the current helper-managed Studio connection, that the canonical Studio PID matches the active channel PID, that `mode_seq` matches the active lifecycle, and that `command_id` matches the current waiting response command. Invalid, stale, unmatched, or wrong-lifecycle results are acknowledged with HTTP 200 and logged as ignored with `recorded=false`; they must not clear `waiting_response_command`, wake command waiters, or call completion callbacks. When this draft says mcp2 must wait for helper2 to acknowledge or record a command result, it means mcp2 must receive `recorded=true`, not merely HTTP 200.

mcp2 must classify `recorded=false` before deciding whether to retry the same result:

```text
retryable:
  invalid_helper_ack_body
  empty/unknown transient helper response

terminal for the delivered command:
  invalid_lifecycle_source
  studio_pid_mismatch
  mode_seq_mismatch
  not_waiting_for_command
```

`invalid_helper_ack_body` means helper2 returned an HTTP success but mcp2 could not parse the acknowledgement body or the body was missing required acknowledgement fields. For retryable cases, mcp2 backs off and reposts the same result. For terminal cases, helper2 has already decided the delivered command can no longer be completed in the current lifecycle. mcp2 must stop the command heartbeat, must not invoke any play/stop Studio transition for that command, should advance `mode_seq` to create a fresh lifecycle boundary, and should return to the pull loop. This prevents a plugin from waiting forever for `recorded=true` after helper2 has already cleaned or superseded the command.

### Debug Command Enqueue

`POST /debug/command/{placeid}` is a temporary local development endpoint. It inserts a simple debug command into the single global queue for the requested place.

This endpoint exists only to test the helper2/mcp2 command channel before hub/task-server integration exists.

If helper2 is in `wait_init_mode`, the endpoint fails immediately because there is no initialized mcp2 execution lifecycle.

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
- exists to let upstream callers verify Studio mode after play/stop, because play/stop command success means the command was accepted before the Studio transition, not that the final mode has already been observed.
- waits at most 5 seconds for an available mcp2 VM. If no VM is available during that short window, it should return an unknown/unavailable mode result instead of blocking for the full play/stop verification window.

For `studio_play` and `studio_stop`, mcp2 must send `response_result` and receive helper2's explicit `recorded=true` confirmation before invoking the Studio transition call. This pre-action result means "the command was accepted and the Studio transition is about to be requested." It is not proof that the final Studio mode has changed. If helper2 returns HTTP 200 with `recorded=false`, mcp2 must not start the transition. Retryable `recorded=false` reasons may be retried with backoff; terminal `recorded=false` reasons end the delivered command and return mcp2 to the pull loop as described in Response Result. After receiving command-level success, upstream LLM or bridge code should verify final state through `studio_mode_query` or task status.

The debug play/stop entrypoints are enqueue-only and do not serialize around a pending Studio mode transition. If a stop command is consumed by edit mode during a play transition, the command may return `already_stopped`; this is acceptable for this phase because the endpoint result is not the authoritative final Studio state. The invariant for this phase is narrower: mixed or concurrent play/stop requests must not poison the helper2 queue, mcp2 loop, or later fresh play/stop commands.

mcp2 must wrap Studio control calls such as `StudioTestService:ExecutePlayModeAsync({})` and `StudioTestService:EndTest({})` so Studio errors do not kill the plugin loop. For ordinary commands, such errors become command failures reported with the current `mode_seq`; after helper2 records the failure, mcp2 bumps `mode_seq` before the next pull. For `studio_play` and `studio_stop`, the command has already completed at pre-action `recorded=true` time; a later Studio API error should be logged and reflected through task status or later mode verification failure rather than mutating the already-completed command callback. After such an error, mcp2 must bump `mode_seq` and continue with the next pull using the new `mode_seq`.

### Command Completion and Cleanup Callbacks

Every formal command should have a command timeout and a completion callback. The callback usually resolves upstream state, for example an HTTP waiter or bridge-script response. helper2 must call the callback exactly once when the command reaches a terminal state.

The callback is not tied only to `response_result`. It must also run when helper2 clears a command from task-scoped queue or waiting-response state.

Callbacks are in-memory helper2 functions in the first implementation. The design does not persist command operations. This is acceptable because helper2 is expected to restart rarely, and helper2 restart should terminate helper-owned Studio processes before fresh task-agent heartbeats recreate new task-owned Studio targets.

Default command timeouts:

```text
queued timeout before delivery: 30 seconds
ordinary response timeout after delivery: 60 seconds
play/stop final mode observation timeout: 20 seconds
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

For `studio_play` and `studio_stop`, command-level success is the pre-action `response_result` recorded by helper2 with `recorded=true` before mcp2 invokes the Studio transition call. Lifecycle cleanup of a delivered play/stop command before that pre-action recorded result is cancellation or timeout, not proof of final Studio mode. Upstream callers should verify final mode separately through `studio_mode_query` or task status, using the 20 second play/stop final mode observation timeout as their verification bound.

If the managed Studio process exits, is manually killed, or is replaced/restarted, helper2 treats the old Studio execution environment as gone. Any queued or waiting-response commands bound to that task's old execution environment must be cleaned and completed before the replacement Studio can receive new commands.

If a late `response_result` arrives after cleanup made the command terminal, helper2 should acknowledge it, log that it was stale, and not call the callback again.

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

`GET /debug/studio/mode/{placeid}` enqueues a `studio_mode_query` command, waits at most 5 seconds for the matching `response_result`, and returns the observed mcp2 mode when available:

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

If no mcp2 VM answers inside 5 seconds, the endpoint returns `available=false` and does not claim final Studio state.

The debug play/stop HTTP endpoints do not wait for Studio to enter play or stop. The debug mode endpoint exists only to make Phase 7 final-mode verification testable before task-scoped APIs exist.

Formal bridge/MCP play and stop entrypoints should wait for the command callback before returning to upstream. For `studio_play` and `studio_stop`, that callback is completed by the pre-action `response_result` recorded with `recorded=true` before mcp2 invokes the Studio transition call. Therefore a successful formal play/stop response means the transition request is about to be issued; the upstream LLM must still verify final Studio mode with `studio_mode_query` or task status.

## mcp2 Loop

mcp2 runs a single sequential loop:

```text
pull_command
  command:
    execute command
    post response_result
    wait for recorded=true or terminal recorded=false
    if terminal recorded=false:
      stop command heartbeat
      bump mode_seq
      pull again without executing post-result transition work
    pull again

  should_restart_pull:
    pull again immediately

  HTTP failure or server-side close:
    retry according to failure type
```

The generic loop above describes ordinary commands. For `studio_play` and `studio_stop`, command processing must post `response_result` and receive `recorded=true` for the pre-action result before invoking the Studio transition call, because the VM may hang or close during the transition. If the pre-action result ends with terminal `recorded=false`, mcp2 must not invoke the Studio transition call.

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

While in `wait_init_mode`, any attempt to enqueue a command fails immediately. Commands must not be queued before a valid mcp2 VM has pulled.

### Pull With Current Lifecycle

If an incoming pull has the current `active_mode_seq`, helper2 accepts it as the current `waiting_pull`.

If a command is already queued, helper2 returns the command immediately.

If no command is queued, helper2 holds the pull for up to 10 seconds.

### Pull With New Lifecycle

If an incoming pull has a different `mode_seq` from `active_mode_seq`, helper2 performs an atomic lifecycle switch:

```text
clear queue
clear waiting_response_command
close old waiting_pull directly
active_mode = incoming mode
active_mode_seq = incoming mode_seq
active_studio_pid = pid resolved from the incoming HTTP connection, if available
last_pull_at = now
```

The old `waiting_pull` is closed by helper2 as a connection close, not by returning `should_restart_pull`.

This close path must be tested against Roblox `HttpService:RequestAsync`; mcp2 must treat it as a normal reconnect condition.

### Ping With Current Lifecycle

If `only_ping=1` and the incoming `mode_seq` matches the current lifecycle, helper2 updates `last_pull_at`, records the peer PID if available, and returns `type=pong` immediately.

If `only_ping=1` carries a new `mode_seq`, helper2 may treat it as a lifecycle switch before returning `pong`, but only after the request is accepted as coming from a currently managed or otherwise valid Studio connection for the same task/place context. A random local HTTP request must not be able to initialize lifecycle state, refresh liveness, create a hanging pull, clear queues, or switch lifecycle state by inventing or replaying a `mode_seq`.

If helper2 cannot validate the pull or ping source as a managed Studio connection, it should reject the HTTP request instead of returning `should_restart_pull`. This lets mcp2 use its existing request-failure backoff path and avoids a hot pull retry loop when PID or process-group resolution temporarily fails.

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
kill the matching managed Studio process immediately
```

The restart reason is not the kill operation. Restart happens because StudioManager owns desired Studio targets.

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
waiting_pull_count
waiting_response_command
active_studio_pid
```

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

This draft does not add desired-mode reconciliation, last-writer-wins semantics, or timeout-based Studio kill recovery for play/stop. Concurrent play/stop requests may individually succeed or fail. The required invariant is that the command channel and plugin mode state remain usable afterward.

## Roadmap: Hubless Session Direction

This section is a roadmap for later PRs, not part of the Phase 1-8 helper2/mcp2 delivery. It is kept here because the Phase 1-8 protocol choices must not block the hubless direction.

The next architecture direction removes hub as a routing and ownership authority. The system keeps `task_id`, but narrows its meaning:

```text
task_id = workspace-local session identity
machine_name = explicit Windows helper2 target
helper2 = Studio/session authority
task-agent = workspace/Rojo/session lease agent
```

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

`POST /session/{task_id}/studio/play` and `POST /session/{task_id}/studio/stop` wait for the in-memory command callback. For play/stop, success means pre-action acceptance by mcp2, not final mode. The response should include:

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
- 60 second stale detection kills only the matching managed Studio and StudioManager restarts it because the desired target remains.
- `/debug/studio/play/{placeid}` enqueues `studio_play` and returns without waiting for play completion.
- `/debug/studio/stop/{placeid}` enqueues `studio_stop` and returns without waiting for stop completion.
- `edit + studio_stop` returns success with an `already_stopped` result.
- `play_server + studio_play` returns failure with an error that clearly identifies `already_playing`.
- concurrent play/stop requests may produce mixed command results, but afterward a fresh play or stop command must still leave mcp2 in a coherent mode and helper2 with no stale queue or waiting-response residue.

### Roadmap Test Layers

The roadmap phases should use these layers. They are not Phase 1-8 acceptance requirements. Do not start with full Studio integration when a state-machine or process-level fake can prove the same behavior faster.

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

### Roadmap Test Gates

Each gate below is a must-have passing condition for the relevant roadmap phase before moving to the next roadmap gate. These gates are not current Phase 1-8 acceptance requirements.

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
- `response_result` from an invalid source, wrong canonical Studio PID, wrong `mode_seq`, or non-waiting `command_id` is HTTP-acknowledged but ignored and must not complete a command.
- Cross-task queues, waiting pulls, waiting responses, active modes, and command ids do not contaminate each other.

Command callback and cleanup gate:

- Every formal command has queued timeout, response timeout, and exactly-once in-memory callback.
- Callback fires exactly once for normal response.
- Callback fires exactly once for lifecycle cleanup.
- Callback fires exactly once for task release and task lease expiry cleanup.
- Callback fires exactly once for queued timeout and response timeout.
- Callback fires exactly once when Studio exits, is manually killed, or is replaced/restarted.
- Callback fires exactly once on helper2 shutdown.
- Late `response_result` after terminal state is acknowledged, logged as stale, and does not call the callback again.
- Cleanup is tested while command is queued, while command is waiting response, after play/stop pre-action `recorded=true`, and during stale recovery.
- Replacement Studio never consumes old queued or waiting-response commands.

Task-scoped Studio API gate:

- `POST /session/{task_id}/studio/play` waits for command callback and returns pre-action acceptance, not final mode proof.
- `POST /session/{task_id}/studio/stop` waits for command callback and returns pre-action acceptance, not final mode proof.
- mcp2 posts play/stop pre-action `response_result` and receives helper2 `recorded=true` before invoking Studio transition calls.
- Later Studio API error after pre-action `recorded=true` is reflected through logs/status/mode verification failure and does not mutate the completed command callback.
- `GET /session/{task_id}/studio/mode` returns edit mode in edit VM.
- `GET /session/{task_id}/studio/mode` returns play_server mode in play server VM.
- If no VM is available, mode query returns `available=false` and `mode=unknown` within 5 seconds.
- Upstream play/stop verification polls mode or task status for at most 20 seconds.
- Repeated play -> stop -> play -> stop works through task-scoped APIs.
- Concurrent play/stop may produce mixed command outcomes, but afterward a fresh play or stop works and task status has no stuck queue, stale waiting response, or incoherent active mode.
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

## Implementation Phases

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
- Resolve the peer PID for pull and ping requests using the helper1 TCP owner lookup approach.
- Add `active_studio_pid` to `/studio/summary`.
- Verify a long-running debug command can keep the channel fresh with pings.

### Phase 5: Stale Recovery

- Add `last_pull_at` and 60 second stale detection.
- Run a watchdog every 5 seconds.
- On stale, clear mcp2 channel state and close `waiting_pull`.
- Kill only the matching managed Studio process.
- Let StudioManager restart only because the desired target remains desired.
- Verify by preventing pull traffic or killing the mcp2 side and watching helper2 recover the desired Studio target.

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
- Implement mcp2 `studio_play` in edit mode by posting pre-action `response_result`, receiving helper2 `recorded=true`, then calling `StudioTestService:ExecutePlayModeAsync({})`.
- Implement mcp2 `studio_stop` in play_server mode by posting pre-action `response_result`, receiving helper2 `recorded=true`, then calling `StudioTestService:EndTest({})`.
- Implement mcp2 `studio_mode_query` to return current mcp2 mode, `mode_seq`, and RunService flags.
- Make `edit + studio_stop` return success with `already_stopped`.
- Make `play_server + studio_play` return failure with an `already_playing` error.
- Treat play/stop command success as pre-action acceptance, not final mode proof.
- Verify final play/stop mode through `studio_mode_query` or task status after command success.
- Mark commands canceled or timed out when queue/waiting-response cleanup removes them before terminal response.
- Acknowledge but ignore late `response_result` after a command is already terminal.
- Clean and callback commands when the managed Studio process exits, is manually killed, or is replaced/restarted.
- Run repeated play -> stop -> play -> stop tests locally.
- Run concurrent play/stop enqueue tests locally.
- After concurrency tests, verify a fresh play or stop command still works and `/studio/summary` has no stuck queue, no stale waiting response command, and a coherent `active_mode`.
- Verify callbacks fire exactly once for normal response, lifecycle cleanup, task cleanup, Studio process replacement, timeout, and late response.
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

## Roadmap Phases

The phases below are future roadmap phases. They are not part of the current helper2/mcp2 PR acceptance boundary.

### Roadmap Phase 9: Task Agent Local Session and Rojo

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
- Verify local start, live-agent replacement, stale-session cleanup, Rojo restart on the same upstream, and config-mismatch behavior without Roblox Studio.

### Roadmap Phase 10: helper2 Task Session and Desired Studio

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
- On task session release or lease expiry, first atomically remove the task-owned desired target, task session, queues, Studio bindings, and task-scoped state from helper2 maps.
- After the atomic removal, kill Studio processes bound to that task.
- Do not auto-restart Studio after task-agent release or lease expiry.
- Verify heartbeat registration, helper2 restart/re-heartbeat, desired Studio creation, release, stale expiry, atomic cleanup, and stale kill with local helper2.
- Verify helper2 restart relies on helper-owned process cleanup, then restores task-owned desired Studio through the next task-agent heartbeat.

### Roadmap Phase 11: Task-scoped mcp2 and Studio APIs

- Scope mcp2 command channel state by `task_id`; do not share queue, active mode, waiting pull, or waiting response command across tasks.
- Route hubless mcp2 pull and response traffic by Studio PID binding to `task_id`, not by a plugin-supplied task hint.
- Add the minimal task-scoped helper2 Studio API paths:
  - `POST /session/{task_id}/studio/play`
  - `POST /session/{task_id}/studio/stop`
  - `GET /session/{task_id}/studio/mode`
  - `GET /session/{task_id}/studio/screenshot`
- Make task-scoped play/stop wait for the in-memory command callback and return pre-action acceptance, not final mode proof.
- Make task-scoped mode query wait at most 5 seconds and return `available=false`, `mode=unknown` when no VM is available.
- Verify final play/stop mode by polling task-scoped mode or task status for at most 20 seconds.
- Promote screenshot from debug place routing to task-scoped Studio PID routing.
- Verify repeated play -> stop -> play -> stop through task-scoped APIs.

### Roadmap Phase 12: Local Rojo Through helper2

- Add helper2 local Rojo proxy endpoints for Studio Rojo plugin traffic, following helper1's config endpoint plus HTTP/WebSocket forwarding shape.
- Rojo plugin connects only to local helper2.
- helper2 resolves plugin connection PID to `studio_pid -> task_id`.
- helper2 proxies to the task-bound Rojo upstream reported by task-agent heartbeat.
- Reject Rojo plugin traffic when no task binding exists.
- Do not fallback from `place_id` to `task_id`.
- Verify a local Rojo sync path through helper2 with one task.

### Roadmap Phase 13: Runtime Logs via helper2

- Remove runtime-log server from the local hubless path.
- Runtime sends logs to local helper2 through `POST /runtime-log/upload`.
- helper2 persists logs by `task_id` and runtime identity under a helper-owned local directory.
- task-agent or bridge scripts pull logs from helper2 by `GET /session/{task_id}/runtime-log`.
- Bind incoming runtime log writes through helper2's connection/PID-to-task mapping when the sender is Studio-side; task-agent-side reads use explicit `task_id`.
- Define minimal runtime log body fields: `runtime_id`, `mode`, `timestamp_ms`, `level`, `message`, and `fields`.
- Add a bounded per-task retention policy by size or line count.
- helper2 cleans helper-owned temporary log files best-effort on shutdown.
- Add slow upload or no-result logging only inside helper2 where the result can be observed.
- Verify runtime log write and read locally without runtime-log server.

### Roadmap Phase 14: Bridge Scripts Read Session Defaults

- Update launch, stop, screenshot, log, and MCP scripts to read `.clock-p/session.json`.
- Scripts use `helper.base_url` plus the documented task-scoped helper2 path, such as `/session/{task_id}/...`.
- Use the minimal task-scoped helper2 API paths for play, stop, mode, screenshot, status, and runtime log reads.
- Scripts must not create sessions, choose machines, scan helpers, or fallback to historical helpers.
- If session descriptor is missing, fail with a clear task-agent start instruction.
- If helper2 reports `task_not_registered` or stale task, fail with a clear task-agent restart instruction.
- Verify local launch/stop/status/log calls use the local helper endpoint from the descriptor.

### Roadmap Phase 15: helper2 MCP Server Replacement

- Move the public MCP surface into helper2.
- Keep mcp2 plugin on localhost helper2.
- Route MCP commands by explicit `task_id`.
- Enforce `task_id -> studio_pid` binding for Studio-affecting commands.
- Remove dependency on the old Linux MCP server in local hubless mode.
- Verify MCP status, play, stop, screenshot, and log tools through helper2 locally.

### Roadmap Phase 16: Public Exposure

- Add helper2 public exposure through clockbridge/https-proxy.
- Prefer embedding clockbridge agent logic as a Go library after lifting it out of `internal`.
- Keep shelling out to `clockbridge-cli` only as an optional early-development fallback.
- Add public `helper.public_url` to `.clock-p/session.json`.
- Expose Rojo with a `task_id`-based public domain or path and report that URL as `rojo.upstream_url`.
- Keep Studio-side plugins on local helper2 even in public mode.
- Verify the same bridge scripts work with local IP/port mode and public mode by changing only session descriptor endpoints.

### Roadmap Phase 17: Multi-task Hardening

- Exercise helper2 with multiple registered task sessions.
- Verify task-agent status ports, Rojo upstreams, Studio PID bindings, runtime logs, and MCP calls remain task-scoped.
- Verify one task lease expiry kills only that task's Studio processes.
- Verify lease expiry does not kill same-place Studio processes from other tasks or manually opened unbound Studio processes.
- Verify Rojo plugin traffic cannot cross tasks.
- Keep single-task as the primary supported workflow until multi-task tests are stable.
