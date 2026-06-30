# helper2 / mcp2 代码 flush 协议设计

## 背景

当前 `task-agent -> helper2 -> plugin-mcp2 -> Studio` 主线必须切换为 code-sync 模式。Rojo 不再承担代码同步、初始收敛、task 会话注册或验收职责。

这带来两个结构性问题：

1. 我们真正需要的是“代码是否已经按当前工程声明准确落到 Studio”。
2. 旧 Rojo 主线把“会话是否 live”和“代码是否同步”耦合在一起，导致没有 Rojo 时 Studio / mcp2 / code-sync 链路无法建立。

本设计的目标不是给 Rojo 链路再加一层兜底，而是建立唯一的、可验证的代码 flush 主线：

- 本地 CLI 负责扫描本地工程树、算 hash、做 diff、驱动 flush。
- task-agent 负责维护 `.clock-p/session.json`、计算 code-sync binding 摘要、向 helper2 心跳，不启动、不看护、不等待 Rojo。
- helper2 负责 task/code-sync binding 校验、task-owned Studio 生命周期与结构化转发。
- mcp2 负责在稳定 edit 态下查询 Studio 当前代码树、执行 apply、回传 live 结果。

## 目标

### 主目标

- 新增一套独立于 `/status` 的 `code_sync` 协议。
- task-agent 主线彻底移除 Rojo：不启动 Rojo、不注册 Rojo public route、不通过 Rojo ready 判定 task live。
- `.clock-p/session.json` 和 helper2 task contract 以 code-sync binding 为会话不可变摘要。
- 只处理“代码文本树”的 flush，不处理通用资产同步。
- flush 只允许在稳定 `edit` 态执行。
- 以 managed roots 为边界，要求目标 Studio 子树最终与本地声明完全一致，包括删除多余节点。
- 用程序化 hash 对账保证 flush 成功，而不是靠日志猜测。
- 第一版按约 1000 个代码文件的工程规模设计，不能退化成逐文件 HTTP 往返，也不能依赖一次拉完整棵树。

### 非目标

- 不处理图片、模型、音频等通用资产同步。
- 不处理 Studio -> 本地 syncback。
- 不在 helper2 里缓存或猜测“应该已经同步好了”。
- 不把 `--data-file` 之类本地文件路径概念下沉到 helper2 协议。
- 不保留 Rojo 作为 fallback、兼容入口或验收依据。
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

### task 会话主线

- task-agent 启动后必须直接写入 `.clock-p/session.json`，并立即向 helper2 发送 heartbeat。
- helper2 收到合法 heartbeat 后将 task session 置为 `live`，并通过 `EnsureTaskDesired(task_id, place_id)` 拉起 task-owned Studio。
- plugin-mcp2 的 command pull / response 通道只依赖 helper2 管理的 task-owned Studio 与 live task session，不依赖 Rojo。
- task-agent 不再暴露 `--rojo-bin`、`--project` 这类 Rojo 运行时参数；如仍需读取工程 DataModel 声明，应使用 code-sync 语义下的 `--code-sync-project` 或默认 `default.project.json`。
- helper2 不再要求 task contract 中存在 `rojo_upstream_url`。

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
- `mapping_profile` 第一版固定为 `rojo_lua_v1`。这是文件系统到 Studio 逻辑树的历史兼容 profile 名称，不表示运行时依赖 Rojo。
- `root_id` 在同一配置内唯一。
- `local_path` 相对 workspace 根。
- `studio_path` 是 Studio 内绝对锚点路径。
- `include` / `exclude` 用于把非目标文件排除在本次代码 flush 外。

## session 绑定摘要

`code-sync.roots.json` 是 flush 配置源，`.clock-p/session.json` 是当前 task 会话源。两者在文件上保持独立；每次 flush 时 bridge2 都会同时读取 session 和 flush 配置。

为了让 helper2 拒绝“把 A 工程 flush 到 B Studio”，task-agent 在启动会话时必须读取同一份 flush 配置，计算 binding 摘要，写入 `.clock-p/session.json`，并通过 heartbeat 告知 helper2。

会话 binding 摘要包含：

- `workspace_id`
- `place_id`
- `machine_name`
- `project_id`
- `code_sync_config_hash`
- `mapping_profile`
- `roots_authority_hash`

其中：

- `workspace_id` 是稳定 workspace 标识。
- `place_id` 是本 task 绑定的 Roblox place。
- `machine_name` 是 helper2 侧识别本机身份的机器名。
- `project_id` 来自 `code-sync.roots.json`。
- `code_sync_config_hash` 是对规范化配置 JSON 计算出的稳定 hash。
- `mapping_profile` 来自 `code-sync.roots.json`。
- `roots_authority_hash` 是 `root_id -> studio_path` 权威表的稳定 hash，纳入 `code_sync_config_hash`。

第一版建议：

- `workspace_id` 由规范化后的 workspace 根路径计算稳定 hash 得到
- `code_sync_config_hash` 由规范化后的 `code-sync.roots.json`、Studio target allowlist 与 root 配置共同计算得到

`.clock-p/session.json` 典型结构：

