# 开发与发布

开发前阅读 [AGENTS.md](../AGENTS.md)。客户端采用 Rust 和 GPUI Kit，QQ 连接与业务在 Rust 内运行；配套 TypeScript 扩展使用 Pi 已有的运行环境。

## 本地构建

需要 Rust 1.92+、Python 3.11+ 和对应平台的本机构建工具。Windows 使用 MSVC C++ 构建工具与 Windows SDK；macOS 使用 Xcode（包括 Metal 编译工具）；Linux 依赖 GTK 3、Ayatana AppIndicator、X11/Wayland 等开发包，安装命令见发布 workflow。

```powershell
cargo check --locked --workspace
./scripts/build.ps1
```

Windows 构建脚本生成 `target/release/pi-qq-herdr-client.exe` 和可直接下载运行的 `dist/pi-qq-herdr-client-windows-x64.exe`，附带 `.exe.sha256` 校验文件。用户无需解压 ZIP。

在 macOS/Linux 本机通过 `bash scripts/build-unix.sh <target>` 构建：macOS target 为 `aarch64-apple-darwin` 或 `x86_64-apple-darwin`，产物为包含 `.app` 的 DMG；Linux target 为 `x86_64-unknown-linux-gnu`，产物为 DEB。各包均有 SHA256，版本与 Cargo 一致。Linux DEB 的运行库依赖由 `dpkg-shlibdeps` 从实际二进制生成，另要求 Ayatana AppIndicator 和 GNOME Keyring。Linux 基线为 Ubuntu 24.04+，需要图形桌面和已解锁的 Secret Service，不是无界面的服务程序。

macOS 应用使用 ad-hoc 签名，尚未配置 Developer ID 和公证。首次运行时可能需要在系统设置的“隐私与安全性”中允许打开。客户端不附带 Node 或浏览器运行时，Pi 与 Core 使用本机安装。替换正在运行的客户端前应先退出它；已有 Pi、Core 和 Herdr App 无需退出。

本项目不编写或运行单元测试。交付检查使用 Rust 编译、扩展语法与格式检查；扫码、手机命令和桌面体验由使用者完成整体运行验证。

## GitHub 发布

[发布 workflow](../.github/workflows/release.yml) 只由 `workflow_dispatch` 手动触发。提交代码、推送 tag 和版本提交均不会自动运行它。

1. 将 workflow 和源码推送到 GitHub；workflow 需要先出现在默认分支，Actions 页面才会显示手动运行入口。
2. 打开 **Actions → Prepare release → Run workflow**，选择源码分支及版本升级方式，默认 `patch`。例如当前 `0.1.0`，`patch` 升到 `0.1.1`，`minor` 升到 `0.2.0`，`major` 升到 `1.0.0`。可勾选 prerelease，将草稿标记为预发布。
3. workflow 准备一次版本文件，同步 `Cargo.toml`、`Cargo.lock` 中两个本地包的版本及 EXE 的 Windows 版本资源。四个原生 runner 并行构建 Windows x64 EXE、macOS ARM64 DMG、macOS Intel DMG 和 Linux x64 DEB；macOS 包从 Cargo 读取相同版本。全部成功且校验文件齐全后，才将版本提交和 tag 一起推送回所选分支，不更新依赖版本、不覆盖已有 tag。
4. workflow 创建 **Draft Release（草稿）**，上传四个平台的安装包、各自的 SHA256 并生成发布说明。Actions 摘要提供草稿链接，每个平台成功构建的产物也保留在该次运行的 Artifacts 中。版本提交和 tag 在准备阶段已写入仓库，正式发布仍需确认。
5. 下载检查产物、编辑说明，确认后在 GitHub 点击 **Publish release**。workflow 在草稿处结束。

使用仓库自带的 `GITHUB_TOKEN`，无需单独配置发布密钥；所选分支必须允许 Actions 写入版本提交和 tag。若构建期间分支已有新提交，推送会失败，不会强行覆盖。此流程不会替换或重启本机客户端。macOS/Linux 的源码适配和发布配置已加入；原生编译、首次扫码与桌面行为须在对应环境验证，不能用 Windows 编译通过代替。

## 数据与代码

普通配置与运行状态保存在系统用户数据目录的 `pi-qq-herdr-client` 下。Windows 使用 `%LOCALAPPDATA%/pi-qq-herdr-client`，QQ 授权由 DPAPI 保护并保存在 `credentials.protected`；macOS 使用 Keychain，Linux 使用 Secret Service 保存授权，不回退为明文文件。非 Windows 的凭据条目按数据目录区分，隔离开发环境不会覆盖正常绑定。替换同一用户下的应用不清理绑定。

`PI_QQ_CLIENT_DIR` 可指定独立数据目录，调试时客户端和 Pi 必须保持一致。`PI_CODING_AGENT_DIR` 决定配套扩展的安装位置。

- `crates/client-core/`：QQ、Core、本机 Pi 桥接、路由与持久化。
- `crates/desktop/`：原生窗口、二维码、托盘和桌面生命周期。
- `integrations/pi/`：随 EXE 分发的 Pi 扩展，无额外 npm 依赖。
- `scripts/generate-icon.py`：生成 EXE、窗口和托盘使用的 SVG、PNG 和 ICO 资源，可通过 `uv run --no-project --with pillow python scripts/generate-icon.py` 运行。
- `scripts/bump-version.py`：升级版本号，同步 Cargo 与 Windows 资源，供发布 workflow 使用。
- `scripts/build-unix.sh`：生成 macOS 应用、DMG 和 Linux DEB。

本机的 `docs/design.md`、`docs/implementation-handoff.md` 和 `docs/research/` 是未跟踪的本地资料，不随克隆分发。
