# hub/helper/task 控制面设计

## 目标

- 为 `studio-rust-mcp-server` 增加 hub 控制面，支持 task / helper / launch 生命周期管理。
- 把 helper 升级为机器级单例，并完成 Linux 联调能力。
- 为 Windows helper 启动 Studio、插件绑定 helper、按 task 连接 server 提供稳定协议。

## 关键约束

- hub 只做 control plane，不承担 MCP / Rojo 业务逻辑
- server cluster 先创建 task，helper 再 claim
- helper 不预启动 Studio
- 单机当前限制最多 4 个 Studio launch
- `restart` 保留 `task_id`
- `stop + start` 默认生成新 `task_id`
- `recover` 才接管旧 `task_id`
- `cluster_key = user + repo + worktree_name + place_id`

## 最新收口共识

- 一个机器只允许一个 helper；helper 是机器级单例。
- helper 的机器稳定身份使用 `helper_id`；Windows helper 使用 `MachineGuid` 派生，避免继续依赖随机落盘 id 或网卡 MAC 直接暴露。
- 当前控制域只允许一个 hub；同一 `task_id` 同时只允许一个有效 `generation`。
- hub 只会把一个 `task_id` 的当前有效任期交给一个 helper；当前强设定下一条 task 永远只对应一个 Studio/launch。
- `generation` 保留为 control plane fencing 字段，只用于 hub/helper/server 判定当前任期是否合法。
- plugin 不应持久化 `generation` 或自行决定 task 任期；plugin 只应该向 helper 注册并接受 helper 下发的当前绑定信息。
- server 数据面不把 `generation` 当业务字段扩散；只在 helper 接入时使用它做 launch fencing。
- 后启动的同 `helper_id` helper 必须失败退出，不能顶掉前一个活跃 helper；只有前一个退出或 heartbeat 超时后，后一个 helper 才能成功注册。
- helper 侧不引入额外 `helper_session_id` 或 token 体系；当前架构通过稳定 `helper_id` + hub 唯一约束收口复杂度。

## 当前问题

- helper 侧本地实例虽然按 `instance_id` 存，但远端连接按 `place_id` 聚合。
- server 侧 `rbx_studio_server.rs` 仍只有一个 `active_helper`，不是 task/helper 多路复用模型。
- helper / server 的 WS 协议核心字段还是 `place_id`，不够表达 task/helper/launch 身份。
- Linux helper 目前能保留 HTTP/WS 桥接能力，但没有独立 hub/task 生命周期。

## 方案

### 1. hub 控制面

- 在本仓新增 hub server，提供最小 API：
  - `POST /v1/helpers/register`
  - `POST /v1/helpers/heartbeat`
  - `POST /v1/helpers/claim`
  - `POST /v1/tasks/create`
  - `POST /v1/tasks/heartbeat`
  - `POST /v1/tasks/release`
  - `POST /v1/tasks/recover`
- hub 负责：
  - 生成 `task_id`
  - 管理 `generation`
  - 分发 `task_token/recover_token`
  - 维护 helper capacity 和 task claim 状态

### 2. 对象模型

- `Task`
  - `task_id`
  - `cluster_key`
  - `generation`
  - `place_id`
  - `game_id`
  - `owner_user`
  - `service_state`
  - `accepting_launches`
- `HelperAgent`
  - `helper_id`
  - `owner_user`
  - `platform`
  - `capacity`
  - `active_launch_count`
- `Launch`
  - `launch_id`
  - `task_id`
  - `generation`
  - `studio_pid`
  - `launch_nonce`
  - `state`

### 3. helper 机器级单例化

- helper 不再按 `place_id` 启动，而是机器级常驻。
- helper 向 hub 注册自身容量并定期心跳。
- helper claim 到 task 后，创建本地 launch 状态并连接对应远端 MCP server。
- Linux 阶段不真正启动 Studio，但保留 launch/task 绑定、MCP 代理和 hub 生命周期能力。

### 4. 协议升级

- helper 与 hub：增加 register / heartbeat / claim / ack 协议。
- helper 与 server WebSocket：`Hello/Heartbeat/Artifact` 等消息补 `task_id`，必要时补 `launch_id`。
- plugin 与 helper：注册结果除 `instance_id` 外，还要能关联当前 task 身份；Windows helper 负责补齐 pid / launch 绑定。

### 4.1 简化方向

- 不再继续引入新的 token/lease 概念扩张协议面；先把复杂度收进 helper/hub 内部。
- hub/helper 之间继续使用 `task_id + generation + launch_id` 作为当前最小充分身份。
- helper 对 plugin 暴露的应是“当前有效绑定”，而不是完整控制面真相。
- 继续简化时，应优先减少 plugin 可见字段，而不是增加新的协议对象。

### 5. Linux helper 裁剪策略

- 提供：
  - helper HTTP bridge
  - plugin 注册 / long polling
  - 远端 MCP WebSocket 连接
  - hub 注册、心跳、claim
  - task/launch 本地状态
- 禁用：
  - Win32 截图
  - 读 Studio 本地日志
  - 通过系统 TCP 表反查 Studio pid

## 当前实现

- 已提供 hub server 和 task/helper 生命周期协议。
- Linux server cluster 已支持创建 task、续命、release。
- Linux helper 已打通 helper <-> hub <-> server 联网链路。
- 已完成 task claim、MCP 代理和必要的状态查询测试。

### Windows 接力指引

- Windows 侧应重点查看：
  - 本文档的“关键约束”“最新收口共识”“4.1 简化方向”
  - `src/bin/studio_helper.rs`
  - `src/rbx_studio_server.rs`
  - `src/helper_ws.rs`
- Windows 侧实现要求：
  - 启动 helper 前先从 `MachineGuid` 派生稳定 `helper_id`
  - helper 启动流程里把 hub register 前置；若 hub 返回 `helper_id_conflict`，直接打印原因并退出，不进入 claim/启动 Studio 流程
  - helper 对本机 Studio/plugin 维持单点权威
  - plugin 不缓存或传播 `generation`
  - 不增加新的临时 token、回退路径或启发式补丁
  - helper 对“手动打开的 Studio 例外实例”保持不分配 task/launch 的策略，避免误把手动 Studio 拉入控制面

## 非目标

- 当前不要求 Linux helper 实现截图和 Studio 窗口观测
- 不把 hub 调度逻辑混进 MCP tool handler

## 文档改动

- `README.md`
- helper / HTTP backend / hub 相关章节
- 插件和 helper 状态说明

## 验证

- `cargo test`
- Linux 上构建 `rbx-studio-mcp` 与 `studio_helper`
- hub/task/helper 相关自动化测试
- 通过 platform 新链路完成联网测试