```json
{
  "task_id": "t0123abcd45",
  "environment": "local",
  "machine_name": "sunjun2",
  "place_id": "113577273791190",
  "task_agent_pid": 12345,
  "task_agent_started_at_ms": 1780000000000,
  "task_agent_status_url": "http://127.0.0.1:32123/status",
  "task_session_token": "opaque-random-token",
  "helper": {
    "base_url": "http://127.0.0.1:44750"
  },
  "code_sync": {
    "protocol_version": 1,
    "workspace_id": "a-stable-workspace-id",
    "place_id": "113577273791190",
    "machine_name": "sunjun2",
    "project_id": "game1",
    "mapping_profile": "rojo_lua_v1",
    "code_sync_config_hash": "hex-hash",
    "roots_authority_hash": "hex-hash",
    "config_path": "code-sync.roots.json",
    "project_path": "default.project.json",
    "roots": [
      {
        "root_id": "workspace-test",
        "studio_path": ["Workspace", "ClockPTest"]
      }
    ]
  }
}
```

约束：

- session 中不再写入 `rojo` 对象。
- helper2 task contract 不再包含 `rojo_upstream_url`。
- task contract 的不可变元组至少包括：`task_id`、`machine_name`、`place_id`、`code_sync.protocol_version`、`code_sync.workspace_id`、`code_sync.project_id`、`code_sync.mapping_profile`、`code_sync.code_sync_config_hash`、`code_sync.roots_authority_hash`、`code_sync.roots[]`。
- 同一个 `task_id` 的后续 heartbeat 若改变不可变元组中的任意字段，helper2 必须返回 immutable mismatch。
- `code_sync.roots[]` 是 helper2 / mcp2 的 managed root 权威表。后续 live manifest、query_tree 和 apply 只能引用其中已有的 `root_id`，不得由 CLI 在请求中临时提供新的 `studio_path`。
- `code-sync-manifest` 可只读本地 flush 配置，不需要 session；`code-sync-live-manifest`、`code-sync-dry-run`、`code-sync-apply` 必须同时读取 session 和 flush 配置，并确认两边 binding 摘要一致。

后续每次 `code_sync` 请求都必须带统一 binding envelope：

- `protocol_version`
- `workspace_id`
- `place_id`
- `machine_name`
- `project_id`
- `code_sync_config_hash`
- `mapping_profile`
- `roots_authority_hash`

请求路径中的 `task_id`、请求头中的 `X-ClockP-Task-Token`、以及 body 中的 binding envelope 必须同时与 helper2 当前 task contract 匹配；任一不匹配都必须 fail closed。

第一版认证边界：

- task-agent 生成高熵 `task_session_token`，写入 `.clock-p/session.json`，并通过 heartbeat 注册到 helper2。
- bridge2 读取 session 后，通过 `X-ClockP-Task-Token` 调 helper2 task API。
- helper2 必须校验 token、`machine_name`、`task_id` 与不可变 binding。
- 第一版仍假定本机同一 OS 用户可读 workspace/session 文件属于可信边界；若要防同用户恶意进程，需要后续引入 OS credential / named pipe ACL 级别隔离。

helper2 若发现与当前 task 绑定摘要不一致，必须 fail closed。

## Studio 托管目标规则

Studio 侧目标是代码 flush 的破坏性写入边界，第一版必须优先对齐当前游戏工程的 DataModel 声明文件。当前可以沿用 `default.project.json` 的 tree 格式作为声明源，但它只提供静态 Studio target allowlist，不再代表运行时 Rojo 同步链路。

原则：

- 当前游戏工程已经声明、已经使用到的 Studio 目标，`code_sync` 第一版必须支持。
- 不因为“安全”理由缩窄当前工程已经声明的目标范围；安全约束只用于防止越界写入、路径歧义和误绑定。
- Studio 允许哪些 service / container 作为目标，应从 project 声明和 `code-sync.roots.json` 显式配置中得出，而不是在协议里写死一个过窄白名单。

对当前 `game1/default.project.json`，工程声明的 Studio 顶层目标包括：

- `ReplicatedStorage`
- `Workspace`

其中已显式声明的代码/工程容器包括：

- `ReplicatedStorage/ClockPRealTest`

因此 `game1` 的第一版 `code_sync` 至少必须允许把 managed root 放到这些已声明目标下，不能只开放 `ReplicatedStorage` / `ServerScriptService` / `StarterPlayerScripts` 这类通用代码位置。

### Studio target allowlist 来源

第一版建议由 CLI 在读取 workspace 时构造 `studio_target_allowlist`：

1. 解析 project 声明文件中的 DataModel tree。
2. 收集 project 显式声明的 sync target path。
3. 校验 `code-sync.roots.json` 里的每个 `studio_path` 都等于某个 sync target，或落在某个允许 descendant 的 sync target 内。
4. 把规范化后的 allowlist 摘要纳入 `code_sync_config_hash`。

也就是说，是否允许同步到 `Workspace`，由当前工程声明文件是否声明了 `Workspace` 决定。当前 `game1` 已声明 `Workspace`，所以第一版必须支持。

sync target 的粒度规则：

- DataModel service 节点只是 service 存在性声明时，不自动放开该 service 下任意路径。
- 若 service 节点自身带 `$path`，或该 service 在当前 project 声明中作为整棵 service 目标声明，则该 service 可作为允许 descendant 的 sync target。
- service 下显式声明的 container path 可作为允许 descendant 的 sync target。
- 当前 `game1` 中，`ReplicatedStorage/ClockPRealTest` 是显式 container target；`Workspace` 是显式 service target。
- 当前 `game1` 中，不能仅因为 `ReplicatedStorage` service 存在，就自动允许 `ReplicatedStorage/Shared`、`ReplicatedStorage/Packages` 或 `ReplicatedStorage/rbxts_include`。如果要托管这些路径，需要先在 project 声明文件或 `code-sync.roots.json` 配置策略中显式声明它们。

