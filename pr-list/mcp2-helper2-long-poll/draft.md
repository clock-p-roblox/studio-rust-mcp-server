# mcp2 / helper2 long-poll command channel draft

## Objective

This draft defines the first mcp2 <-> helper2 command channel.

The channel is intentionally minimal:

- mcp2 pulls one command at a time from helper2.
- helper2 returns a command only in response to a pull.
- mcp2 reports command results back with an explicit `command_id`.
- mcp2 never runs the command loop in the play client VM.
- a Studio mode lifecycle change invalidates all queued work.
- helper2 recovers a managed Studio if the mcp2 channel becomes stale.

hub and task-server are outside this draft. This draft includes the first formal Studio control command kinds needed to test edit/play lifecycle switching.

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

## Lifecycle Identity

Each mcp2 process creates a numeric `mode_seq` at startup. The value should be millisecond-level startup time, for example `DateTime.now().UnixTimestampMillis`.

`mode_seq` is the lifecycle boundary used by helper2. `mode` is carried for diagnostics and logs.

When Studio enters play, the play server VM has a new `mode_seq`. When Studio returns to edit, the edit VM might reuse its previous process and therefore keep its old `mode_seq`. helper2 must still treat a newly observed `mode_seq` from another VM as a lifecycle switch.

## Protocol Endpoints

helper2 exposes:

```text
GET  /plugin/mcp2/pull_command
POST /plugin/mcp2/response_result
POST /debug/command/{placeid}
POST /debug/studio/play/{placeid}
POST /debug/studio/stop/{placeid}
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

helper2 returns HTTP 200 when the result has been received. The first implementation keeps stale or unmatched results simple: helper2 may acknowledge them and log that they were ignored.

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

Formal Studio control command results may be lost because successful play/stop changes the active Studio VM. helper2 must not treat the absence of `response_result` as proof that the command failed.

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

These debug HTTP endpoints do not wait for Studio to enter play or stop.

## mcp2 Loop

mcp2 runs a single sequential loop:

```text
pull_command
  command:
    execute command
    post response_result
    wait for HTTP 200
    pull again

  should_restart_pull:
    pull again immediately

  HTTP failure or server-side close:
    retry according to failure type
```

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

`next_command_id` is process-local and globally incrementing. It does not need persistence because helper2 kills Studio processes it started when helper2 exits.

Queued commands are typed internally:

```text
MCP2Command
  command_id
  type = command
  kind = debug_echo | debug_stop_loop | studio_play | studio_stop
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

If `only_ping=1` carries a new `mode_seq`, helper2 treats it as a lifecycle switch before returning `pong`.

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

## Other helper2 Interfaces

The helper2 HTTP surface for this draft is:

```text
GET  /healthz
GET  /plugin/mcp2/pull_command
POST /plugin/mcp2/response_result
POST /debug/command/{placeid}
POST /debug/studio/play/{placeid}
POST /debug/studio/stop/{placeid}
GET  /studio/summary
POST /debug/start-roblox-studio/{placeid}
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

The PID is diagnostic in early phases and becomes the kill target for stale recovery when it maps to a managed Studio.

## Implementation Boundary

This draft adds the first formal Studio control command kinds, `studio_play` and `studio_stop`, plus temporary debug command kinds.

This draft does not integrate hub or task-server.

This draft does not split command queues by mode.

This draft does not make `response_result` the authority for when the next command may be issued. The plugin decides it is ready by issuing the next `pull_command`.

This draft does not add desired-mode reconciliation, last-writer-wins semantics, or timeout-based Studio kill recovery for play/stop. Concurrent play/stop requests may individually succeed or fail. The required invariant is that the command channel and plugin mode state remain usable afterward.

## Testability

helper2 tests should use local Roblox Studio processes and local helper2 ports. They must not depend on online hub/task-server.

Roblox Studio CLI observations on Windows:

```text
RobloxStudioBeta.exe -task EditPlace -placeId <place_id> -universeId <universe_id>
```

This is stable enough for helper2 startup, restart, stale recovery, and edit-mode mcp2 tests. helper2 already uses this launch shape.

Studio play creates additional Studio processes with CLI tasks such as:

```text
RobloxStudioBeta.exe -task StartServer ...
RobloxStudioBeta.exe -task StartClient ...
```

These are useful test fixtures for observing `play_server` and `play_client`, but they are not the production control protocol in this draft. Their arguments include session-specific values such as edit PID, target id, transport settings, session guid, and user metadata. Tests may use them only as local process fixtures after verifying the required arguments from current Studio behavior.

The test plan must cover:

- edit VM connects, pulls, receives `should_restart_pull`, and continues.
- `/debug/command/{placeid}` enqueues a command and mcp2 returns `response_result`.
- long command execution sends `only_ping=1` and prevents false stale.
- `debug_stop_loop` stops mcp2 pull traffic so stale recovery can be tested without editing Studio manually.
- `play_client` logs mode and does not call helper2.
- `play_server` enters the same sequential pull loop as edit.
- `mode_seq` switch clears the queue and closes the old `waiting_pull`.
- server-side close of a hanging pull is observed by Roblox `HttpService:RequestAsync` as a retryable failure.
- 60 second stale detection kills only the matching managed Studio and StudioManager restarts it because the desired target remains.
- `/debug/studio/play/{placeid}` enqueues `studio_play` and returns without waiting for play completion.
- `/debug/studio/stop/{placeid}` enqueues `studio_stop` and returns without waiting for stop completion.
- `edit + studio_stop` returns success with an `already_stopped` result.
- `play_server + studio_play` returns failure with an error that clearly identifies `already_playing`.
- concurrent play/stop requests may produce mixed command results, but afterward a fresh play or stop command must still leave mcp2 in a coherent mode and helper2 with no stale queue or waiting-response residue.

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
- Use local play/server fixtures or manual Studio Play to verify `edit -> play_server -> edit` lifecycle changes.

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
- Add `studio_play` and `studio_stop` command kinds.
- Add `POST /debug/studio/play/{placeid}` and `POST /debug/studio/stop/{placeid}`.
- Keep both endpoints enqueue-only; they must not wait for final Studio mode.
- Implement mcp2 `studio_play` in edit mode with `StudioTestService:ExecutePlayModeAsync({})`.
- Implement mcp2 `studio_stop` in play_server mode with `StudioTestService:EndTest({})`.
- Make `edit + studio_stop` return success with `already_stopped`.
- Make `play_server + studio_play` return failure with an `already_playing` error.
- Allow play/stop `response_result` to be lost across VM switches.
- Run repeated play -> stop -> play -> stop tests locally.
- Run concurrent play/stop enqueue tests locally.
- After concurrency tests, verify a fresh play or stop command still works and `/studio/summary` has no stuck queue, no stale waiting response command, and a coherent `active_mode`.
