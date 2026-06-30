# bridge2 / helper2 / mcp2 play-stop 验证链设计

## 背景

当前 `play` 成功的判定过弱。只看：

- `play` 请求返回成功
- `mode == play_server`
- `mode_seq` 变化

还不能证明“当前这次进入 play 的 runtime，就是这次脚本请求启动出来的那个 runtime”。如果 Studio 被手点进入 play，或别的调用方抢先进入 play，上述判据会误判成功。

本设计要把 play 启动验证收敛成一条更硬的链：

1. `play` 请求被 edit 态 mcp2 明确受理。
2. `mode_seq` 发生变化，且当前进入 `play_server`。
3. 当前 play runtime 通过 `StudioTestService:GetTestArgs()` 读到的 `launch_id` 与本次请求一致。

只有三者都成立，才算本次启动成功。

同时，stop 也要有自己的验证链，但 stop 不依赖 `launch_id`：

1. `stop` 请求被当前 play runtime 明确受理。
2. `mode_seq` 发生变化。
3. 当前模式稳定回到 `edit`。

只有三者都成立，才算本次停止成功。

## 目标

### 主目标

- 为每次脚本发起的 `play` 注入一个随机 `launch_id`。
- 让当前 play runtime 能把这个 `launch_id` 作为 live 状态的一部分回传给 helper2。
- 让 bridge2 用 `mode_seq + launch_id` 程序化验证启动成功，而不是让上层脚本或 LLM 人肉判断。

### 非目标

- 不处理自动 restart 编排。
- 不让 helper2 自己缓存或猜测 play/stop 真相。
- 不把文件路径能力下沉到 helper2 HTTP/MCP 协议。
- 不为手点 play 伪造 `launch_id`。

## 主线约束

### 事实源

- `mode` / `mode_seq` / `launch_id` 的权威来源是当前连着的 Studio runtime。
- helper2 只负责透传当前 live 查询结果，不自己记账推出 `launch_id`。
- 对脚本发起的 play，`launch_id` 来自本次请求参数。
- 对手点 play，没有 `launch_id`，返回缺失即可。

### 参数边界

- 外部调用方不能指定 `launch_id`。
- 外部调用方只能提供自定义 `data`。
- Python bridge2 负责：
  - 读取 `--data-json` 或 `--data-file`
  - 解析 JSON
  - 校验 `data` 必须是 JSON object
  - 生成随机 `launch_id`
  - 组装 `play_args`

### play_args 固定格式

helper2 往 mcp2 发起 `studio_play` 时，固定携带：

```json
{
  "place_id": "...",
  "mode": "start_play",
  "play_args": {
    "launch_id": 1234567890123,
    "data": {
      "k": "v"
    }
  }
}
```

约束：

- `launch_id` 为正整数。
- `launch_id` 必须落在 JSON safe integer 范围内。
- `data` 必须是 object。
- `data` 可以为空 object。

## 分层职责

### 原则补充

- bridge2 CLI 是面向人和脚本的主入口，只允许外部输入 `data`。
- helper2 HTTP/MCP 协议是结构化内部协议，承载完整 `play_args`。
- `/studio/mode` 在 `edit` 模式下不得回传历史 `launch_id`；只有当前 live play runtime 确实读到了 `launch_id` 时才返回。

## 步骤 1：bridge2 负责外部输入归一化

bridge2 `play` 命令新增：

- `--data-json`
- `--data-file`

规则：

- 两者互斥。
- 都不传时，默认 `data = {}`。
- `--data-file` 只在 Python CLI 层解析，不进入 helper2 协议。
- bridge2 每次调用 `play` 时都生成新的随机 `launch_id`。

bridge2 调 helper2 `/studio/play` 时，body 为：

```json
{
  "play_args": {
    "launch_id": 1234567890123,
    "data": {
      "k": "v"
    }
  }
}
```

## 步骤 2：helper2 负责结构化转发与 live 结果透传

helper2 新职责：

- `/studio/play` 接收 `play_args`
- 转发给 mcp2 `studio_play`
- `/studio/play` 响应中带出本次请求的 `requested_launch_id`
- `/studio/mode` 响应中带出当前 live runtime 报告的 `launch_id`

helper2 不做的事：

- 不读取文件
- 不生成 `launch_id`
- 不缓存 `launch_id` 作为自己的事实
- 不根据旧状态猜当前 `launch_id`

helper2 只把当前查询到的 `launch_id` 回给调用方。