### Studio target 边界

每个 managed root 必须满足：

- `studio_path` 使用路径段数组表达。
- `studio_path` 必须落在当前工程声明的 Studio target allowlist 内。
- 不同 managed root 的 `studio_path` 不能相同。
- 不同 managed root 之间不能互为祖先 / 后代。
- `delete_path`、`ensure_container`、`upsert_script` 都必须被限制在对应 `root_id` 的 managed root 内。
- 协议不得允许任何 op 通过 `..`、空路径段、字符串拼接等方式逃逸 managed root；路径只接受已经解析好的 `string[]`。

### target 不存在或形态不一致

第一版采用和现有工程声明体验接近、但边界更明确的规则：

- DataModel service 自身必须存在，不由 `code_sync` 创建。
- service 下的 managed root 若不存在，可以由 flush 创建。
- managed root 内的对象形态若和本地逻辑树不一致，第一版允许按声明替换。
- 替换只能发生在 managed root 内，不能替换 DataModel service 自身。

示例：

- `ReplicatedStorage/ClockPRealTest` 不存在：允许创建。
- `ReplicatedStorage/ClockPRealTest` 已存在但类型不符合本地逻辑树：允许在该 root 边界内替换。
- `Workspace` 作为 service：不能删除或替换 service 本身，但可以托管其下显式声明的 managed root。

### 同名节点

Roblox 允许同一个 parent 下存在多个同名 child，但第一版 `code_sync` 不处理这个复杂度。

规则：

- mcp2 live 查询 managed root 时，如果同一个 parent 下出现多个同名 `Folder` / `ModuleScript` / `Script` / `LocalScript`，直接失败。
- 推荐错误码：`code_sync_ambiguous_remote_tree`。
- CLI 不尝试猜测、不尝试按 class name disambiguate、不自动删除其中任何一个。

这样逻辑路径才能保持确定性。

## 文件系统到 Studio 的映射规则

第一版不发明新规则，直接采用一个明确的 Lua/Luau project mapping 子集：

```text
mapping_profile = rojo_lua_v1
```

`rojo_lua_v1` 是历史兼容 profile 名称，只用于描述既有 `init.*` / 脚本后缀映射规则，不表示运行时依赖 Rojo。

支持规则：

- `*.server.lua` / `*.server.luau` -> `Script`
- `*.client.lua` / `*.client.luau` -> `LocalScript`
- `*.lua` / `*.luau` -> `ModuleScript`
- `init.server.lua` / `init.server.luau` -> containing directory usurp 为 `Script`
- `init.client.lua` / `init.client.luau` -> containing directory usurp 为 `LocalScript`
- `init.lua` / `init.luau` -> containing directory usurp 为 `ModuleScript`

其中“usurp”语义沿用既有 project mapping 约定：

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

hash 规则必须跨 CLI / helper2 / mcp2 稳定一致。第一版不使用 JSON stringify 作为 hash 输入，因为不同语言、字段顺序和转义细节容易产生不一致。

Studio / mcp2 侧使用 Roblox 内置 `EncodingService` 计算 BLAKE3：

```lua
local EncodingService = game:GetService("EncodingService")
local digest = EncodingService:ComputeStringHash(canonicalInput, Enum.HashAlgorithm.Blake3)
```

`ComputeStringHash` 返回 binary string，协议里统一转成小写 hex。CLI 侧使用 Python `blake3` 包，对同一 canonical input 计算 digest。

统一约定：

- hash 输出为小写 hex 字符串。
- 所有字符串先按 UTF-8 编码。
- 文本 Source 先按本文本规范化规则统一为 LF。
- children 排序固定为按 `name` 的 UTF-8 字节序升序；若 `name` 相同，再按 `entry_kind` 的 UTF-8 字节序升序。
- 若同一 parent 下出现多个同名 managed-kind 节点，live 查询直接失败，不进入 hash 计算。

### canonical encoding

hash 输入使用长度前缀字符串拼接：

```text
S(value) = utf8_byte_length(value) + ":" + utf8_bytes(value)
```

示例：

```text
S("abc") = 3:abc
S("") = 0:
S("模块") = 6:模块
```

数字字段先转为十进制 ASCII 字符串，再用 `S(...)` 编码。

所有 hash 输入都以 domain tag 开头，避免不同层级碰撞复用：

```text
clockp.code_sync.v1.script
clockp.code_sync.v1.folder
clockp.code_sync.v1.root
clockp.code_sync.v1.combined
clockp.code_sync.v1.config
```

### 脚本节点 hash

脚本节点指最终落到 Studio 的 `ModuleScript` / `Script` / `LocalScript` 实例。

脚本节点 hash 计算输入至少包括：

- 节点类型：`ModuleScript` / `Script` / `LocalScript`
- 节点名
- 规范化后的 `Source`
- 排序后的直接 children summary

脚本节点必须支持 children。原因是 `init.*` usurp 语义会把目录表现为脚本实例，同时把该目录下其他子节点挂到这个脚本实例下面。

也就是说：

- 仅内容相同但脚本类型不同，hash 必须不同。
- 仅名字不同但内容相同，hash 也必须不同。
- 脚本 children 增删改，hash 也必须不同。

