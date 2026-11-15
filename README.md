# Pi QQ Herdr Client

通过手机 QQ 使用电脑上的 Pi，会话由 Herdr Core 管理。Windows 客户端采用 Rust 和 GPUI Kit，QQ 连接与业务在 Rust 内运行；配套 TypeScript 扩展使用 Pi 已有的运行环境。

## 运行

需要 Windows x64、已安装的 [Pi](https://pi.dev/) 和 [Herdr Core](https://herdr.dev/)。Herdr App 窗口可以关闭，客户端通过 `hc connect` 连接或启动 Core。

1. 打开 `pi-qq-herdr-client.exe`。缺依赖时按窗口提示安装并重新检测。
2. 用手机 QQ 扫码，选择自己的机器人并确认授权。没有机器人时点击官方创建入口。
3. 若授权结果没有返回控制者身份，窗口会显示 `/pair CODE`，在机器人私聊中发送后完成配对。
4. 连接成功后，客户端会主动向机器人私聊发送一次入门指南：已有 Pi 用 `/sessions` 选择，没有在线 Pi 时直接 `/new`；发送 `/help` 查看全部用法。新 Pi 默认在用户主目录启动，可在设置中更改。

桌面连接成功页也会显示具体步骤和已连接的 Pi 数量，指南按钮会显示发送进度、重发或失败重试入口。自动重连和重开客户端不会重复发送。QQ 若拒绝主动消息，仍可在机器人私聊发送 `/help` 开始。

客户端会安装自己的 Pi 扩展。新 Pi 自动加载；已经打开的 Pi 请在空闲时 `/reload` 或下次启动后加载。遇到同名用户文件会停止安装并提示冲突。

关闭窗口后继续运行，可从托盘打开、进入设置或退出。明确退出只关闭本客户端，已有 Pi 和共享 Core 继续运行。登录后启动默认关闭，在设置中自行开启。

设置还可以配置手机菜单、重新绑定 QQ、重试发送失败的通知。菜单配置会先备份原菜单并保留自定义入口。

客户端不导入旧桥接凭据。若用同一机器人替换旧网关，请先由你停止旧连接，避免两个网关同时接收；客户端不会自动停止旧网关。

## 手机命令

| 操作 | 命令 |
| --- | --- |
| 工作区与会话 | `/workspaces`、`/workspace wN`、`/sessions [all] [页码]`、`/use sN` |
| 新建与命名 | `/new [sN]`、`/name [sN] 名称` |
| 任务与终端 | `/status`、`/peek [sN]`、`/stop`、`/close [sN]` |
| 指定来源与反馈 | `/to sN 内容`、`/steer 内容`、引用通知后回复 |
| Pi 控制 | `/commands`、`/model`、`/thinking`、`/compact`、`/reload`、`/pi 命令 参数` |
| 问卷与权限 | `/answer qID 1`、`/cancel qID`、`/permissions`、`/approve pID`、`/deny pID [原因]` |

`/commands` 展示 Model、Thinking、Compact、Reload、All cmds、Status、Approvals、Peek 八个入口。`/commands 1` 和 `/model [页码]` 每页最多显示 12 项；命令点选后预填输入框，可追加参数，模型与 Thinking 可直接选择，当前项有标记。选项、翻页及状态入口都绑定到来源 Pi。`/help commands` 提供详细用法。修改模型/Thinking、`/compact [补充要求]`、`/reload`、`/pi 命令 参数` 需要 Pi 空闲，重载会等待同一 Pi 重新连接后再确认完成。

QQ 普通消息、`/to sN 内容`、`/steer 内容` 和带附件的消息都沿用原桥接的 Steer 行为，忙碌时作为当前任务反馈；Pi 空闲时正常开始下一轮。切换工作区会同步当前会话，选择一个 Pi 也会同步所属工作区。

问卷支持 `/answer qID 2`、`/answer sN 2`、`/answer 2` 和 `answer 2`；没有选中 Pi 时，只在当前工作区（或未选工作区时全部在线 Pi）有唯一待回答问卷时接受简写。`/permissions` 可重新显示已有的待处理请求。附件与控制命令需要分开发送，避免忽略附件。

`/new sN` 继承来源 Pi 的模型和 Thinking；新进程仍通过客户端已检测的本机 Pi 安装和配置启动。新建结果会持久等待扩展注册，首次等待超时后连接成功仍会补发通知；沿用原桥接的五分钟等待期限，不能据超时重复创建。

图片支持 JPG/PNG/GIF/WebP/BMP，单张最多 30 MiB；视频支持 MP4/MOV/WebM/MKV/AVI，视频和文件最多 100 MiB。附件保存后作为本地路径提交给 Pi，不自动解析、执行或转码。

自动转发每条已完成的助手文字，并在整轮 `agent_settled` 后发送完成通知。Pi 也可通过 `qq_send` 显式发送附件。入队回执和 QQ 接受回执不代表手机已经收到通知。

问卷兼容 `rpiv` 的事件协议；手机权限回答兼容已安装的 permission-system external-answer 适配。没有这些插件时基础聊天与会话功能仍可用；任意插件自绘弹窗仍需在 Pi 终端处理。

## 构建

需要 Rust 1.92+、MSVC C++ 构建工具、Windows SDK，以及 GPUI 所需的本机构建工具。

```powershell
cargo check --workspace
cargo build --release -p pi-qq-herdr-client -j 4
```

产物为 `target/release/pi-qq-herdr-client.exe`。运行 `./scripts/build.ps1` 可同时生成 `dist/pi-qq-herdr-client-windows-x64.zip` 和 SHA256 文件。客户端不附带 Node 或浏览器运行时，Pi 与 Core 使用本机安装。

本项目不编写或运行单元测试。交付检查使用 Rust 编译、扩展语法与格式检查；扫码、手机命令和桌面体验的整体运行验证由使用者完成。

## 数据与开发

普通配置与运行状态默认放在 `%LOCALAPPDATA%/pi-qq-herdr-client`，QQ 授权保存在 `credentials.protected`，由 Windows DPAPI 保护。替换 EXE 或重新打包不影响绑定，在同一个 Windows 用户下启动即可复用。`PI_QQ_CLIENT_DIR` 可指定独立数据目录，`PI_CODING_AGENT_DIR` 决定配套扩展安装位置。调试时让客户端和 Pi 使用一致的环境变量。

- `crates/client-core/`：QQ、Core、本机 Pi 桥接、路由与持久化。
- `crates/desktop/`：原生窗口、二维码、托盘和桌面生命周期。`assets/` 内的原创黑白像素聊天企鹅用于 EXE、窗口和托盘；可用 `uv run --no-project --with pillow python scripts/generate-icon.py` 重新生成。
- `integrations/pi/`：随 EXE 分发并安装到 Pi 的配套扩展，无额外 npm 依赖。

开发先读 [AGENTS.md](AGENTS.md)。本机的 `docs/design.md`、`docs/implementation-handoff.md` 和 `docs/research/` 刻意加入 Git 忽略，不随克隆分发。
