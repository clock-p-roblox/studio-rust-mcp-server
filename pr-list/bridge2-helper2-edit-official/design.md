# bridge2 / helper2 编辑态与本地 CLI 设计

## 范围

本文定义新 Roblox agent 主线下一步的本地 CLI 与编辑态能力：

```text
task-agent -> helper2 -> mcp2
```

Phase 19 单独承载 bridge2、本地 CLI 编排、编辑态代码执行与日志读取。Phase 17 已废弃；Phase 18 聚焦 Roblox 官方 CLI generation bridge；Phase 19 不应塞回 Phase 14 / 15 / 18。

Phase 19 拆分为：

- **Phase 19A**：本地 bridge2 CLI、`ensure-edit`、现有 helper2 task API 薄封装、`run-code-direct`、`run-code`、`play-mode-logs`。
- **Phase 19B**：等 Phase 18 helper2 官方原语落地后，bridge2 再接 official 命令。
- **Phase 19C**：helper2 Windows 本机 `read-studio-log`。

本 phase 不再以 helper1、mcp1、旧 Rust clockp MCP server、旧 hub/task-server 路由、旧独立 runtime-log server 作为实现目标、兼容 fallback 或验收依据。旧代码只能用于理解历史操作需求，新 helper2/mcp2 行为必须直接实现。

## Phase 19A 目标

- 新增 `tools/bridge2`，作为新 agent 的本地 CLI 系统。
- 新增极薄启动脚本：
  - `tools/bridge2/clockp-roblox-cli.cmd`
  - `tools/bridge2/clockp-roblox-cli.sh`
- bridge2 只读 `.clock-p/session.json`，不读本机 `machine_name` 文件，不猜 helper。
- helper2 只提供稳定原语，不做跨步骤编排。
- Python 侧负责编排流程、读取 `session.json`、决定哪些命令需要 `ensure-edit`。
- 新增编辑态代码执行能力：`run-code-direct` 与 `run-code`。
- `play-mode-logs` 作为 CLI 日志读取命令名；它可以调用 helper2 现有 task-scoped log API，但不得暴露旧 `runtime-log` CLI 名称。
- Phase 19A 只做本地验证，不做公网路由验收。
- Phase 19A 不验收 Studio 多开场景；多开隔离另行定义，不作为本轮完成条件。

## Phase 19A 非目标

- 不恢复旧 marketplace 关键词式 `insert_model`。
- 不保留旧 MCP 工具别名，例如 `run_code`、`insert_model`、`launch_studio_session`、`start_stop_play`。
- 不提供全局 CLI 选项来隐式控制所有子命令是否进入 edit。
- 不做公网路由测试。
- 不做 Studio 多开测试。
- 不接 official 命令后端；official 入口等 Phase 19B。
- 不实现 `read-studio-log`；该能力等 Phase 19C。
- 不做通用 Roblox 官方 CLI passthrough。

## Phase 19A 架构

### helper2 原语

helper2 只暴露稳定、task-scoped 的原语。一个原语只做一个动作，并返回结构化成功结果或结构化错误，不负责多步骤策略。

Phase 19A 新增编辑态原语：

```text
POST /session/{task_id}/studio/run-code-direct
helper2_studio_run_code
```

请求体：

```json
{
  "code": "print(\"hello\")"
}
```

行为要求：

- 必须显式传入 `task_id`。
- 只解析当前 live task session。
- 要求 task-owned Studio 已存在。
- 要求 task-bound mcp2 当前模式为 `edit`。
- 通过 task-scoped mcp2 command broker 下发 `studio_run_code`。
- 返回 `print`、`warn`、返回值、错误、当前 mode、command id 与终端诊断信息。
- 当前模式不是 `edit` 时快速失败。
- 内部不调用 stop、play，也不等待 edit。

### mcp2 命令

mcp2 需要直接实现 Studio 内部命令，不导入旧插件实现。

Phase 19A 新增命令：

```text
studio_run_code
```

规则：

- 只允许在 edit 模式运行。
- 经过命令体校验后才允许使用 `loadstring`。
- 这是本地可信场景下的危险 Studio 生命周期 API 拦截，不是强安全沙箱。
- 至少禁止：
  - `StudioTestService`
  - `ExecutePlayModeAsync`
  - `ExecuteRunModeAsync`
  - `EndTest`
