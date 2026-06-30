# helper2 / mcp2 代码 flush 协议设计

## 背景

当前 `task-agent -> helper2 -> plugin-mcp2 -> Studio` 主线里，Rojo 仍承担代码同步与初始收敛职责。

这带来两个结构性问题：

1. 我们真正需要的是“代码是否已经按当前工程声明准确落到 Studio”。
2. Rojo 当前更偏持续自动同步，不直接提供一个硬的、可程序化验证的 flush 闭环。

本设计的目标不是给现有链路再加一层兜底，而是补一条独立的、可验证的代码 flush 主线：

- 本地 CLI 负责扫描本地工程树、算 hash、做 diff、驱动 flush。
- helper2 负责 task/binding 校验与结构化转发。
- mcp2 负责在稳定 edit 态下查询 Studio 当前代码树、执行 apply、回传 live 结果。

## 目标

### 主目标

- 新增一套独立于 `/status` 的 `code_sync` 协议。
- 只处理“代码文本树”的 flush，不处理通用资产同步。
- flush 只允许在稳定 `edit` 态执行。
- 以 managed roots 为边界，要求目标 Studio 子树最终与本地声明完全一致，包括删除多余节点。
- 用程序化 hash 对账保证 flush 成功，而不是靠日志猜测。

### 非目标

- 不处理图片、模型、音频等通用资产同步。
- 不处理 Studio -> 本地 syncback。
- 不在 helper2 里缓存或猜测“应该已经同步好了”。
- 不把 `--data-file` 之类本地文件路径概念下沉到 helper2 协议。
- 不在第一版实现根锚点保护策略；第一版遇到 managed root 内的形态不一致时，直接按声明替换。

## 主线原则

### 事实源

- Studio 当前代码树的权威事实源是当前连接的 mcp2 live 查询结果。
- helper2 只负责转发、绑定校验和保存最近一次 flush 结果摘要，不自己推断远端树状态。
- CLI 每次 flush 都重新扫描本地树；第一版不做本地扫描缓存。

### 托管边界

- 只有显式声明在 managed roots 里的目标根由 flush 托管。
- 一旦某个目标根被托管，其最终 Studio 子树必须与本地声明完全一致：
  - 本地有、远端没有：创建
  - 本地和远端都存在但内容不同：更新
  - 远端有、本地没有：删除

### 运行模式

- flush 只允许在稳定 `edit` 态执行。
- 若当前不是稳定 `edit`，直接失败，不自动 stop，不自动 restart。

## 配置与绑定

### 配置源

managed roots 的权威配置放在 workspace 根目录的版本化文件：

```text
code-sync.roots.json
```

它是工程配置，不放进 `.clock-p/`。

建议格式：

```json
{
  "project_id": "game1",
  "mapping_profile": "rojo_lua_v1",
  "roots": [
    {
      "root_id": "rewatch",
      "local_path": ".ts-out/packages/rewatch",
      "studio_path": ["ReplicatedStorage", "Packages", "rewatch"],
      "include": ["**/*.lua", "**/*.luau"],
      "exclude": ["**/*.js", "**/*.d.ts", "**/README.md", "**/LICENSE", "**/package.json"]
    },
    {
      "root_id": "rbxts_include",
      "local_path": "include",
      "studio_path": ["ReplicatedStorage", "rbxts_include"],
      "include": ["**/*.lua", "**/*.luau"],
      "exclude": []
    }
  ]
}
```

约束：

- `project_id` 是人类可读、稳定的工程标识。
- `mapping_profile` 第一版固定为 `rojo_lua_v1`。
- `root_id` 在同一配置内唯一。
- `local_path` 相对 workspace 根。
- `studio_path` 是 Studio 内绝对锚点路径。
- `include` / `exclude` 用于把非目标文件排除在本次代码 flush 外。

## session 绑定摘要

虽然配置文件放在 workspace 内，但 helper2 仍必须能拒绝“把 A 工程 flush 到 B Studio”。

因此 `task-agent` 在启动会话时，需要把 binding 摘要写入 `.clock-p/session.json`，并通过 heartbeat 告知 helper2：

- `workspace_id`
- `place_id`
- `project_id`
- `code_sync_config_hash`

其中：

- `workspace_id` 是稳定 workspace 标识。
- `project_id` 来自 `code-sync.roots.json`。
- `code_sync_config_hash` 是对规范化配置 JSON 计算出的稳定 hash。

第一版建议：

- `workspace_id` 由规范化后的 workspace 根路径计算稳定 hash 得到
- `code_sync_config_hash` 由规范化后的 `code-sync.roots.json` 文本计算稳定 hash 得到

