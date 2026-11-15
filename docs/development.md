# 开发与发布

开发前阅读 [AGENTS.md](../AGENTS.md)。客户端采用 Rust 和 GPUI Kit，QQ 连接与业务在 Rust 内运行；配套 TypeScript 扩展使用 Pi 已有的运行环境。

## 本地构建

需要 Windows x64、Rust 1.92+、MSVC C++ 构建工具、Windows SDK，以及 GPUI 所需的本机构建工具。

```powershell
cargo check --locked --workspace
./scripts/build.ps1
```

构建脚本生成 `target/release/pi-qq-herdr-client.exe`、`dist/pi-qq-herdr-client-windows-x64/`、ZIP 和 SHA256 文件。客户端不附带 Node 或浏览器运行时，Pi 与 Core 使用本机安装。更新正在运行的 dist EXE 前，需要先退出客户端；已有 Pi、Core 和 Herdr App 无需退出。

本项目不编写或运行单元测试。交付检查使用 Rust 编译、扩展语法与格式检查；扫码、手机命令和桌面体验由使用者完成整体运行验证。

## GitHub 发布

[发布 workflow](../.github/workflows/release.yml) 只由 `workflow_dispatch` 手动触发。提交代码、推送 tag 和版本提交均不会自动运行它。

1. 将 workflow 和源码推送到 GitHub；workflow 需要先出现在默认分支，Actions 页面才会显示手动运行入口。
2. 打开 **Actions → Prepare Windows release → Run workflow**，选择源码分支及版本升级方式，默认 `patch`。例如当前 `0.1.0`，`patch` 升到 `0.1.1`，`minor` 升到 `0.2.0`，`major` 升到 `1.0.0`。可勾选 prerelease，将草稿标记为预发布。
3. workflow 自动同步 `Cargo.toml`、`Cargo.lock` 中两个本地包的版本及 EXE 的 Windows 版本资源，然后构建 Windows x64 ZIP 和 SHA256 文件。构建成功后才将版本提交和 tag 一起推送回所选分支，不更新依赖版本、不覆盖已有 tag。
4. workflow 创建 **Draft Release（草稿）**，上传 ZIP、SHA256 并生成发布说明。Actions 摘要提供草稿链接，构建产物也保留在该次运行的 Artifacts 中。版本提交和 tag 在准备阶段已写入仓库，正式发布仍需确认。
5. 下载检查产物、编辑说明，确认后在 GitHub 点击 **Publish release**。workflow 在草稿处结束。

使用仓库自带的 `GITHUB_TOKEN`，无需单独配置发布密钥；所选分支必须允许 Actions 写入版本提交和 tag。若构建期间分支已有新提交，推送会失败，不会强行覆盖。当前仅生成 Windows x64 发布包，macOS/Linux 尚缺可用的凭据保护实现。此流程不会替换或重启本机客户端。

## 数据与代码

普通配置与运行状态默认放在 `%LOCALAPPDATA%/pi-qq-herdr-client`，QQ 授权保存在 `credentials.protected`，由 Windows DPAPI 保护。替换 EXE 或重新打包不影响绑定，在同一个 Windows 用户下启动即可复用。

`PI_QQ_CLIENT_DIR` 可指定独立数据目录，调试时客户端和 Pi 必须保持一致。`PI_CODING_AGENT_DIR` 决定配套扩展的安装位置。

- `crates/client-core/`：QQ、Core、本机 Pi 桥接、路由与持久化。
- `crates/desktop/`：原生窗口、二维码、托盘和桌面生命周期。
- `integrations/pi/`：随 EXE 分发的 Pi 扩展，无额外 npm 依赖。
- `scripts/generate-icon.py`：生成 EXE、窗口和托盘使用的 SVG、PNG 和 ICO 资源，可通过 `uv run --no-project --with pillow python scripts/generate-icon.py` 运行。
- `scripts/bump-version.py`：升级版本号，同步 Cargo 与 Windows 资源，供发布 workflow 使用。

本机的 `docs/design.md`、`docs/implementation-handoff.md` 和 `docs/research/` 是未跟踪的本地资料，不随克隆分发。