- 捕获 `print`、`warn`、返回值和异常错误。
- 不调用 Studio play/stop API。
- 除了返回 `response_result`，不改写 helper2 command 生命周期。

### bridge2 Python

建议目录：

```text
tools/bridge2/
  cli.py
  clockp-roblox-cli.cmd
  clockp-roblox-cli.sh
  clockp_bridge2/
    __init__.py
    errors.py
    session.py
    http.py
    studio.py
    commands.py
    output.py
```

职责：

- `session.py`：读取 `.clock-p/session.json`，不猜 machine，也不猜 helper。
- `http.py`：使用 `helper.base_url` 与 `task_id` 调 helper2。
- `studio.py`：实现 `mode`、`wait_mode`、`ensure_edit_mode`。
- `commands.py`：实现 argparse 子命令路由。
- `output.py`：统一 JSON-only 成功与失败输出。

### helper2 MCP

Phase 19A 同步暴露 helper2 MCP 工具：

```text
helper2_studio_run_code
```

参数：

```json
{
  "task_id": "task id",
  "code": "print(\"hello\")"
}
```

规则：

- 不做 `ensure-edit`。
- 不调用 stop。
- 要求 live task、task-owned Studio、task-bound broker 当前为 edit。
- code 编译或运行失败作为 run-code 的结构化业务失败结果返回，不转换成旧 MCP 路由或旧工具名。
- 不暴露旧 `run_code` 工具名。

## CLI 契约

所有命令只输出 JSON。

- stdout 只输出一个 JSON 对象。
- stderr 默认不输出内容。
- argparse 参数错误、help、未捕获异常都必须转成 JSON 失败结果。
- 成功 exit code 为 `0`，失败为非 `0`。

成功：

```json
{
  "ok": true,
  "command": "run-code",
  "details": {}
}
```

失败：

```json
{
  "ok": false,
  "command": "run-code",
  "code": "studio_not_edit",
  "message": "Studio is not in edit mode",
  "details": {}
}
```

Phase 19A 命令：

```text
status
mode
ensure-edit
play
stop
screenshot
run-code-direct
run-code
play-mode-logs
```

普通命令的 `ensure-edit` 规则：

- `status`：不调用。
- `mode`：不调用。
- `ensure-edit`：自身就是显式命令。
- `play`：不调用。
- `stop`：不调用。
- `screenshot`：不调用。
- `run-code-direct`：不调用。
- `run-code`：调用。
- `play-mode-logs`：不调用。

`run-code-direct` 与 `run-code` 输入保持一致：

```text
--code <luau>
--file <path>
```

`--code` 与 `--file` 必须且只能提供一个。`--file` 默认按 UTF-8 读取。

## ensure-edit

`ensure-edit` 既是 bridge2 命令，也是 Python 工具函数。它不是 CLI 全局开关。

算法：

```text
读取 session.json
要求 helper2 task status 为 live
查询 /session/{task_id}/studio/mode
如果 mode == edit:
  返回 already_edit
如果 mode == play_server:
  调用 POST /session/{task_id}/studio/stop
  轮询 /session/{task_id}/studio/mode，直到返回 edit
  返回 stopped_play
其他情况:
  返回结构化错误
```

成功结果必须在 `details.reason` 里返回：

- `already_edit`
- `stopped_play`

成功和失败都应在 `details.last_mode` 里带上最后一次 mode payload。

注意点：

- stop 命令返回成功不代表 Studio 已进入 edit。
- stop 之后必须重新查询到 `edit`，才算 `ensure-edit` 成功。
- `unknown`、`unavailable`、`play_client` 与未支持模式必须失败，不允许猜测。
- 轮询必须有超时，失败结果需要带上最后一次 mode payload。

## play-mode-logs

不要把 bridge2 操作命令叫做 `runtime-log`。

当前面向 LLM / bridge2 的语义是：

```text
LLM / bridge2 从 helper2 读取 task 或 play-mode 日志。
```

CLI 命令名为：

```text
play-mode-logs
```

Phase 19A 允许 `play-mode-logs` 调用 helper2 现有 task-scoped log API，例如：

```text
GET /session/{task_id}/runtime-log
```

