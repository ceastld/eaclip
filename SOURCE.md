# 源码与构建

本仓库公开当前 Rust ClipHost / GPUI 宿主源码、共享资源、测试夹具和发布工作流。

## Windows x64

需要 Rust 1.90.0、Visual Studio C++ Build Tools / Windows SDK 和 PowerShell 7。

```powershell
cd host
cargo build --locked --release -p cliphost-gpui --bin cliphost-gpui --target x86_64-pc-windows-msvc
```

正式发布由本仓库的 `Rust Windows Release` GitHub Actions 工作流从对应版本标签构建、验证与扫描。
构建输出位于 `host/target/x86_64-pc-windows-msvc/release/`；发行便携包从 Releases 下载。

本次公开范围为桌面宿主。同步功能留待后续，正式构建不启用 `private-sync`。
示例只使用占位地址；实际服务入口应通过转发域名配置，凭据保存在部署环境或 Actions Secrets 中。

`docs/user/` 是用户文档维护源，发布后同步到 `docs/`；`release-notes/public/` 是版本说明维护源。
`frontend/src/docs/` 与 `ui-design/` 在此提供 GPUI 编译所需的共享内容和设计资源。

## 目录

`host/` 是 Rust 工作区，默认成员为 `crates/cliphost-gpui`。

```text
host/
  crates/cliphost-gpui/    # GPUI 主程序
  crates/                # 核心、HTTP、Bridge 与更新器
  shared/windows/        # 两宿主共用的原生代码与测试资源
  resources/             # 产品图标
  src-tauri/             # 显式选择的 Tauri 回退
```

在 `host/` 直接运行 `cargo build` 默认构建 GPUI；Tauri 回退须显式选择 `-p cliphost-app`。