精确定义：

```text
script_hash = BLAKE3_HEX(
  S("clockp.code_sync.v1.script") +
  S(entry_kind) +
  S(name) +
  S(normalized_source) +
  S(child_count_decimal) +
  child_summary_1 +
  child_summary_2 +
  ...
)
```

其中 `entry_kind` 只能是：

- `ModuleScript`
- `Script`
- `LocalScript`

`child_summary` 和排序规则与目录节点一致。

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

精确定义：

```text
folder_hash = BLAKE3_HEX(
  S("clockp.code_sync.v1.folder") +
  S(name) +
  S(child_count_decimal) +
  child_summary_1 +
  child_summary_2 +
  ...
)
```

每个 child summary：

```text
child_summary =
  S(child_name) +
  S(child_entry_kind) +
  S(child_entry_hash)
```

排序规则：

- 先按 `child_name` 的 UTF-8 字节序升序。
- 再按 `child_entry_kind` 的 UTF-8 字节序升序。

路径本身不直接进入单个节点 hash。节点移动会改变旧 parent 和新 parent 的 children summary，因此最终 root hash 仍会变化。

### 根 hash

每个 managed root 都有自己的 `root_hash`。

此外再计算一个 `combined_hash`，由所有 root 的：

- `root_id`
- `root_hash`

排序后再 hash 得到。

`combined_hash` 用于表示“本次 managed roots 整体视图”。

精确定义：

```text
root_hash = BLAKE3_HEX(
  S("clockp.code_sync.v1.root") +
  S(root_id) +
  S(mapping_profile) +
  S(root_kind) +
  S(root_node_hash)
)
```

其中：

- `root_kind` 是 root 逻辑节点类型。
- `root_node_hash` 是 root 对应逻辑节点自身的 `folder_hash` 或 `script_hash`。

`combined_hash`：

```text
combined_hash = BLAKE3_HEX(
  S("clockp.code_sync.v1.combined") +
  S(mapping_profile) +
  S(root_count_decimal) +
  root_summary_1 +
  root_summary_2 +
  ...
)
```

每个 root summary：

```text
root_summary = S(root_id) + S(root_hash)
```

roots 排序规则固定为按 `root_id` 的 UTF-8 字节序升序。

注意：

- `combined_hash` 的作用域，只覆盖当前 task contract 的 `code_sync.roots[]`，或请求中显式选择且已被 helper2 校验存在的 `root_ids[]` 子集
- 计算时按 `root_id` 排序，不依赖请求传入顺序
- `mapping_profile` 变化时，`combined_hash` 也视为不同语义下的值，不能混用

### 配置 hash

`code_sync_config_hash` 用于绑定 task-agent / helper2 / CLI 三方看到的是同一份同步配置。

第一版配置 hash 覆盖：

- `protocol_version`
- `project_id`
- `mapping_profile`
- 规范化后的 Studio target allowlist 摘要
- `roots[]` 中每个 root 的 `root_id`、`local_path`、`studio_path`、`include`、`exclude`

配置 hash 同样使用 canonical encoding，不使用 JSON stringify。

`roots_authority_hash` 单独覆盖 helper2 / mcp2 写入边界所需的权威表：

- `root_id`
- `studio_path`

`roots_authority_hash` 不包含 `local_path`、`include`、`exclude`，因为 helper2 / mcp2 不读取本地文件系统；它们只需要证明请求引用的 `root_id` 对应哪个 Studio managed root。

建议定义：

```text
config_hash = BLAKE3_HEX(
  S("clockp.code_sync.v1.config") +
  S(protocol_version_decimal) +
  S(project_id) +
  S(mapping_profile) +
  S(target_count_decimal) +
  target_summary_1 +
  ... +
  S(root_count_decimal) +
  root_config_summary_1 +
  ...
)
```

其中 target summary：

```text
target_summary = S(path_segment_count_decimal) + S(segment_1) + S(segment_2) + ...
```

target summary 按完整路径段的 UTF-8 字节序逐段升序。

root config summary：

```text
root_config_summary =
  S(root_id) +
  S(local_path_normalized) +
  S(studio_path_segment_count_decimal) + S(studio_segment_1) + ... +
  S(include_count_decimal) + S(include_1) + ... +
  S(exclude_count_decimal) + S(exclude_1) + ...
```

roots 按 `root_id` 升序；`include` / `exclude` 按 UTF-8 字节序升序。`local_path_normalized` 使用 `/` 作为分隔符，不允许绝对路径，不允许 `..` 逃逸 workspace。

root authority hash：

```text
roots_authority_hash = BLAKE3_HEX(
  S("clockp.code_sync.v1.roots_authority") +
  S(root_count_decimal) +
  root_authority_summary_1 +
  ...
)
```

每个 root authority summary：

```text
root_authority_summary =
  S(root_id) +
  S(studio_path_segment_count_decimal) + S(studio_segment_1) + ...
```

root authority summary 同样按 `root_id` 升序。

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
- `place_id`
- `machine_name`
- `project_id`
- `code_sync_config_hash`
- `mapping_profile`
- `roots_authority_hash`

其中：

- 第一版 `protocol_version` 固定为 `1`
- helper2 或 mcp2 若发现版本不支持，直接返回 `code_sync_protocol_version_unsupported`
- 这些字段组成统一 binding envelope。各 handler 不再各自挑字段校验，而是先校验完整 envelope，再处理具体业务字段。
- `root_id -> studio_path` 只能来自 helper2 保存的 task contract 权威表，不能来自请求 body。

