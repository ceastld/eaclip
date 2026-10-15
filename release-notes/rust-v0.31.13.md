# Rust ClipHost 0.31.13

发布日期：2026-10-15。本版提供 Windows x64 和 ARM64 便携包，使用 GPUI 原生界面，无需 WebView2。

## 功能改进与修复

- 改进“打开 / 另存为”窗口的文件夹跳转，标准窗口内切换目录时保留已填写的文件名，繁忙或失败时停止重复跳转。
- 完善资源管理器设置：增加历史记录上限、重开菜单显示数量、使用说明及文件菜单设置入口，记录开关立即生效。
- 增加收藏、面板、自定义目录和分类的拖拽整理能力，完善移动、复制、排序与撤销。
- 优化主窗口布局、分类编辑和图标显示；分类名称留空时跟随内容类型及界面语言显示。
- 改进各原生页面的平滑滚动、滚动条显隐与拖动，减少删除条目时的整页刷新。
- 修复部分长 HTML、纯数字内容及聊天富文本的预览与原样保留问题。
- 完善 Quicker 启动器对开发宿主、实际监听进程及退出状态的校验。
- 修复连续便捷粘贴的后续快捷键提交，以及修改内容后仍显示旧预览的问题。

## ARM64 与安装入口更新

- 新增 Windows ARM64 原生便携包，沿用本版业务功能，与 x64 分开下载。原有 x64 包保持原样。
- ARM64 宿主已同步到 Quicker 官方依赖库和 Bitiful；官方依赖、备用依赖以及便携镜像均已完整下载并核对摘要。
- 独立发布 Quicker 入口 DLL `0.31.14.1`（官方依赖版本 `0.31.14`）。新版安装器将架构判断和宿主安装收进 DLL：先从 Quicker 官方依赖下载入口 DLL，再由 DLL 选择并下载宿主，支持官方与 Bitiful 两个来源。
- 新安装器按 Windows 原生架构选择宿主；在 ARM64 Windows 上运行 x64 模拟版 Quicker 时，仍选择 ARM64 宿主。ARM64 需要 `0.31.14.1` 或更新启动器，x64 的最低兼容要求仍为 `0.31.7.0`。

**Quicker 使用范围：**新版入口 DLL 已在官方依赖库提供，公开分享动作尚未切换到新版安装入口。ARM64 用户当前可直接使用下方便携包；通过新版安装入口运行的完整 ARM Quicker 流程仍待实机验证。

## 下载与使用

请按 Windows 系统架构选择 ZIP，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。

| Windows 架构 | 便携包 | SHA-256 校验文件 |
|---|---|---|
| x64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.13/eaclip-0.31.13-windows-x64.zip) | [SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.13/SHA256SUMS.txt) |
| ARM64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.13/eaclip-0.31.13-windows-arm64.zip) | [ARM64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.13/eaclip-0.31.13-windows-arm64.SHA256SUMS.txt) |

ARM64 便携包另有 [Bitiful 备用下载](https://quicker-pkgs.s3.bitiful.net/eaclip/rust-v0.31.13/eaclip-0.31.13-windows-arm64.zip)，与 GitHub 包的内容和摘要相同。

## 验证范围

ARM64 已通过原生编译、隔离环境中的基础健康启动、随包 VC 运行库加载和正常退出检查。完整桌面交互、输入法、OLE 与第三方应用回贴仍待实机验证；文件对话框的部分原生增强目前仅提供 x64 实现。

问题反馈与功能建议请提交到 [统一反馈仓库](https://github.com/QuickerHub/cea-action-issues/issues)。
