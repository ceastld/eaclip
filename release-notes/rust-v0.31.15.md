# Rust ClipHost 0.31.15

发布日期：2026-10-16。本版加强运行与分发安全，提供 Windows x64 和 ARM64 便携包，继续使用 GPUI 原生界面，无需 WebView2。

## 安全与兼容改进

- 文件对话框目录跳转不再向第三方进程加载辅助 DLL，改用已有的外部控件导航方式；保留原文件名、目标窗口校验和实际目录确认。
- 限制本地 API 的浏览器访问来源，阻止外部网页读取剪贴板接口或发送操作请求；保留 Quicker 原生入口与内嵌页面兼容。
- 补齐程序的标准 Windows 产品名称和文件版本信息，便于核对下载文件身份。
- 发布流程增加文件身份检查及原始分发包的 Defender 扫描，扫描未完成或失败时停止分发。

本版针对安全软件告警反馈减少不必要的跨进程执行行为并加强访问边界。尚未取得卡巴斯基的复测或厂商确认，不能保证所有安全软件的告警已消除；程序仍未配置可信发布者代码签名。

## 下载与兼容

按 Windows 系统架构选择 ZIP，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。

| Windows 架构 | 便携包 | SHA-256 校验文件 |
|---|---|---|
| x64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.15/eaclip-0.31.15-windows-x64.zip) | [x64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.15/eaclip-0.31.15-windows-x64.SHA256SUMS.txt) |
| ARM64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.15/eaclip-0.31.15-windows-arm64.zip) | [ARM64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.15/eaclip-0.31.15-windows-arm64.SHA256SUMS.txt) |

本次复用已发布的 `0.31.14.1` Quicker 入口 DLL；x64 最低兼容要求保持 `0.31.7.0`，ARM64 保持 `0.31.14.1`。宿主升级不改变用户的数据位置。

## 验证范围

文件对话框的定向回归及两个自建另存为窗口已验证目录跳转和原文件名保留；本地接口回归与 GPUI 构建通过。正式产物按两种架构分别执行原生回归、隔离启动、载荷与运行库校验，并对原 ZIP 扫描后再分发。完整中文输入法、物理拖放、第三方应用回贴和 ARM64 桌面操作仍按实际验收结果记录，不以隔离启动检查代替。

问题反馈与功能建议请提交到 [统一反馈仓库](https://github.com/QuickerHub/cea-action-issues/issues)。