所有 task API 请求还必须带：

- 路径中的 `task_id`
- 请求头 `X-ClockP-Task-Token`

helper2 必须在转发 mcp2 command 时带上：

- `command_id`
- `task_id`
- `studio_instance_id`
- 完整 binding envelope
- 由 helper2 展开的 managed root 权威信息

mcp2 response 必须回传同一个 `command_id`、`task_id` 和 `studio_instance_id`。helper2 只接受当前 live task session、当前 task-owned Studio 实例、当前未过期 command 的 response；session 变化、Studio 重连或 command 超时后的旧 response 一律丢弃。

非 task-owned Studio 不允许 pull 或执行 `code_sync` command。

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

### 规模目标

第一版按约 1000 个代码文件的工程规模设计。

这意味着：

- CLI 本地扫描可以每次全量扫描，不需要本地缓存。
- 远端查询不能按“一个文件一次请求”实现。
- 常见情况下，flush 往返次数应主要接近逻辑树最大深度，而不是文件数量。
- `query_tree` 必须支持一批 frontier 节点合并查询。
- `apply` 第一版支持客户端分批、服务端无状态 apply；每个 batch 必须在发送前估算 JSON body 体积，超过目标上限时继续拆分。
- 若单个 op 自身超过目标上限，才返回 `code_sync_apply_payload_too_large`。

目标行为：

```text
约 1000 个文件、常规目录宽度和深度：
1 次 get_manifest
D 轮 query_tree，其中 D 接近脏子树最大深度
N 次 apply batch，其中 N 由 8 MiB 目标上限决定
1 次最终 get_manifest
```

非目标行为：

```text
1000 个文件 -> 1000 次 query_tree
1000 个文件 -> 1000 次 apply
```

若某个单独目录 direct children 极宽，允许按 `child_offset` 分页；这是宽目录的例外路径，不应影响常规树形工程的主流程。

### 稳定 edit 与 mode_seq

`code_sync` 只允许在稳定 `edit` 态执行。第一版定义：

- mcp2 负责从 Studio 侧观测当前模式，至少区分 `edit` / `play` / `transitioning`。
- helper2 维护当前 task-owned Studio 的 `studio_instance_id` 和 `mode_seq`。
- 每次 Studio 从 edit 进入 play、从 play 回到 edit、Studio 进程重连、mcp2 command broker 重连、task-owned Studio 实例变化时，helper2 都必须递增 `mode_seq`。
- `transitioning` 态一律视为不稳定，返回 `code_sync_not_in_edit`。
- `get_manifest` / `query_tree` 返回的 `mode_seq` 是本次 live 观察的序号。
- `apply` 必须带 `expected_mode_seq`；即使 Studio 曾经 play 后又回到 edit，只要 `mode_seq` 已变化，也必须返回 `code_sync_state_stale`，要求 CLI 重新 `get_manifest` / diff。

这保证“看起来又回到 edit”不会复用 play 前的旧 diff。

## code_sync_get_manifest

用途：

- 校验当前为稳定 `edit`
- 校验 binding
- 获取远端当前 managed roots 的总览摘要

请求包含：

- binding envelope
- 可选 `root_ids[]`

其中：

- 不传 `root_ids[]` 表示查询当前 task contract 中全部 managed roots。
- 传 `root_ids[]` 时，helper2 必须逐个校验它们存在于 `code_sync.roots[]` 权威表。
- mcp2 只能收到 helper2 从权威表展开后的 `root_id -> studio_path`，不能直接相信 CLI 传入的 `studio_path`。

返回：

- `mode`
- `mode_seq`
- `studio_instance_id`
- `project_id`
- `code_sync_config_hash`
- `mapping_profile`
- `roots_authority_hash`
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

- binding envelope
- `queries[]`

其中每个 query 至少包含：

- `root_id`
- `studio_rel_path`
- `child_offset`

helper2 必须校验每个 `root_id` 存在于 `code_sync.roots[]` 权威表，并把对应 `studio_path` 展开给 mcp2；mcp2 必须拒绝越过该 root 的任何 `studio_rel_path`。

返回：

- `mode_seq`
- `studio_instance_id`
- `roots_authority_hash`
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

- binding envelope
- `expected_mode_seq`
- `expected_studio_instance_id`
- `expected_combined_hash_before`
- `ops[]`

其中：

- `expected_mode_seq` 来自最近一次 `get_manifest` 或 `query_tree`
- `expected_studio_instance_id` 来自最近一次 `get_manifest` 或 `query_tree`
- `expected_combined_hash_before` 来自最近一次 `get_manifest`

`apply` 前必须同时检查：

- 当前仍是稳定 `edit`
- 当前 `mode_seq == expected_mode_seq`
- 当前 `studio_instance_id == expected_studio_instance_id`
- 当前 `combined_hash == expected_combined_hash_before`
- `ops[]` 里的每个 `root_id` 都存在于 `code_sync.roots[]` 权威表
- 每个 op 的目标路径都落在对应 `root_id` 的 managed root 内

状态校验不满足时，不得开始写入，直接返回：

- `code_sync_state_stale`

root 校验不满足时，不得开始写入，并按具体问题返回：

- `code_sync_unknown_root`
- `code_sync_root_boundary_violation`