## 步骤 3：edit 态 mcp2 负责受理前校验与 ExecutePlayModeAsync

edit 态 mcp2 收到 `studio_play` 后：

1. 检查当前必须是 `edit`
2. 读取 `command.args.play_args`
3. 校验：
   - `launch_id` 存在
   - `launch_id` 是正整数且在 safe integer 范围内
   - `data` 存在且是 table/object
4. 校验成功后，在 `response_result` 中返回：
   - `status = play_requested`
   - `launch_id = ...`
5. 只有 helper2 确认这条 `response_result` 已记录成功后，才执行：

```luau
StudioTestService:ExecutePlayModeAsync(playArgs)
```

校验失败时，直接在 edit 态返回明确错误，不进入 play。

## 步骤 4：play 态 mcp2 负责从当前 runtime 读取 launch_id

当前 runtime 在 `play_server` 模式下，通过：

```luau
StudioTestService:GetTestArgs()
```

读取这次 play 的真实参数。

规则：

- 若当前 play 由脚本发起且参数合法，提取其中的 `launch_id`
- 若当前 play 为手点 play，或没有合法 test args，则 `launch_id` 缺失
- `studio_mode_query` 返回当前 live 的：
  - `mode`
  - `mode_seq`
  - `launch_id`（可缺失）

`launch_id` 必须来自当前 runtime 的 live 读取结果，而不是 edit 态残留、helper2 缓存或其他历史观察。

## 启动成功判定

bridge2 的 `play` 命令应从“只发请求”升级为“发请求并验证结果”。

若 `/studio/play` 直接返回：

- `already_playing`
- `studio_play requires edit mode`
- `invalid_play_data`
- 其他明确失败

则 bridge2 立即失败返回，不进入后续验证轮询。

成功条件固定为：

1. `/studio/play` 返回受理成功
2. 后续 `/studio/mode` 观察到：
   - `mode == play_server`
   - `mode_seq != before_mode_seq`
3. 同一次 `/studio/mode` 返回的 `launch_id == requested_launch_id`

只有三者同时成立，bridge2 才返回 play 成功。

## 停止成功判定

bridge2 的 `stop` 命令也应从“只发请求”升级为“发请求并验证结果”。

若 `/studio/stop` 直接返回：

- `already_stopped`

则 bridge2 立即成功返回，不进入后续验证轮询。

若 `/studio/stop` 直接返回其他明确失败，则 bridge2 立即失败返回，不进入后续验证轮询。

成功条件固定为：

1. `/studio/stop` 返回受理成功
2. 后续 `/studio/mode` 观察到：
   - `mode == edit`
   - `mode_seq != before_mode_seq`

两者同时成立，bridge2 才返回 stop 成功。

stop 不校验 `launch_id`。原因不是弱化验证，而是当前 `StudioTestService:EndTest` 没有一条像 play 一样可闭环验证的 args 传递链。对 stop，主线证明的是“当前 live Studio 已从 play 收敛回稳定 edit”。

## 超时与失败语义

### request timeout

含义：

- edit 态 mcp2 没能在时限内把 `play_requested` 结果成功记录给 helper2

规则：

- 返回 `play_request_timeout`
- 不得继续执行 `ExecutePlayModeAsync`

这是 fail closed 语义。

### transition timeout

含义：

- 请求已经受理
- 但在验证窗口内，bridge2 没看到：
  - `play_server`
  - `mode_seq` 变化
  - `launch_id` 匹配

规则：

- 返回 `play_transition_timeout`
- 不自动重试
- 不自动补发第二次 `play`

### stop transition timeout

含义：

- 请求已经受理
- 但在验证窗口内，bridge2 没看到：
  - `edit`
  - `mode_seq` 变化

规则：

- 返回 `stop_transition_timeout`
- 不自动重试
- 不自动补发第二次 `stop`

### 非超时失败

- `invalid_play_data`
  - `data` 非合法 object
  - 或 `play_args` 结构非法
- `launch_id_missing`
  - 已进入 `play_server`
  - `mode_seq` 已变化
  - 但当前 runtime 没有 `launch_id`
- `launch_id_mismatch`
  - 已进入 `play_server`
  - 但当前 runtime 的 `launch_id` 不是本次请求的那个
- `unexpected_mode_transition`
  - 过程进入不可接受状态
  - 例如 `mode_seq` 已变化，但模式停在 `edit` 或进入未知模式

对于 stop，`unexpected_mode_transition` 例如：

- `mode_seq` 已变化，但模式仍停在 `play_server`
- 模式进入未知状态而不是回到 `edit`

