# Code Sync Node Tree

归档说明：这份设计记录保留历史讨论脉络，不再作为当前实现判定依据。当前 code-sync 事实以 `README.md`、`skills/roblox-agent/SKILL.md` 和 `tools/bridge2` / `go-helper` / `plugin-mcp2` 源码为准。

## 目标

把 code-sync 配置从旧的 `roots` 列表改成按 Studio DataModel 形状书写的 `tree`。bridge2 和 task-agent 负责解释配置、扫描本地文件、推导文件 kind、组合 node/subnode，并把最终目标树发给 helper2/mcp2。helper2/mcp2 不理解 ownership / subnode 持有关系，只负责查询 Studio 子树和把给定目标树增量写入 Studio。

本改动不做旧 `roots` 兼容，不做迁移层。旧格式直接失败。

## 概念

- `node`：配置树里带 `$local_path` 的 Studio 节点。
- `file`：从某个 node 的 `$local_path` 扫描出来、映射成 Studio 节点的本地文件。
- `subnode`：Studio 路径落在另一个 node 之下的 node。

语义：

- 每个 node 持有自己 Studio 节点之下的整棵子树。
- 如果后代路径上声明了 subnode，那么该后代子树由 subnode 持有，父 node 不再持有那一段。
- Studio path 不能重复。
- local path 可以重复；同一份本地目录可以复制到多个不同 Studio 位置。
- mcp2 必须接收最终目标树里每个条目的 `kind`，但 kind 由 agent 推导或读取 node `$kind` 后写入 payload，mcp2 不自行推导。
- 最终 apply target 的身份就是 canonical Studio path。不存在用户配置或协议语义上的 `root_id`。

Canonical Studio path：

- `canonical_studio_path = "studio-path-v1:" + concat(segment_part)`。
- `segment_part = byte_length_utf8(segment) + ":" + segment`。
- 例：`["ReplicatedStorage", "rbxts_include"]` 派生为 `studio-path-v1:17:ReplicatedStorage13:rbxts_include`。
- canonical Studio path 只作为 hash / binding / 排序身份，不要求人手写。
- target hash 直接包含 Studio path。
- combined hash 按 canonical Studio path 排序。
- Python bridge2 与 Go task-agent 必须用同一组 golden fixture 验证 canonical Studio path、target hash 和 combined hash。

## 配置格式

配置文件仍由 `clock-p.workspace.json.code_sync_config` 指向。默认文件名从 `code-sync.roots.json` 改为 `code-sync.tree.json`。

```json
{
  "tree": {
    "ReplicatedStorage": {
      "rbxts_include": {
        "$local_path": "include",
        "$include": ["**/*.lua", "**/*.luau"],
        "$exclude": [],

        "node_modules": {
          "@rbxts": {
            "services": {
              "$local_path": "node_modules/@rbxts/services",
              "$include": ["**/*.lua", "**/*.luau"]
            }
          }
        }
      }
    },
    "ServerScriptService": {
      "TS": {
        "$local_path": ".ts-out/server"
      }
    },
    "StarterPlayer": {
      "StarterPlayerScripts": {
        "TS": {
          "$local_path": ".ts-out/client"
        }
      }
    }
  }
}
```

元字段：

- `$local_path`：workspace 相对路径。出现该字段的配置点就是 node。
- `$kind`：可选，只作用于当前 node 自己，不继承，不影响子 node 或 file。允许值：`Folder`、`ModuleScript`、`Script`、`LocalScript`。
- `$include`：可选，默认 `["**/*.lua", "**/*.luau"]`。
- `$exclude`：可选，默认 `[]`。

普通 key 都是 Studio child name。以 `$` 开头的普通 Studio 节点名第一版不支持，避免和元字段冲突。JSON 解析必须拒绝同一 object 内重复 key，不能让后者静默覆盖前者；Python 不能直接用普通 `json.loads`，Go 不能直接用普通 `json.Unmarshal` 跳过这个检查。

Service 节点不能声明 `$local_path` 或 `$kind`。Service 只作为路径锚点，不能被创建、替换或删除。没有 `$local_path` 的普通中间节点只是配置锚点，不能声明 `$kind` / `$include` / `$exclude`。

## Kind 规则

node kind：

- 如果 node 写了 `$kind`，使用 `$kind`。
- 如果 node 没写 `$kind`，默认是 `Folder`。
- 如果 node 没写 `$kind`，且本地目录存在 `init.lua` 或 `init.luau`，允许 `init.*` usurp node 自己，node kind 由 `init.*` 文件名推导。
- 如果 node 写了 `$kind: "Folder"`，则 `init.*` 不允许 usurp node 自己；发现会直接失败。
- 如果 node 写了 `$kind: "ModuleScript"` / `"Script"` / `"LocalScript"`，本地目录必须存在一个 `init.*` 给 node 自己提供 Source；否则失败。
- 显式脚本 `$kind` 和 `init.*` 推导 kind 必须一致；例如 `$kind: "Script"` 需要 `init.server.lua` 或 `init.server.luau`，不能用普通 `init.lua`。
- node 自己被 `init.*` usurp 时，该 `init.*` 文件只作为 node 的 Source，不再作为 child 出现。
- `init.*` 候选集合固定为：`init.lua`、`init.luau`、`init.server.lua`、`init.server.luau`、`init.client.lua`、`init.client.luau`。

