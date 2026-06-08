# Studio 控制命令单飞与一次性测试边界

## 背景

Windows helper 通过长轮询把 Linux 侧 clockp MCP 命令派发给 Studio 插件。Studio 插件在执行 `LaunchStudioSession`、`StartStopPlay`、`RunScriptInPlayMode` 这类命令时，可能会等待 Studio 日志、`StudioTestService` 或 runtime 启动标记。这个等待期间插件不会继续长轮询，因此 helper 不能把“没有新的长轮询”直接当作插件实例失活。

之前的问题有两个：

- helper 的插件实例过期时间短于工具响应等待时间，导致正在执行工具的实例被清理，后续 stop 信号无法送达。
- `run_script_in_play_mode` 内部隐式调用 stop / force stop，让一次性测试工具承担了恢复职责，日志上难以解释到底是谁触发了 stop/play。

## 设计原则

1. helper 以 in-flight 工具响应作为插件实例存活依据之一。只要 helper 仍在等待该实例的工具响应，就不能因为长轮询暂时停止而删除实例。
2. Studio 控制类命令在同一个插件实例上单飞。`LaunchStudioSession`、`StartStopPlay`、`RunScriptInPlayMode` 任一未返回时，后来的控制类命令直接失败，不进入队列。
3. `run_script_in_play_mode` 只做一次性测试，不做隐藏恢复。它要求入口已经是 stop/edit；若 `StudioTestService` 仍在 settle，可以短暂等待并重试本次测试启动，但不能主动发 stop/play。
4. 恢复入口只有显式 `start_stop_play(stop)`。如果 stop 超时或 pending 长时间无法清除，应把状态和日志错误报告给上游，而不是继续自动 play/stop 循环。
5. 超时预算必须自上而下一致。clockp MCP 等 Studio 控制工具的窗口略大于 helper 等插件响应的窗口；`run_script_in_play_mode` 的 settle 与用户脚本 timeout 必须落在这个窗口内。

## 非目标

- 不改变 hub 的 task / helper 所有权规则。
- 不改 official MCP adapter 调度。
- 不把 `run_script_in_play_mode` 升级成通用 launch 工具；正常启动游戏仍走 `launch_studio_session` / `launch-game.py`。

## 验收

- helper 等待插件响应时不会清理对应插件实例。
- 同实例并发 Studio 控制命令会被 helper 拒绝。
- `run_script_in_play_mode` 不再隐式调用 stop 或 force stop。
- `run_script_in_play_mode` 默认 timeout 为 30 秒，最大 90 秒。
- `start_play` 内部自动恢复最多只做一次 stop 后重试。
- Rust 测试覆盖 helper 实例保活与单飞规则。
