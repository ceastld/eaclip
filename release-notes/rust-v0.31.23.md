# Rust ClipHost 0.31.23

发布日期：2026-10-20。本版针对开机自启时的时间计算溢出修复启动稳定性。

## 修复

- 修复部分电脑在开机早期自动启动时，可能提示 `overflow when subtracting duration from instant` 并退出的问题。
- 同时修正采集状态及全局采集抑制状态的初始化，保持默认立即允许采集，后续临时暂停与恢复规则沿用原有设置。
- 改进设置窗口打开失败时的诊断信息。

剪贴板 n10 与超级菜单 n10 同步要求宿主 `0.31.23.0`。入口 DLL 继续复用 `0.31.14.1`，x64 最低兼容要求为 `0.31.7.0`，ARM64 为 `0.31.14.1`。

## 下载

按 Windows 系统架构选择 ZIP，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。GPUI 原生界面无需 WebView2，沿用原有数据目录。

| Windows 架构 | 便携包 | SHA-256 校验文件 |
|---|---|---|
| x64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.23/eaclip-0.31.23-windows-x64.zip) | [x64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.23/eaclip-0.31.23-windows-x64.SHA256SUMS.txt) |
| ARM64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.23/eaclip-0.31.23-windows-arm64.zip) | [ARM64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.23/eaclip-0.31.23-windows-arm64.SHA256SUMS.txt) |

## 验证范围

新增回归覆盖计时起点附近的初始化及临时暂停到期边界。正式附件按两种架构检查回归、隔离启动、文件身份与载荷，并扫描原始分发 ZIP。

用户反馈电脑上的实际开机自启效果仍待后续反馈确认。问题反馈与功能建议请提交到 [统一反馈仓库](https://github.com/QuickerHub/cea-action-issues/issues)。