file kind：

- file 不能在配置里指定 kind。
- `*.server.lua` / `*.server.luau` 映射为 `Script`。
- `*.client.lua` / `*.client.luau` 映射为 `LocalScript`。
- 其他 `*.lua` / `*.luau` 映射为 `ModuleScript`。
- `init.*` 继续使用目录 usurp 规则。

冲突：

- 同一 node 扫描后，如果两个文件映射到同一个 Studio child，直接失败。
- Studio path 重复，直接失败。
- local path 重复允许。

## Agent 流程

1. 读取 `code-sync.tree.json`。
2. 校验配置树：
   - 顶层 key 必须是受支持的 DataModel service。
   - service 不能带 `$local_path` / `$kind`。
   - 同一 JSON object 内不能有重复 key。
   - 没有 `$local_path` 的普通中间节点不能带任何 `$` 元字段。
   - Studio path 不重复。
   - `$local_path` 必须是 workspace 相对路径，不能逃逸 workspace。
3. 收集所有 node，并按 Studio path 建 node/subnode 关系。
4. 每个 node 独立扫描自己的 `$local_path`，按 include/exclude 生成逻辑树。
5. agent 把 subnode 目标树 graft 到父 node 对应后代位置。父 node 原本扫描出的同路径内容被 subnode 替换，这不是冲突。
6. 只把不在其他 node 之下的顶层 node 作为最终 apply target 发给 helper2/mcp2；每个 target 以 `studio_path` 作为唯一身份。
7. 本地 manifest、live manifest、dry-run diff、apply payload、最终 verify 全部基于这些最终 apply target。

helper2/mcp2 仍然使用现有 endpoint 与 `protocol_version: 2`，但请求体和 session binding 改为 path-based target 语义：

- 请求列表字段使用 `targets`，不再使用 `roots`。
- session binding 字段使用 `target_authority_hash`，不再使用 `roots_authority_hash`。
- 每个 target 只需要 `studio_path`；不再传 `root_id`。
- 用户配置、文档和 CLI 错误信息都不再出现 `roots` 作为配置概念。

task-agent 必须和 bridge2 解析同一份 `code-sync.tree.json`，并从同一棵最终 apply target 列表写入 `.clock-p/session.json` 的 code-sync binding。helper2 的 binding gate 继续校验 mapping profile 和 target `studio_path`；它不解析 tree 配置。

## mcp2 规则

mcp2 不理解 node/subnode/ownership。它只执行：

- 查询给定 Studio path 的当前实际子树。
- 对给定 Studio path 执行目标树 reconcile。

reconcile 的核心语义：把某个 Studio path 下的实际子树，增量改到和 agent 给出的目标树完全一致；能复用就复用，只在必要时创建、删除或替换。

apply 必须从当前 `ClearAllChildren()` 递归重建改成增量 reconcile：

- 目标有、Studio 没有：创建。
- 目标有、Studio 有且类型正确：复用并递归更新。
- 目标有、Studio 有但类型错误：
  - 如果现有 child 是 managed class，替换这个 child。
  - 如果现有 child 不是 managed class，失败。
- Studio 有、目标没有：删除该 managed child。
- 脚本 Source 不同才写入。
- 同一 parent 下出现重复 child name：失败。
- 碰到不属于 managed class 的 child 且需要删除或覆盖时：失败。
- managed target root subtree 内出现任何 unmanaged descendant 时，apply 和 live manifest 都必须失败，不能被 hash 忽略。live manifest v2 不接收目标 tree，因此按整个 target root subtree 检查。

这仍然满足“目标子树最终与 agent 给出的目标树一致”，但不会无条件销毁未变化节点。

Reconcile 算法边界：

- reconcile 只在当前 target root subtree 内运行，不碰 target path 的祖先。
- ensure target path 时，service 后的中间路径若不存在则创建 `Folder`；若存在但不是 folder-like managed class，失败。
- 每一层按 direct child name 对齐；Studio 同一个 parent 下出现重复 child name，失败。
- 对每个目标 child：
  - 找不到则创建目标 kind。
  - 找到且 kind 一致则复用。
  - 找到且 kind 不一致，若现有 child 是 managed class 则销毁该 child 并按目标 kind 新建；若不是 managed class 则失败。
- 对每个 Studio 现有 child：
  - 若目标 children 没有该 name，且该 child 是 managed class，则删除。
  - 若目标 children 没有该 name，且该 child 不是 managed class，则失败。
- 脚本节点先按 kind/source 更新自身，再 reconcile children；Source 相同不写入。
- Folder 节点没有 Source；payload 如果给 Folder 带 source，mcp2 忽略或拒绝都可以，第一版建议拒绝，避免 agent 产物脏。