这里禁止的是旧独立 runtime-log server、旧 CLI 命令名和旧 fallback 路由，不是禁止 helper2 内部已有的 task-scoped log API。

## Phase 19B：official CLI 包装

Phase 19A 不实现 official 命令。

Phase 19B 等 Phase 18 helper2 官方原语落地后，再提供 bridge2 薄包装。初始候选命令：

```text
official-ping
official-store-image
official-generate-mesh
official-generate-procedural-model
official-wait-job
```

Creator Store search / insert 不属于 Phase 19A。它们需要先在 Phase 18B 或后续 official Creator Store phase 中定 helper2 原语，再接入 bridge2：

```text
official-search-creator-store
official-insert-from-creator-store
```

official 命令即使不调用 `ensure-edit`，也仍必须读取 `.clock-p/session.json`、要求 live task、走 task-scoped helper2 API。`不 ensure-edit` 只表示 bridge2 不负责停止 play 并等待 edit，不表示绕开 task / Studio 绑定。

## Phase 19C：read-studio-log

`read-studio-log` 是 helper2 的 Windows 本机能力，不是 mcp2 能力。它通过 helper2 的进程 / PID / 日志发现读取 task-owned Studio 日志，不要求 edit 模式。

Phase 19C 需要另行明确：

- helper2 HTTP 路径。
- `tail` / `limit` / cursor 参数。
- 是否返回日志文件路径。
- 如何按 task-owned Studio PID 过滤。
- 错误码与 JSON 形状。

## Phase 19A 验收

Python 单测：

- 从 `.clock-p/session.json` 读取 session。
- 不 fallback 到 helper1、mcp1、旧 hub、旧独立 runtime-log server。
- 成功和失败 stdout 都只输出一个 JSON。
- stderr 默认为空。
- argparse help、参数错误和异常都转 JSON。
- argparse 子命令路由正确。
- 每个子命令是否调用 `ensure_edit_mode` 与本文一致。
- `run-code-direct` 与 `run-code` 的 `--code` / `--file` 互斥且必填一个。
- `ensure_edit_mode` 覆盖 already-edit。
- `ensure_edit_mode` 覆盖 play-server -> stop -> edit。
- `ensure_edit_mode` 覆盖超时、未知模式、不可用模式和 stale task。

Go / helper2 测试：

- direct run-code 原语要求 live task。
- direct run-code 原语要求 task-owned Studio。
- direct run-code 原语拒绝非 edit 模式。
- direct run-code 原语通过 task-scoped mcp2 broker 路由。
- direct run-code 原语返回结构化终端诊断。
- helper2 MCP `tools/list` 暴露 `helper2_studio_run_code`，并要求 `task_id` 与 `code`。
- helper2 MCP `helper2_studio_run_code` 不暴露旧 `run_code` 别名。

真实 Studio 本地测试：

- `status`、`mode`、`play`、`stop`、`screenshot` 通过 bridge2 调现有 helper2 task API。
- `run-code-direct` 在 edit 模式成功，并返回打印输出。
- `run-code-direct` 在 play 模式失败，且不会停止 Studio。
- `run-code` 在 play 模式下先停止、等待新鲜的 `edit` mode 结果，再执行代码。
- `play-mode-logs` 从 helper2 读取 task/play 日志，不使用旧独立 runtime-log server。
- helper2 MCP `helper2_studio_run_code` 在真实 Studio edit 模式返回结构化结果。

本地验证记录：

- 2026-06-29，Windows client，本地 helper2 + task-agent + 真实 Studio。
- workspace：`K:\roblox_space\test_game3`。
- place_id：`113577273791190`。
- machine_name：`sunjun2`。
- Phase 19A bridge2 已跑通：`status`、`mode`、`screenshot`、`play-mode-logs`、edit 模式 `run-code-direct`、play 模式 `run-code-direct` 失败且不 stop、play 模式 `run-code` 先 stop 再执行。
- helper2 MCP 已跑通：`tools/list` 暴露 `helper2_studio_run_code` 且要求 `task_id` / `code`；edit 模式执行成功返回 `print` / `warn` / `return`；编译失败与 forbidden token 拦截返回结构化 JSON，外层 MCP 不报工具异常。
- 未执行公网验收。
- 未执行 Studio 多开验收。