这样可以 fail closed 地挡住：

- 手工改了 managed root
- 别的调用方先 flush 了
- Studio 中途进过 play / stop，哪怕后来又回到 edit

### apply 传输粒度

第一版 `code_sync_apply` 支持“一个 flush 多个 apply batch”：

- 不做 server 端 apply session
- 不做断点续传
- 不做后台队列

原因：

- 约 1000 个文件的首次同步可能超过单次 8 MiB 目标上限。
- 仅靠“超限失败”不能满足第一版可用目标。
- 引入服务端 apply session / 断点续传会明显增加复杂度，第一版不做。

因此第一版采用客户端分批、服务端无状态 apply：

- CLI 把完整 `ops[]` 按顺序切成多个 batch。
- 每个 batch 都是一次普通 `code_sync_apply` 请求。
- 每个 batch 的请求 / 响应仍必须满足 8 MiB 目标上限和 10 MiB hard cap。
- 每个 batch apply 前都检查稳定 `edit`、`studio_instance_id`、`mode_seq` 和 `expected_combined_hash_before`。
- 每个 batch 成功后返回新的 `combined_hash_after`。
- 下一 batch 的 `expected_combined_hash_before` 必须使用上一 batch 的 `combined_hash_after`。
- 下一 batch 的 `expected_mode_seq` 和 `expected_studio_instance_id` 必须继续使用当前 live 观察值；若任一变化，必须停止并重新 diff。
- 最后一 batch 完成后，CLI 再调用 `get_manifest` 做最终 live hash 验证。

这样每个 batch 仍然是可验证的小事务，但整体 flush 不需要服务端保存 apply session。

分批约束：

- CLI 不得拆分单个 op。
- 若单个 `upsert_script` op 自身编码后超过 8 MiB 目标上限，直接返回 `code_sync_apply_payload_too_large`。
- `delete_path` 必须先于 `ensure_container`，`ensure_container` 必须先于 `upsert_script`。批次边界不能破坏这个全局顺序。
- 如果某个 batch 失败，CLI 必须停止，重新 `get_manifest` 后再决定是否重新 diff；不能基于旧 diff 继续发送后续 batch。

若后续需要跨进程恢复、断点续传或后台 apply，再单独设计 `apply_session`，不在第一版预埋半套协议。

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
- `code_sync_protocol_version_unsupported`
  - `protocol_version` 不是 helper2 / mcp2 支持的版本
- `code_sync_auth_failed`
  - `task_id`、`X-ClockP-Task-Token`、`machine_name` 或 task owner 校验失败
- `code_sync_binding_mismatch`
  - binding envelope 任一字段与当前 task contract 不一致，包括 `workspace_id`、`place_id`、`machine_name`、`project_id`、`mapping_profile`、`code_sync_config_hash`、`roots_authority_hash`
- `code_sync_unknown_root`
  - 请求引用了 task contract 的 `code_sync.roots[]` 中不存在的 `root_id`
- `code_sync_root_boundary_violation`
  - `query_tree` 或 `apply` 试图越过 managed root 边界
- `code_sync_ambiguous_remote_tree`
  - live tree 中同一 parent 下出现多个同名 managed-kind 节点，无法确定逻辑路径
- `code_sync_invalid_config`
  - 本地配置文件非法
- `code_sync_unsupported_encoding`
  - 文本不是 UTF-8
- `code_sync_unsupported_mapping`
  - 命中了第一版不支持的文件或规则
- `code_sync_remote_query_failed`
  - manifest / query_tree live 查询失败
- `code_sync_state_stale`
  - `studio_instance_id`、`mode_seq` 或 `combined_hash` 已不是本次 diff 观察到的那个
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

对当前 `game1/default.project.json`，工程已声明的 Studio target 是：

- `ReplicatedStorage/ClockPRealTest`
- `Workspace`

因此第一版无需修改工程声明文件时，应先只在这些已声明 target 下配置 managed roots。

如果后续要托管常见 roblox-ts 输出目录，必须先把对应 Studio target 显式加入 `game1/default.project.json` 或等价工程声明，再配置：

- `.ts-out/packages/rewatch -> ReplicatedStorage/Packages/rewatch`
- `include -> ReplicatedStorage/rbxts_include`
- `.ts-out/shared -> ReplicatedStorage/Shared`
- `.ts-out/server -> ServerScriptService`
- `.ts-out/client -> StarterPlayer/StarterPlayerScripts`

这些路径不是当前 `game1/default.project.json` 已声明 target，不能仅因为 `ReplicatedStorage` service 存在就默认放行。

`node_modules/@rbxts` 不建议第一版整棵搬运。若运行期确实需要，只纳入实际会进入 Studio 的 Lua/Luau 子集，不把 `.d.ts`、README、LICENSE、`package.json` 一起带进去。

## Rojo 主线废弃原则

从本设计开始，Rojo 不再是 task-agent 链路的一部分，也不再是 code flush 的前置条件、fallback 或验收依据。

必须删除或废弃的主线行为：

- task-agent 不启动 Rojo、不注册 Rojo public route、不等待 Rojo ready、不把 Rojo 状态写入 `.clock-p/session.json`。
- helper2 不再依赖 `rojo_upstream_url` 判定 task session live。
- helper2 当前主线不再暴露 `/v1/rojo/config`、`/rojo-forward/*` 作为 task 会话能力。若历史兼容代码暂未物理删除，必须明确标记为 deprecated，且 code-sync 主线不可调用。
- Studio 验证不再看 Rojo connected、Rojo initial sync 日志或 Rojo 插件状态。
- bridge2 flush / dry-run / apply 不允许在 code-sync 失败时 fallback 到 Rojo。