后续每次 `code_sync` 请求都必须带上：

- `workspace_id`
- `project_id`
- `code_sync_config_hash`

helper2 若发现与当前 task 绑定摘要不一致，必须 fail closed。

## 文件系统到 Studio 的映射规则

第一版不发明新规则，直接采用一个明确的 Rojo Lua 子集：

```text
mapping_profile = rojo_lua_v1
```

支持规则：

- `*.server.lua` / `*.server.luau` -> `Script`
- `*.client.lua` / `*.client.luau` -> `LocalScript`
- `*.lua` / `*.luau` -> `ModuleScript`
- `init.server.lua` / `init.server.luau` -> containing directory usurp 为 `Script`
- `init.client.lua` / `init.client.luau` -> containing directory usurp 为 `LocalScript`
- `init.lua` / `init.luau` -> containing directory usurp 为 `ModuleScript`

其中“usurp”语义与 Rojo 一致：

- 若目录命中 `init.*` 规则，则该目录自身不再表现为 `Folder`
- 而是表现为同名脚本实例
- 该目录下其他子节点成为这个脚本实例的 children

注意：

- `code_sync` 查询和 diff 的对象，是按 `mapping_profile` 解释后的“逻辑树”
- 不是 Studio 原始 Instance 树的逐节点镜像
- 也就是说，命中 `init.*` 的目录在协议里从一开始就是脚本节点，而不是“目录加一个 init 文件”

第一版不支持：

- `.json` 转 `ModuleScript`
- `.model.json`
- `.meta.json`
- 任意非 UTF-8 文本
- 任意二进制资产

## 文本规范化

为了避免 Linux / Windows 行尾差异污染 hash，文本规范化规则必须先定死。

对所有被纳入 flush 的文本文件：

1. CLI 按 UTF-8 读取。
2. 若文件不是合法 UTF-8，直接失败。
3. 文本内容统一规范化为 `\n`。
4. hash 对规范化后的文本计算。
5. mcp2 写回 `Source` 时也写规范化后的 `\n`。

第一版只支持 UTF-8 文本文件。遇到其他编码，直接 `code_sync_unsupported_encoding`。

## 哈希模型

第一版使用 BLAKE3。

### 叶子节点 hash

叶子节点指最终落到 Studio 的脚本实例。

叶子节点 hash 计算输入至少包括：

- 节点类型：`ModuleScript` / `Script` / `LocalScript`
- 节点名
- 规范化后的 `Source`

也就是说：

- 仅内容相同但脚本类型不同，hash 必须不同。
- 仅名字不同但内容相同，hash 也必须不同。

### 目录节点 hash

目录节点 hash 由排序后的直接子节点摘要组成。每个 child summary 至少包括：

- `name`
- `entry_kind`
- `entry_hash`

其中 `entry_kind` 第一版取值固定为：

- `Folder`
- `ModuleScript`
- `Script`
- `LocalScript`

排序规则固定为按 `name` 升序。

### 根 hash

每个 managed root 都有自己的 `root_hash`。

此外再计算一个 `combined_hash`，由所有 root 的：

- `root_id`
- `root_hash`

排序后再 hash 得到。

`combined_hash` 用于表示“本次 managed roots 整体视图”。

注意：

- `combined_hash` 的作用域，只覆盖“本次请求里声明的 roots[]”
- 计算时按 `root_id` 排序，不依赖请求传入顺序
- `mapping_profile` 变化时，`combined_hash` 也视为不同语义下的值，不能混用

## 协议面

`code_sync` 不并入 `/status`。helper2 新增独立协议面。

建议稳定原语如下：

- `code_sync_get_manifest`
- `code_sync_query_tree`
- `code_sync_apply`
- `code_sync_get_last_result`

对应 helper2 task API 可以是：

- `POST /session/{task_id}/code-sync/get-manifest`
- `POST /session/{task_id}/code-sync/query-tree`
- `POST /session/{task_id}/code-sync/apply`
- `GET  /session/{task_id}/code-sync/last-result`

对应 MCP 工具名保持同义。

### 通用协议字段

所有 `code_sync` 请求与响应，第一版都应带：

- `protocol_version`
- `workspace_id`
- `project_id`
- `code_sync_config_hash`

其中：

- 第一版 `protocol_version` 固定为 `1`
- helper2 或 mcp2 若发现版本不支持，直接返回 `code_sync_protocol_version_unsupported`

### 路径编码

协议里的逻辑路径统一用“路径段数组”表达，不用 `/` 拼接字符串。

例如：