换句话说：同名同类复用，同名异类按 managed 安全替换，额外 managed 删除，额外 unmanaged 失败。

## 需要修改的代码

- `tools/bridge2/clockp_bridge2/code_sync/config.py`
  - 从 `{ "roots": [...] }` 改为 `{ "tree": {...} }`。
  - 输出 node 列表与 node/subnode 关系。
- `tools/bridge2/clockp_bridge2/code_sync/manifest.py`
  - 基于最终 apply targets 构建本地 manifest。
- `tools/bridge2/clockp_bridge2/code_sync/apply.py`
  - 下发最终 apply targets。
- `tools/bridge2/clockp_bridge2/code_sync/mapping.py`
  - 支持 node `$kind`、node-level `init.*` 规则和 subnode graft。
- `tools/bridge2/test_code_sync.py`
  - 删除 roots overlap 拒绝测试。
  - 增加 tree 配置、subnode graft、local_path 复用、Studio path 重复拒绝、`init.*` + `$kind` 规则测试。
  - 增加重复 JSON key 拒绝测试。
  - 增加 canonical Studio path / target hash / combined hash golden fixture。
- `go-helper/internal/taskagent/code_sync.go`
  - 从 tree 配置构建 code-sync binding。
  - 默认配置文件名改为 `code-sync.tree.json`。
  - session 里写最终 apply targets，不再写用户配置 roots。
  - 使用和 bridge2 一致的 canonical Studio path、配置 hash、target authority hash 规则。
- `go-helper/internal/taskagent/workspace_config.go`
  - 默认 `code_sync_config` 改为 `code-sync.tree.json`。
  - 缺配置文件的错误文案改为 node tree 语义。
- `go-helper/internal/tasksession/registry.go` 与 `go-helper/cmd/studio-helper/main.go`
  - 保持 v2 binding 校验能力；校验对象是最终 apply targets。
  - 用户可见错误文案避免把旧用户配置称为 roots。
- 相关 Go 测试
  - 更新默认文件名、binding hash、session fixture 和错误文案。
  - 增加 tree 配置、重复 JSON key 拒绝、canonical Studio path golden fixture。
  - 增加 Python/Go 共享 fixture，验证最终 apply targets、canonical Studio path、target hash、combined hash、`code_sync_config_hash`、`target_authority_hash` 一致。
  - 共享 fixture 放在 repo 内固定 JSON 文件，例如 `tools/bridge2/testdata/code_sync_tree_fixture.json`，Python 和 Go 测试必须读取同一份；fixture 至少覆盖 subnode graft、重复 local_path、显式 `$kind`、非 ASCII / 特殊符号 Studio segment。
- `plugin-mcp2/src/Main.server.luau`
  - 把 `codeSyncWriteNode` 改为增量 reconcile。
  - live manifest 查询遇到 target subtree 内 unmanaged child 时失败。
  - 增加 plugin 侧或等价 Luau 测试夹具，证明未变化 child 被复用而不是重建。
- `README.md` 和 `skills/roblox-agent/SKILL.md`
  - 删除 `code-sync.roots.json` 说法，改为 `code-sync.tree.json`。

## Phase

### Phase 1：配置解析与跨语言 target identity

- 实现 `code-sync.tree.json` 解析。
- 删除旧 roots 格式支持。
- Python/Go 都拒绝重复 JSON key。
- Python/Go 都能从 tree 配置生成同一组最终 apply targets。
- Python/Go 共享 fixture 验证 canonical Studio path、target hash、combined hash、配置 hash、target authority hash 一致；fixture 必须是 repo 内同一份 JSON 文件。
- 不要求 Studio live apply 通过。

### Phase 2：bridge2 manifest/apply 与 task-agent binding

- bridge2 本地 manifest 基于最终 apply targets。
- task-agent 从 tree 配置写出 session code-sync binding。
- helper2 binding gate 改为 path-based target 校验。
- apply/dry-run/live manifest 全部使用最终 apply targets 与 `targets` payload。
- helper2 接口和 protocol version 保持不变。
- 单元测试覆盖嵌套 subnode 与 local_path 复用。

### Phase 3：mcp2 增量 reconcile

- 替换 `ClearAllChildren()` 写法。
- live manifest 查询按 target root subtree 检查 unmanaged descendant。
- 测试覆盖 Source 相同不写入、未变化 child 复用、managed 类型错误局部替换、unmanaged extra child 失败、unmanaged 覆盖失败、duplicate child name 失败、root path 中间节点类型冲突失败。
- 保持最终 live hash 验证。
- 构建 plugin，跑 bridge2 Python 测试与 go-helper 测试。

### Phase 4：文档与 game1 配置落地验证

- README / skill 改成 node tree 主线。
- game1 配置改为 `code-sync.tree.json`。
- 用真实 workspace 做 code-sync apply 验证；Studio 测试由 Windows 侧部署 helper2/plugin-mcp2 后执行。
