# helper2 Phase 9/10 design

> 阶段文档：本文记录 helper2/task-agent 早期 Phase 9/10 设计。当前操作入口与验收标准以仓库根 README 和 Phase 19 文档为准。
> 当前主线仍是 `task-agent -> helper2 -> mcp2`；旧 hub、task-server、helper1、mcp1 不作为 fallback 或验收依据。

## Scope

Implement the first hubless task-agent/helper2 loop from the mcp2/helper2 draft:

- Phase 9 task-agent local session descriptor, status server, shutdown endpoint, Rojo supervision, and helper heartbeat loop.
- Phase 10 helper2 task session registration, heartbeat refresh, release, status, lease expiry, and task-owned desired Studio targets.

This PR intentionally does not implement task-scoped mcp2 command channels, Rojo proxying, runtime log storage, bridge script migration, public exposure, or MCP server replacement. Those are Phase 11+.

The active mainline for this PR is:

```text
task-agent -> helper2 -> mcp2
```

The old hub, task-server, helper1, mcp1 / old Rust clockp MCP server path, and old runtime-log server are deprecated. They are historical context only and must not be used as compatibility fallback, implementation target, or acceptance gate for Phase 9/10.

## Task Agent

`go-helper/cmd/task-agent` owns a workspace-local `.clock-p/session.json`.

The descriptor stores routing identity and volatile process fields:

- `task_id`
- `machine_name`
- `place_id`
- `task_agent_pid`
- `task_agent_started_at_ms`
- `task_agent_status_url`
- `helper.base_url`
- `rojo.local_url`
- `rojo.upstream_url`

It does not store the workspace path. The local status endpoint reports the workspace path for diagnostics.

`task-agent start` requires explicit `--machine_name` and `--place_id`. In local mode it also requires `--helper-base-url`. In public mode it derives `helper.base_url` from `--machine_name` and the current user identity as `https://roblox-helper-{machine_name}-{user}-user.dev.clock-p.com`. Before starting, it checks an existing descriptor by calling the recorded status URL. If a live matching task-agent answers, it asks it to shut down. If the status URL is unreachable, it treats the old descriptor as stale and replaces it.

The task-agent starts Rojo on a selected local port and reports a stable `rojo.upstream_url` to helper2 in every heartbeat. If Rojo exits while the task-agent is still running, it restarts Rojo on the same URL. Rojo is tied to task-agent lifetime: Windows uses a kill-on-close Job Object, Linux uses process-group shutdown plus parent-death signaling, and other Unix-like hosts use process-group shutdown.

`POST /shutdown` and Ctrl-C share bounded shutdown: best-effort helper2 release, a one second grace window, then Rojo stop. The process should not rely on writing a stopped marker.

## Helper2 Task Sessions

Helper2 adds:

- `POST /session/{task_id}/heartbeat`
- `POST /session/{task_id}/release`
- `GET /session/{task_id}/status`

Heartbeat is registration. The first heartbeat creates a task session; later matching heartbeats refresh it. Immutable fields are:

- `machine_name`
- `place_id`
- `task_agent_pid`
- `task_agent_started_at_ms`
- `rojo.upstream_url`

Changing any immutable field for a live or recoverable expired `task_id` is rejected.

Graceful release removes live state and records an ended tombstone for the helper2 process lifetime. Later heartbeats for that task id are rejected.

Lease expiry happens after 31 seconds without heartbeat. It removes live task state and task-owned desired Studio state, then kills Studio processes bound to that task. It keeps only immutable identity for recovery. A later heartbeat with the same immutable fields can recreate the session and desired Studio target.

## Studio Ownership

Existing debug/manual desired Studio targets continue to work. Phase 10 adds task-owned desired targets keyed by `task_id`. Releasing or expiring one task removes only that task-owned target and kills only Studio processes launched for that task. It must not kill unrelated same-place tasks or manually opened/unbound Studio processes.

The heartbeat handler records the task-owned desired target. Existing periodic reconciliation starts or restarts Studio from desired targets.

## Verification

Required before delivery:

- `go test ./...` under `go-helper`.
- `go build ./cmd/studio-helper ./cmd/task-agent` under `go-helper`.
- Local helper2/task-agent lifecycle probe for start, heartbeat, release, cleanup, and ended-session rejection.
- Review pass 1: Phase 9/10 contract and lifecycle.
- Review pass 2: process cleanup and isolation.
- Review pass 3: API shape, user-facing errors, and test coverage.

Do not run `cargo` validation for this PR unless the task explicitly changes the deprecated Rust binaries.