```json
["ReplicatedStorage", "Shared", "world", "ClientWorld"]
```

原因：

- 避免名字里含 `/`、转义、大小写规范之类的歧义
- 让 CLI / helper2 / mcp2 三边都按同一结构处理

因此：

- `studio_path`
- `studio_rel_path`
- `logical_path`

都统一是 `string[]`。

### 负载上限

第一版必须把单次请求体和响应体控制在可预估范围内。

硬约束：

- 单次 HTTP 请求 body 目标上限：`<= 8 MiB`
- 单次 HTTP 响应 body 目标上限：`<= 8 MiB`
- 绝对 hard cap：`10 MiB`

也就是说：

- 协议设计、CLI 分批策略、helper2 / mcp2 校验都要围绕这个上限展开
- 不能默认“先全塞进去，超了再看”

选择 `8 MiB` 作为目标上限，是为了给 JSON 编码膨胀、头部、未来少量字段增加留余量。

## code_sync_get_manifest

用途：

- 校验当前为稳定 `edit`
- 校验 binding
- 获取远端当前 managed roots 的总览摘要

请求包含：

- `workspace_id`
- `project_id`
- `code_sync_config_hash`
- `mapping_profile`
- `roots[]`

其中每个 root 至少带：

- `root_id`
- `studio_path`

返回：

- `mode`
- `mode_seq`
- `project_id`
- `code_sync_config_hash`
- `mapping_profile`
- `combined_hash`
- `roots[]`

每个 root 返回：

- `root_id`
- `studio_path`
- `root_kind`
- `root_hash`
- `children_count`
- `children_complete`
- `next_child_offset`
- `children[]`

其中：

- `children[]` 只返回该 root 的直接 children summary，不递归全量展开
- 若第一层 children 太多，允许只返回一个前缀页
- 此时：
  - `children_complete = false`
  - `next_child_offset` 指向下一页起点

也就是说，`get_manifest` 的首要职责是给出 root hash 总览；第一层 children summary 在必要时可以分页，不强求一次回完。

这样本地可以先做第一层判断：

- 根 hash 相同：整棵根跳过
- 根 hash 不同：根据 direct children summary 决定是否继续下钻

注意：

- `get_manifest` 返回的是一次 live 观察，不保证之后远端不再变化
- CLI 后续若基于它继续下钻或 apply，必须在 `apply` 阶段再做并发校验

## code_sync_query_tree

用途：

- 对一批已知不同的远端子树继续下钻

请求包含：

- `workspace_id`
- `project_id`
- `code_sync_config_hash`
- `queries[]`

其中每个 query 至少包含：

- `root_id`
- `studio_rel_path`
- `child_offset`

返回：

- `mode_seq`
- `entries[]`

其中每个 `entry` 至少包含：

- `root_id`
- `studio_rel_path`
- `kind`
- `hash`
- `children_count`
- `children_complete`
- `next_child_offset`
- `children[]`

注意：

- 只返回一层
- 不直接回整棵远端树
- `studio_rel_path` 指的是相对 root 的逻辑路径，而不是原始文件系统路径
- `child_offset` 表示“从该节点 direct children 的第几个开始返回”

这样协议可以多轮往返，但每轮都稳定、轻量、可缓存。

关键语义：

- CLI 应把同一轮 frontier 上所有需要继续展开的目录，合并到一次 `query_tree` 请求里
- 不是“一个目录一次请求”

例如：

- 第一层有 10 个 child folder 都不同
- CLI 应一次发送 10 个 `queries[]`
- helper2 / mcp2 一次返回 10 个 `entries[]`

因此往返次数应主要近似于“需要展开的最大树深”，而不是“脏目录个数”。

但这里还有一个同等重要的约束：

- 同一轮 frontier 要尽量批量
- 但不能为了少 RTT 把单次响应堆到超过负载上限

因此 CLI 组一轮 `queries[]` 时，应同时考虑：

- frontier 批量展开
- 响应体大小预算

第一版建议：

- CLI 维护一个保守的“预计响应字节数”预算
- 逐个把 query 加入当前批次
- 一旦预计当前批次响应接近 `8 MiB` 目标上限，就切下一批
- 若单个节点 direct children 本身就太多，则通过 `child_offset` 分页继续取

因此真实语义不是“整层必须一请求”，而是：

- “整层尽量少请求”
- “但每个请求/响应都不能超过负载预算”

第一版不做：

- 服务端维护的 opaque cursor 分页
- 服务端流式返回

也就是说，一次 `query_tree` 返回一批逻辑节点下一层的全部 children summary。

更准确地说：

