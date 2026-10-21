# eaclip（Cea 剪贴板）使用文档

eaclip 是 Windows 剪贴板管理工具（程序名 ClipHost，Rust + GPUI 原生界面）。本目录是它的使用说明，适用于 **0.31.26**。

| 文档 | 内容 |
| --- | --- |
| [安装与配置](install.md) | Quicker「剪贴板 n10」安装、便携包（x64 / ARM64）、与安装版共存、数据位置、开机自启、卸载 |
| [使用说明](usage.md) | 打开窗口、托盘、粘贴、便捷粘贴、收藏、聚合搜索、超级菜单、局域网配对 / 互传与设备同步 |
| [功能清单](features.md) | 按设置页列出当前版本的功能，以及尚未开放的功能 |
| [快捷键](hotkeys.md) | 全局快捷键默认值、窗口内按键、CapsLock 引导键、鼠标操作 |
| [设置说明](settings.md) | 设置页各分类的选项、默认值和取值范围 |
| [常见问题](faq.md) | 打不开、闪退诊断、安全软件告警、「剪贴板」与「剪贴板 n10」区别等 |
| [已知问题](known-issues.md) | 当前版本已知限制、尚未确认根因的问题、旧版（0.30.x）问题 |
| [更新记录](CHANGELOG.md) | eaclip 公开发布的版本（0.31.12 起） |

## 名称对照

| 名称 | 说明 |
| --- | --- |
| eaclip | 剪贴板产品的公开发布名。下载包名为 `eaclip-<版本>-windows-<x64\|arm64>.zip` |
| ClipHost / `cliphost-app.exe` | 程序本体（Rust + GPUI 原生界面，不需要 WebView2，也不需要 .NET） |
| 剪贴板 n10 | Quicker 里用来安装、启动这个程序的动作 |
| 超级菜单 n10 | Quicker 里打开超级菜单的配套动作，使用同一个宿主程序 |
| 旧版 ClipHost（0.30.x） | 以前的 .NET + WebView 版本，已被 0.31.x 取代，不再更新 |

## 下载与反馈

- 下载：[eaclip Releases](https://github.com/ceastld/eaclip/releases/latest)
- 问题反馈与功能建议：[QuickerHub/cea-action-issues](https://github.com/QuickerHub/cea-action-issues/issues)