保留但改名义的内容：

- `default.project.json` 可继续作为工程 DataModel 声明源，但它不是运行时 Rojo project。
- `rojo_lua_v1` 仅作为历史兼容 mapping profile 名称，描述文件到 Studio 逻辑树的映射规则。

## Phase 计划

实现语言与归属：

- task-agent 使用 Go，负责 session、heartbeat、code-sync binding 计算，不负责任何 Rojo supervisor。
- helper2 协议面、task binding、mcp2 command broker 使用 Go。
- flush CLI / 本地扫描 / diff 编排使用 Python 3，和 `tools/bridge2` 放在一起。
- Studio live tree 查询和 apply 使用 `plugin-mcp2` Luau。
- 不在第一版引入新的独立服务进程。

每个 Phase 合并前必须做 3 个 reviewer 视角审查：

- Reviewer A：session / binding / 安全边界，确认 task live、workspace、config hash 不可串线。
- Reviewer B：Studio / helper2 / mcp2 运行时，确认 edit 态、task-owned Studio、command pull / response 链路成立。
- Reviewer C：CLI / diff / 测试规模，确认 1000 文件目标、错误码、可观测输出和回归测试覆盖。

reviewer 意见必须逐条判断并记录处理结论；若意见会显著增加复杂度，需要回到“code-sync-only、1000 文件可用闭环”的目标判断是否值得。

### Phase 0：文档与入口收敛

目标：先把所有设计、README、skill 和 CLI 文档收敛到 code-sync-only 主线，废弃旧 Rojo phase。

范围：

- 更新 README、`skills/roblox-agent/SKILL.md`、bridge2 帮助文案和 task-agent 启动示例。
- 删除或标记 deprecated：`--rojo-bin`、运行时 `--project`、Rojo public route、Rojo ready 相关说明。
- 明确 `.clock-p/session.json` 的 `code_sync` schema 与 heartbeat binding 字段。
- 明确 `code-sync.roots.json` 和 `.clock-p/session.json` 独立存在，flush 时二者都要读取并对账。

验证：

- `rg -n "rojo|Rojo" README.md skills docs pr-list tools helper2 task-agent util`，剩余引用必须只属于废弃说明、历史 profile 名称或工程声明兼容说明。
- 文档中不再出现“等待 Rojo ready”“Rojo initial sync 验收”“Rojo fallback”作为当前做法。

### Phase 1：task-agent / helper2 会话契约改造

目标：task-agent 不依赖 Rojo 也能把 task session 置为 live，并让 helper2 拉起 task-owned Studio。

范围：

- task-agent 启动时读取 `code-sync.roots.json` 和工程声明文件，计算 `workspace_id`、`project_id`、`mapping_profile`、`code_sync_config_hash`。
- task-agent 启动后立即写 `.clock-p/session.json`，其中只包含 `code_sync` binding、`code_sync.roots[]` 权威表和 `task_session_token`，不包含 `rojo` 对象。
- task-agent 立即向 helper2 发送 heartbeat，不等待任何代码同步服务。
- helper2 的 task session contract 删除 `rojo_upstream_url`，新增 `code_sync` binding。
- helper2 收到合法 heartbeat 后将 task session 置为 live，并通过 `EnsureTaskDesired(task_id, place_id)` 拉起 task-owned Studio。
- 同一 `task_id` 下若 task contract 不可变元组变化，helper2 返回 binding mismatch，要求启动新 task session。

验证：

- Go 单测覆盖：无 Rojo 二进制时 task-agent 可以启动并写 session；heartbeat 可使 helper2 task session live；`place_id` / `machine_name` / root authority / config hash 变化被拒绝；旧 `rojo_upstream_url` 不再是必填项。
- 本机手工验证：不安装、不启动 Rojo，仅启动 task-agent 后，`bridge2 status` 能看到 live task session 和 task-owned Studio 期望状态。

### Phase 2：bridge2 binding 对账与本地 manifest

目标：把本地 code-sync 视图做准，并让所有 live / dry-run / apply 命令在进入 Studio 前先完成 binding 对账。

范围：

- `code-sync-manifest` 只读取本地 flush 配置，可用于离线生成 local manifest。
- `code-sync-dry-run` / `code-sync-apply` 同时读取 `.clock-p/session.json` 和 `code-sync.roots.json`，重新计算 `code_sync_config_hash` 并与 session 对账。
- 所有 helper2 code-sync 请求携带完整 binding envelope 和 `X-ClockP-Task-Token`。
- 读取工程声明文件，生成 Studio target allowlist；该声明只作为静态边界，不作为 Rojo runtime。
- 实现 canonical hash：config / leaf / folder / root / combined。

验证：

- Python 单测覆盖 hash fixture、init usurp、CRLF/LF、非法 UTF-8、root 重叠、工程 target allowlist、session/config hash mismatch、unknown root。
- Go/Python 共享 fixture 验证 `code_sync_config_hash` 一致。
- 人工对 `K:\roblox_space\game1` 跑 local manifest，确认 `Workspace` 只因工程声明存在而被放行。

### Phase 3：helper2 / mcp2 原生 live manifest

