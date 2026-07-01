# bridge2 launch / prelaunch 设计

## 背景

当前 bridge2 对外只有 `play`，语义是直接调用 helper2 `/studio/play`，并用 `mode_seq + launch_id` 验证启动成功。

随着 `code_sync` 主线接入，`game1` 这类工程的常见启动流程已经不再只是“直接 play”，而是：

1. 本地 build
2. 确保 Studio 回到稳定 edit
3. 执行 code sync flush
4. 再进入 play

这说明当前需要两个层次的入口：

- `play`：直接启动原语
- `launch`：带 prelaunch steps 的高层编排入口

## 目标

### 主目标

- 新增 bridge2 `launch` 命令。
- `launch` 读取 `prelaunch.json`，按顺序执行 steps，全部成功后再调用现有 `play`。
- 保持 `play` 继续作为 direct primitive，不掺 build / flush / prelaunch 逻辑。
- 把 `ensure_edit` 明确成一个可复用原语，既能单独作为 CLI 命令，也能作为 `launch` step 以及 `code_sync_apply` 的可选前置行为。

### 非目标

- 不改 helper2 或 mcp2 协议来承载 prelaunch step。
- 不把任意本地文件路径、shell 编排或 workflow 下沉到 helper2。
- 不在第一版实现任意复杂条件分支、重试、并行 step 或 apply session。

## 命名主线

- `play`：direct primitive
- `launch`：workflow entry

第一版不恢复 `launch-direct` 作为主命令名。

如果未来确实需要兼容旧脚本，再单独考虑把 `launch-direct` 做成 `play` 的 alias；但当前主线设计与实现都围绕 `play` 和 `launch`。

## `prelaunch.json`

工作区根目录新增可选文件：

```text
prelaunch.json
```

第一版格式：

```json
{
  "steps": [
    {
      "kind": "ensure_edit",
      "name": "ensure_edit"
    },
    {
      "kind": "shell",
      "name": "build",
      "argv": ["npm", "run", "build:roblox"],
      "cwd": "."
    },
    {
      "kind": "code_sync_apply",
      "name": "flush",
      "config": "code-sync.roots.json",
      "project": "default.project.json"
    }
  ]
}
```

约束：

- 根对象必须是 JSON object。
- `steps` 必须是 array。
- step 按数组顺序串行执行。
- 任一步失败，`launch` 立即停止，不继续后续 step，也不进入 `play`。

## Step 种类

第一版只支持三种 step：

### `ensure_edit`

字段：

- `kind = "ensure_edit"`
- `name` 可选

语义：

- 调用 bridge2 Python 原语 `ensure_edit_mode(session)`
- 若当前已在 edit，返回 `already_edit`
- 若当前在 `play_server`，调用 `stop` 并等待稳定 edit
- 若当前处于其他不可收敛状态，失败

### `shell`

字段：

- `kind = "shell"`
- `name` 可选
- `argv` 必填，string array
- `cwd` 可选，默认 workspace 根；解析后必须仍位于 workspace 内

语义：

- 在 bridge2 所在工作区本地执行 shell command
- 不通过 helper2
- 默认不走字符串 shell 拼接，直接按 argv 调 `subprocess.run`
- 捕获 `stdout` / `stderr`
- 非零退出码即失败

### `code_sync_apply`

字段：

- `kind = "code_sync_apply"`
- `name` 可选
- `config` 可选，默认 `code-sync.roots.json`；解析后必须仍位于 workspace 内
- `project` 可选，默认 `default.project.json`；解析后必须仍位于 workspace 内
- `ensure_edit` 可选，默认 `true`

语义：

- 调用 bridge2 Python 的 `apply_code_sync(...)`
- 若 `ensure_edit = true`，先完成本地 manifest / roots preflight，再调用 `ensure_edit_mode(session)`，然后做 flush
- 若 `ensure_edit = false`，维持 direct 行为：当前不是稳定 edit 就失败

## `ensure_edit` 原语

当前 bridge2 已有：

- CLI 命令：`ensure-edit`
- Python 函数：`ensure_edit_mode(session, ...)`

本轮改动要把它正式当成原语复用，而不是只当 CLI 顶层命令。

具体要求：

- `launch` step `ensure_edit` 直接调用它
- `code_sync_apply(...)` 增加 `ensure_edit: bool = False` 参数
- CLI `code-sync-apply` 也增加：
  - `--ensure-edit`
  - `--no-ensure-edit`

推荐语义：

- Python 函数默认 `ensure_edit=False`
- CLI `code-sync-apply` 默认 `--ensure-edit`
- `launch` 的 `code_sync_apply` step 默认 `ensure_edit=true`

这样：

- 原语层保持保守
- 人工 CLI 使用更顺手

## `launch` 命令

bridge2 新增子命令：

```text
launch
```

参数：

- `--prelaunch`：可选，默认 `prelaunch.json`；解析后必须仍位于 workspace 内
- `--data-json`
- `--data-file`

语义：

1. 读取 session
2. 读取并解析 `prelaunch.json`
3. 逐步执行 `steps`
4. 所有 steps 成功后，调用现有 `play(session, data)`
5. 返回结构化结果

返回形态：

```json
{
  "prelaunch": {
    "ok": true,
    "steps": [...]
  },
  "play": {
    "...": "现有 play 返回"
  }
}
```

若 `prelaunch.json` 不存在，第一版建议直接失败，而不是隐式当成空 steps。

原因：

- `launch` 的语义就是“显式 workflow”
- 若用户只想 direct start，应该用 `play`

## 失败语义

### `launch_prelaunch_failed`

用于：

- step 执行失败
- `prelaunch.json` 缺失 / 读取失败 / 非法

细节至少包括：

- `step_index`
- `step_name`
- `step_kind`
- `step`
- `prelaunch_results`
- `step_error = { code, message, details }`

若是 `shell` step，`argv/cwd/exit_code/stdout/stderr` 放在 `step_error.details` 里。

若是 `code_sync_apply` 或 `ensure_edit` step，下层 `BridgeError` 的 `code/message/details` 统一放入 `step_error`。

若是 prelaunch 文件本身失败，则：

- `step_index = null`
- `step_name = null`
- `step_kind = null`
- `step = null`
- `prelaunch_path = ...`

### `launch_play_failed`

用于：

- prelaunch 全成功，但最终 `play` 失败

细节至少包括：

- `prelaunch_results`
- `play_error`

## 代码落点

建议 Python 层新增模块：

- `clockp_bridge2/prelaunch.py`

职责：

- 读取 / 校验 `prelaunch.json`
- 定义 step 数据结构
- 执行 step

bridge2 变更点：

- `commands.py`
  - 新增 `launch`
  - 新增 `code-sync-apply --ensure-edit/--no-ensure-edit`
- `code_sync/apply.py`
  - 新增 `ensure_edit` 参数
- 视需要在 `studio.py` 继续复用 `ensure_edit_mode`

## 验证

至少补：

- `play` 现有测试不回归
- `launch` 成功执行 `ensure_edit -> shell -> code_sync_apply -> play`
- `shell` step 失败时，`play` 不会执行
- `code_sync_apply(ensure_edit=True)` 会先收敛 edit
- `code_sync_apply(ensure_edit=False)` 仍保持 direct fail-closed 语义
- `prelaunch.json` 非法时返回明确错误

## game1 预期使用方式

对 `game1`，预期命令形态是：

```bash
clockp-roblox-cli --workspace /root/roblox_space/game1 launch
```

由 `prelaunch.json` 负责声明：

1. `ensure_edit`
2. `npm run build:roblox`
3. `code_sync_apply`

最后再进入现有 `play`。
