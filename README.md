# Pi QQ Herdr Client

在手机 QQ 里使用电脑上的 [Pi](https://pi.dev/)：发送任务、接收回复，在多个项目和 Sessions 之间切换。

## 能做什么

- **随时继续任务**：通过 QQ 发送任务和补充要求，接收 Pi 的回复与任务完成通知。
- **图片、视频、文件双向传输**：既能从手机发送附件给 Pi，也能接收 Pi 从电脑发回的附件。
- **管理多个项目**：选择 Workspace、切换 Session，也可以直接在手机上新建 Pi。
- **调整任务与模型**：补充要求、停止任务、查看当前输出，或切换模型和 Thinking 等级。
- **在手机上回应**：使用兼容插件时，可在 QQ 中回答问卷、确认权限请求。

## 开始使用

电脑需要安装 Pi 和 [Herdr Core](https://herdr.dev/)，Herdr App 窗口无需保持打开。

1. 从本仓库的 [Releases](https://github.com/AHGGG/pi-qq-herdr-client/releases) 下载对应平台的版本：Windows x64 直接运行 `.exe`；macOS 打开 `.dmg`，按 Apple Silicon／Intel 选择；Linux x64 使用 `.deb`（Ubuntu 24.04+）。
2. 用手机 QQ 扫码，按页面提示选择机器人并完成授权；没有机器人时，可从客户端打开创建入口。
3. 在机器人私聊中发送 `/sessions`，选择已有 Session；也可以发送 `/new` 创建一个 Pi。随后直接发送任务即可。

首次接入已经运行的 Pi 时，请在它空闲时执行一次 `/reload`。Linux 需要图形桌面和已解锁的系统密钥环。

## QQ 内的交互

- **左滑引用回复**：向左滑动 Pi 的一条回复或通知，引用后直接回复。内容会发回原消息对应的 Session，切换过 Session 也能继续原来的对话。
- **点击按钮操作**：通过列表和消息卡片上的按钮选择 Session、打开“模型与命令”或“查看当前输出”。
- **随时补充要求**：选择 Session 后直接发送文字或附件；Pi 工作时，新的补充要求会作为当前任务的反馈。

## 常用操作

| 想做什么 | 发送什么 |
| --- | --- |
| 选择 Workspace | `/workspaces` |
| 切换 Session | `/sessions` |
| 新建 Pi | `/new` |
| 调整模型、Thinking 或使用命令 | `/commands` |
| 查看状态或当前输出 | `/status`、`/peek` |
| 停止任务或关闭 Pi | `/stop`、`/close` |
| 查看完整帮助 | `/help` |

一个 Session 就是一个独立运行的 Pi。

新建 Pi 会使用选中的 Workspace；没有 Workspace 时，会在用户主目录启动第一个 Pi。

电脑需要保持开机、联网，客户端也需要保持运行。关闭窗口后会驻留托盘，右键托盘可暂停连接或退出。

[开发文档](docs/development.md)