`launch_id_missing` 与 `launch_id_mismatch` 都不是 timeout，而是明确失败。

## 默认时序

建议默认值：

- bridge2 `/studio/play` HTTP timeout：10 秒
- helper2 等待 edit 态 `response_result`：沿用现有 command wait 框架，但 play 受理阶段必须 fail closed
- bridge2 `play` 验证窗口：30 秒
- bridge2 `stop` 验证窗口：20 秒
- bridge2 `mode` 轮询间隔：0.3 到 0.5 秒
- 单次 `/studio/mode` 请求 timeout：5 秒

## 对手点 Play 的语义

- 手点 play 时，没有 `play_args`
- 因此当前 runtime 没有 `launch_id`
- `studio_mode_query` 返回 `launch_id` 缺失
- bridge2 若看到 `mode_seq` 变化但 `launch_id` 缺失，应判 `launch_id_missing`

这不是兼容问题，而是主线语义的一部分：脚本只能认出“是不是自己这次启动的 play”。

## 要改的地方

### bridge2 Python

- `tools/bridge2/clockp_bridge2/commands.py`
  - `play` 增加 `--data-json`
  - `play` 增加 `--data-file`
  - 解析并校验 `data`
- `tools/bridge2/clockp_bridge2/studio.py`
  - `play()` 改为提交 `play_args`
  - 在 Python 内部完成：
    - 读取调用前 `mode` / `mode_seq`
    - 调 `/studio/play`
    - 轮询 `/studio/mode`
    - 用 `mode_seq + launch_id` 验证成功
  - `stop()` 改为：
    - 读取调用前 `mode` / `mode_seq`
    - 调 `/studio/stop`
    - 轮询 `/studio/mode`
    - 用 `mode == edit + mode_seq` 验证成功
  - 输出结构化失败码
- `tools/bridge2/test_cli.py`
  - 增补 `--data-json`
  - 增补 `--data-file`
  - 增补 play 验证链成功 / timeout / missing / mismatch 测试
  - 增补 stop 验证链成功 / timeout / already_stopped 测试

### helper2

- `go-helper/cmd/studio-helper/main.go`
  - `studioPlayCommandArgs` 增加 `PlayArgs`
  - `/session/{task_id}/studio/play` 解析 body 中 `play_args`
  - enqueue play 时携带 `play_args`
  - `/studio/play` 响应增加 `requested_launch_id`
  - play request timeout 改成 fail closed，不再在未 recorded 时继续启动 play
  - `/studio/mode` 响应增加 `launch_id`
- `go-helper/cmd/studio-helper/mcp_server.go`
  - `helper2_studio_play` 支持结构化 `play_args`
  - `helper2_studio_mode` 透出 `launch_id`
- `go-helper/cmd/studio-helper/main_test.go`
  - 增补 play_args 透传、launch_id 透出、fail closed 相关测试

### plugin-mcp2

- `plugin-mcp2/src/Main.server.luau`
  - edit 态校验 `play_args`
  - `response_result` 回传 `launch_id`
  - `ExecutePlayModeAsync(playArgs)` 替代当前空表
  - play 态从 `GetTestArgs()` 提取 `launch_id`
  - `studio_mode_query` 返回 `launch_id`
  - 未 recorded 时不得继续启动 play

## 需要保持不变的边界

- helper2 不得自己缓存 play/stop 真相
- helper2 不得因为没有 `launch_id` 就猜测“应该是刚才那次脚本请求”
- stop 的最终稳定态仍使用当前主线里的 `edit`
- 不为旧协议保留双写或兼容分支；直接切到新协议

## 验收标准

### 正向

- `play --data-json` 成功启动，且返回验证后的 `launch_id`
- `play --data-file` 成功启动，且返回验证后的 `launch_id`
- `/studio/mode` 在脚本启动的 play 中返回正确 `launch_id`
- `stop` 成功停止，且返回验证后的 `mode == edit` 与新 `mode_seq`

### 反向

- 非法 JSON / 非 object `data` 在进入 play 前明确失败
- 手点 play 后，bridge2 不得误判为脚本启动成功
- 若别的调用方先抢进 play，本次调用必须返回 `launch_id_mismatch`
- 若 helper2 未记录到 edit 态 `play_requested`，Studio 不得继续启动 play
- 若 stop 请求已受理但 Studio 未回到 `edit`，bridge2 必须返回 `stop_transition_timeout` 或 `unexpected_mode_transition`