- 对“正常大小”的节点，一次返回该节点下一层的全部 children summary
- 对“单节点 direct children 过多”的节点，允许按 children page 多次返回

第一版建议按“分轮批量展开 frontier”实现：

1. `get_manifest` 拿到各 root 第一层摘要
2. CLI 找出第一轮需要继续展开的所有目录，组成 `queries[]`
3. `query_tree` 一次返回这一整轮的 `entries[]`
4. CLI 再从这些 `entries[]` 中挑出下一轮仍需展开的目录
5. 重复，直到足够生成 diff

若某一轮 frontier 很大，则允许这一轮拆成：

- `frontier batch 1`
- `frontier batch 2`
- ...

这些 batch 仍属于同一层展开，而不是退化成逐节点请求。

若某个单独节点过宽，则还允许在同一层内对这个节点继续取：

- `child_offset = 0`
- `child_offset = next_child_offset`
- ...

直到该节点 `children_complete = true`。

因此典型往返次数应接近：

- `1 次 get_manifest`
- `D 次 query_tree`
- `1 次 apply`
- `1 次最终 get_manifest`

其中：

- 常见情况下，往返次数主要由最大深度 `D` 决定
- 只有在“单个节点极宽”时，才会额外增加少量同层 children page 往返

## code_sync_apply

用途：

- 在稳定 `edit` 态下把本地 diff 应用到远端 Studio

请求包含：

- `workspace_id`
- `project_id`
- `code_sync_config_hash`
- `mapping_profile`
- `expected_mode_seq`
- `expected_combined_hash_before`
- `ops[]`

其中：

- `expected_mode_seq` 来自最近一次 `get_manifest` 或 `query_tree`
- `expected_combined_hash_before` 来自最近一次 `get_manifest`

`apply` 前必须同时检查：

- 当前仍是稳定 `edit`
- 当前 `mode_seq == expected_mode_seq`
- 当前 `combined_hash == expected_combined_hash_before`

任何一个不满足，都不得开始写入，直接返回：

- `code_sync_state_stale`

这样可以 fail closed 地挡住：

- 手工改了 managed root
- 别的调用方先 flush 了
- Studio 中途进过 play / stop，哪怕后来又回到 edit

### apply 传输粒度

第一版 `code_sync_apply` 固定为“一次 flush 一次请求”：

- 不做多请求分片上传
- 不做 server 端 apply session
- 不做断点续传

原因不是永远不支持，而是：

- 当前 `game1` 代码规模足够先用单请求跑通
- 单请求语义最简单
- 更容易先把正确性钉死

但这并不意味着可以无视负载上限。

因此第一版还必须满足：

- 若单次 `ops[]` JSON 编码后预计会超过 `8 MiB` 目标上限，CLI 不得直接发送
- 此时直接返回明确错误：
  - `code_sync_apply_payload_too_large`

也就是说：

- 第一版 `apply` 不做多请求分片
- 但也不允许默默发超大请求
- 真遇到这个量级，再单独设计 `apply_session`

若后续 body 体积或 op 数量真的成为瓶颈，再单独设计 `apply_session`，不在第一版预埋半套协议。

`ops[]` 第一版建议支持：

- `ensure_container`
- `upsert_script`
- `delete_path`

语义：

第一版 `ops[]` 由 CLI 按确定顺序生成，mcp2 按给定顺序执行，不自行重排。

推荐顺序：

1. `delete_path`，按路径深度从深到浅
2. `ensure_container`，按路径深度从浅到深
3. `upsert_script`

这样可以稳定处理：

- 删除多余节点
- `Folder -> Script`
- `Script -> Folder`
- `init.*` usurp 替换

### ensure_container

确保某个目录锚点存在，且类型正确。

它用于创建：

- 真实存在于逻辑树中的 `Folder` 容器路径

它不用于创建被 `init.*` usurp 的脚本目录。后者应直接由 `upsert_script` 在对应逻辑路径创建脚本节点。

### upsert_script

按逻辑路径写入脚本节点：

- `logical_path`
- `name`
- `class_name`
- `normalized_source`

若目标已存在但类型不对，第一版直接替换。

### delete_path

删除 managed root 内本地不存在的多余节点。

第一版不做 rename 特判。重命名统一表现为：

- `delete old`
- `upsert new`

### apply 返回语义

`apply` 响应至少返回：

- `ok`
- `mode_seq`
- `observed_combined_hash_before`
- `combined_hash_after`
- `applied_op_count`

若失败，还要返回：

- `error_code`
- `error_message`
- `failed_op_index`
- `applied_op_count`

### apply 的部分失败语义

