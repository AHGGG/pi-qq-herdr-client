# Pi QQ Herdr Client

通过手机 QQ 使用电脑上的 Pi，会话由 Herdr Core 管理。

客户端采用 Rust 和 GPUI Kit 原生界面，配套的 TypeScript 扩展运行在 Pi 内部。

## 当前状态

仓库已完成基础初始化，客户端功能、Cargo 工作区和安装包将在后续实现。

## 目录

- `crates/client-core/`：QQ、Herdr Core、Pi 连接与会话管理。
- `crates/desktop/`：原生窗口与桌面生命周期。
- `integrations/pi/`：自动安装到 Pi 的配套扩展。

## 开发入口

先阅读 [AGENTS.md](AGENTS.md)。本机若存在 `docs/design.md` 和
`docs/implementation-handoff.md`，再阅读这两份本地设计与交接文档。
这些文件以及 `docs/research/` 已按项目要求加入 Git 忽略，不随克隆分发。