目标：打通原生 Studio live tree 查询，但不写 Studio，也不依赖 `studio_run_code` 作为最终协议。

范围：

- helper2 新增 task API：`code-sync/get-manifest`、`code-sync/query-tree`，并统一校验 request binding。
- mcp2 新增 command kind：`code_sync_get_manifest`、`code_sync_query_tree`。
- mcp2 在稳定 edit 态下查询 managed roots，返回同一 hash 规则下的 live summary。
- helper2 转发 command 时附带 `command_id`、`task_id`、`studio_instance_id` 和 root authority；旧 response、非 task-owned Studio response 必须丢弃。
- live manifest 响应返回 `code_sync_config_hash`、`mapping_profile`、`roots_authority_hash`、`studio_instance_id`、`mode_seq`、`combined_hash`。
- 同名 managed-kind 节点返回 `code_sync_ambiguous_remote_tree`。

验证：

- Go 单测覆盖 handler、binding 失败、auth 失败、unknown root、非 edit 失败、协议版本失败、task session not live、旧 response 丢弃。
- Luau 侧通过 helper2 原生命令在 Studio edit 态查询 manifest。
- play 态查询返回 `code_sync_not_in_edit`。
- play/stop 后即使回到 edit，旧 `mode_seq` 的 apply 也返回 `code_sync_state_stale`。
- 验证链路中不调用 Rojo public route，也不要求 Rojo 插件存在。

### Phase 4：apply 与最终 hash 验证

目标：完成 code-sync-only 的写入闭环：把 managed roots 写入 Studio，并用最终 live manifest hash 验证成功。

范围：

- helper2 新增 `code-sync/apply`，统一校验 request binding。
- mcp2 实现 `delete_path`、`ensure_container`、`upsert_script`，删除和写入都必须限制在 root authority 内。
- apply 前校验稳定 edit、`expected_studio_instance_id`、`expected_mode_seq`、`expected_combined_hash_before`。
- apply 支持客户端分批、服务端无状态 batch；每个 batch 都执行 payload 上限和 stale 校验。
- apply 后 bridge2 再调 `get-manifest`，最终 `combined_hash` 必须与本地目标一致。
- helper2 保存最近一次 flush result 摘要，供 status / debug 查询。

验证：

- 本地 Studio edit 态 flush 成功。
- play 态 flush 返回 `code_sync_not_in_edit`。
- apply 前远端变化返回 `code_sync_state_stale`。
- 单个 op payload 超过上限返回 `code_sync_apply_payload_too_large`。
- 越过 managed root 边界返回 `code_sync_root_boundary_violation`。
- apply 后 hash 不一致返回 `code_sync_verify_failed`。
- 验证 task-agent 全程未启动 Rojo，Studio 侧未安装 Rojo 插件也能完成 flush。

### Phase 5：ops / query_tree / 规模化完成

目标：把 Phase 4 的写入闭环压到 1000 文件可用规模，验证 query_tree / apply batch 不退化。

范围：

- bridge2 对 hash 不一致的 root 分轮批量 `query_tree`，避免逐文件请求。
- 生成确定顺序 ops，并按 8 MiB 目标上限切 apply batch。
- 实现 query / apply 分批、payload 估算和宽目录分页。
- last-result 记录包含差异数量、写入数量、失败错误码、最终 hash。

验证：

- 1000 文件 fixture 不退化成逐文件请求；常规树形工程的 query_tree 往返数应接近脏子树最大深度 `D`，不得接近文件数 `N`。
- 单次请求 / 响应目标上限 `<= 8 MiB`，hard cap `10 MiB`；测试要覆盖 apply batch 拆分和单 op 超限。
- 人工构造新增、修改、删除、Folder/Script 互换、init usurp 差异。
- dry-run 输出能定位前 N 个差异路径。
- apply 后最终 live manifest 与本地 combined hash 一致。

### Phase 6：game1 / test_game3 接入与 Studio 验收

目标：用真实项目证明 code-sync-only 主线可用，验收不再借 Rojo。

范围：

- 为 `test_game3` 和 `game1` 增加实际 `code-sync.roots.json`。
- 对齐 `default.project.json` 或等价工程声明中的 Studio target。
- 调整 bridge2 / task-agent 启动脚本，确保默认就是 code-sync 模式。
- 删除测试脚本里对 Rojo 二进制、Rojo 插件、Rojo initial sync 日志的依赖。

验证：

- 不安装、不启动 Rojo，启动 helper2、task-agent、task-owned Studio。
- `bridge2 status` 显示 task session live，且 live 来源是 helper2 heartbeat，不是 Rojo。
- `bridge2 code-sync-dry-run` 能读到 Studio live manifest。
- `bridge2 code-sync-apply` 能完成 flush，并通过最终 `combined_hash` 验证。
- flush 后 `play`，通过 play logs 或明确 runtime 证据确认代码运行。
- 验收记录必须包含：执行命令、session binding 摘要、最终 `combined_hash`、last-result 摘要、play log 关键行或日志文件路径。
- 回归 `test_game3` 删除 Rojo 世界的验证：无 Rojo 也能完成 status / mode / dry-run / apply / play 链路。

Reviewer 节点：

- Phase 6 开始前，3 个 reviewer 审 managed root 边界和真实项目接入范围。
- Phase 6 完成后，3 个 reviewer 审 Studio 验收记录，确认没有把 Rojo 当隐含前置条件。