第一版不承诺事务性回滚。

更准确地说：

1. mcp2 在真正写入前，应先完成请求级校验：
   - `protocol_version`
   - binding
   - `mapping_profile`
   - 当前稳定 `edit`
   - `expected_mode_seq`
   - `expected_combined_hash_before`
   - `ops[]` 结构合法
2. 上述检查全部通过后，才开始顺序执行 `ops[]`
3. 若中途某个 op 失败：
   - 立即停止
   - 返回 `code_sync_apply_failed`
   - 带上 `failed_op_index` 和 `applied_op_count`
4. CLI 收到后必须重新 `get_manifest`，把当前远端树当成新的 live 事实源，不能假设远端仍停留在 apply 前状态

也就是说：

- 第一版保证“校验不过不写”
- 不保证“写了一半还能自动回滚”

## code_sync_get_last_result

helper2 可以保留最近一次 flush 的结果摘要，但这不等于远端事实源。

它只用于：

- 调试
- 人类查看
- 失败复盘

建议字段：

- `workspace_id`
- `project_id`
- `code_sync_config_hash`
- `requested_combined_hash`
- `applied_combined_hash`
- `observed_mode_seq`
- `ok`
- `finished_at_ms`
- `error_code`
- `error_message`

注意：

- 这是“最近一次 helper2 观测到的 flush 结果”
- 不是 `/status`
- 也不是当前远端树的权威事实

当前远端树的权威事实仍需通过 live `get_manifest/query_tree` 查询。

## flush 编排

第一版 CLI 侧流程：

1. 读取 `code-sync.roots.json`
2. 规范化配置 JSON，计算 `code_sync_config_hash`
3. 全量扫描本地 managed roots
4. 对文本内容做 UTF-8 + `\n` 规范化
5. 基于 `rojo_lua_v1` 规则生成本地逻辑树
6. 计算本地各 root hash 与 combined hash
7. 调 `code_sync_get_manifest`
8. 对 hash 不同的根按需调用 `code_sync_query_tree` 逐层下钻
9. 生成 `ops[]`
10. 调 `code_sync_apply`
11. apply 成功后再次调用 `code_sync_get_manifest`
12. 若返回的 `combined_hash` 与本地目标一致，则 flush 成功；否则失败

也就是说，成功判据不是“apply 返回成功”，而是：

- 远端 live manifest 最终与本地目标 hash 对上

## 错误语义

建议第一版明确区分这些错误：

- `code_sync_not_in_edit`
  - 当前不在稳定 `edit`
- `code_sync_binding_mismatch`
  - `workspace_id` / `project_id` / `code_sync_config_hash` 与当前 task 绑定不一致
- `code_sync_invalid_config`
  - 本地配置文件非法
- `code_sync_unsupported_encoding`
  - 文本不是 UTF-8
- `code_sync_unsupported_mapping`
  - 命中了第一版不支持的文件或规则
- `code_sync_remote_query_failed`
  - manifest / query_tree live 查询失败
- `code_sync_state_stale`
  - `mode_seq` 或 `combined_hash` 已不是本次 diff 观察到的那个
- `code_sync_query_payload_too_large`
  - 某轮 `query_tree` 预计响应超过单次负载上限，需要 CLI 缩小 batch
- `code_sync_apply_payload_too_large`
  - 本次 `apply` 请求体预计超过单次负载上限
- `code_sync_apply_failed`
  - mcp2 apply 过程中失败
- `code_sync_verify_failed`
  - apply 后复查 hash 仍不一致

第一版不自动重试，不自动 fallback 到其他同步路径。

## game1 当前落地建议

对当前 `game1`，第一版可先只托管这些代码树：

- `.ts-out/packages/rewatch -> ReplicatedStorage/Packages/rewatch`
- `include -> ReplicatedStorage/rbxts_include`
- `.ts-out/shared -> ReplicatedStorage/Shared`
- `.ts-out/server -> ServerScriptService`
- `.ts-out/client -> StarterPlayer/StarterPlayerScripts`

`node_modules/@rbxts` 不建议第一版整棵搬运。若运行期确实需要，只纳入实际会进入 Studio 的 Lua/Luau 子集，不把 `.d.ts`、README、LICENSE、`package.json` 一起带进去。

## 迁移原则

这套 `code_sync` 协议的引入，不要求第一天就删除现有 Rojo。

但语义上必须明确：

- `code_sync flush` 的成功标准是 hash 对账闭环
- 不是“Rojo 连上了”
- 也不是“日志里看起来像同步过了”

后续若逐步弱化甚至移除 Rojo，这套协议仍应保持不变。
