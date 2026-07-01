# studio-rust-mcp-server

本仓维护 Roblox Studio 侧联调服务源码：

- `go-helper/cmd/studio-helper`：Windows helper2 进程，负责和 Studio 插件、MCP、公网 clockbridge 通信。
- `go-helper/cmd/task-agent`：workspace 侧 task-agent，负责写入 `.clock-p/session.json` 并向 helper2 心跳。
- `plugin-mcp2`：Roblox Studio 插件，执行 helper2 下发的 Studio 命令。
- `go-helper/internal/*`：task session、Studio 控制、截图、official adapter、task-agent 配置等实现。

bridge CLI 不在本仓维护。当前 CLI、PlayMode data、code-sync、launch/prelaunch、run-code、screenshot 等操作手册在：

```text
K:\roblox_space\roblox-dev-infra\README.md
K:\roblox_space\roblox-dev-infra\skills\roblox-agent\SKILL.md
K:\roblox_space\roblox-dev-infra\tools\bridge\
```

不要在本仓恢复历史 CLI、旧 bridge wrapper、helper1、hub、task-server、mcp1 或 runtime-log 入口。

## 构建 helper2 / task-agent

helper2 必须使用本仓当前源码 build 出来的本地二进制，不要使用公网矩阵测试临时目录里的 `studio-helper.exe`。

```powershell
cd K:\roblox_space\studio-rust-mcp-server\go-helper
New-Item -ItemType Directory -Force bin | Out-Null
go build -o bin\studio-helper.exe ./cmd/studio-helper
go build -o bin\task-agent.exe ./cmd/task-agent
```

启动 helper2：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\studio-helper.exe --addr 127.0.0.1:44750
```

启动 task-agent：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe start `
  --workspace <workspace> `
  --machine_name <machine_name>
```

查看和停止 task-agent：

```powershell
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe status --workspace <workspace>
K:\roblox_space\studio-rust-mcp-server\go-helper\bin\task-agent.exe stop --workspace <workspace>
```

## 身份与会话边界

Windows 客户端身份文件统一放在：

```text
%APPDATA%\dev.clock-p.com\
```

当前使用：

- `machine_name`：helper2 读取，用于注册公网 helper 域名。
- `feishu-user_name`：helper2 读取，用于拼接公网用户名段。
- `feishu-token`：helper2 读取，用于 clockbridge 注册认证。

`task-agent --machine_name` 必须显式传入，语义是目标 Windows helper 的 `machine_name`，不是 task-agent 当前机器名。task-agent 启动前读取 workspace 根的 `clock-p.workspace.json`，启动后写入 `.clock-p/session.json`。

后续 bridge 命令只读 `.clock-p/session.json`。不要重新拼 machine、user、token 或 task 身份。

## plugin-mcp2

Studio 侧插件必须使用本仓当前源码构建。更新 `plugin-mcp2` 后，先停止当前 Studio / task-agent，再按 Windows 侧当前插件打包流程生成并安装 `MCP2Plugin.rbxm`。

server 侧不把插件打包工具作为 code-sync / flush 运行时依赖；Linux 本机通常只做源码、Go 与静态检查，最终插件包由 Windows 侧部署验证。

## 验证

Go 侧：

```powershell
cd K:\roblox_space\studio-rust-mcp-server\go-helper
go test -count=1 ./...
```

bridge Python 侧在 `roblox-dev-infra` 验证：

```powershell
cd K:\roblox_space\roblox-dev-infra
py -3 -m py_compile tools\bridge\cli.py
py -3 -m unittest discover tools\bridge -p "test_*.py"
```

涉及公网 task session gate 时再跑：

```powershell
cd K:\roblox_space\studio-rust-mcp-server
py -3 util\helper2_task_session_gate_test.py --place-id <place_id>
```

## 文档边界

本仓 README 只描述 helper2、task-agent、plugin-mcp2 的源码与构建边界。Roblox 联调操作流程、bridge CLI 参数和测试输入策略只在 `roblox-dev-infra` 维护。
