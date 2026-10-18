# Rust ClipHost 0.31.16

发布日期：2026-10-18。本版更新 GPUI 原生界面、收藏与历史操作，改善图片列表缓存，并继续沿用原有数据位置和存储格式。

## 使用与界面

- 历史条目使用更紧凑的右键菜单，整合常用操作、收藏和局域网发送；补齐菜单与子窗口的键盘、焦点及关闭处理。
- 收藏支持直接保存到总目录、悬停选择指定目录和路径搜索；保存结果通过紧凑气泡显示位置，并保留撤销操作。
- 列表常用设置改为右下角紧凑弹层，可调整预设外观和交互；历史列表恢复返回顶部按钮，并提供默认关闭的闲置回顶选项。
- 图片列表按可见内容加载显示资源，并在多个列表间共享缓存预算；滚动、隐藏窗口和关闭列表时回收缓存。显示缩图不改写原图、历史内容或收藏数据，复制与编辑仍使用原始内容。
- 完善原生文件选择、文件预览与局域网窗口的配合，补充启动失败提示和可导出的诊断信息。

## 数据与兼容

本版继续读取原有历史、收藏、配置及图片目录，**不自动转换为新的存储格式，也不移动用户数据**。独立迁移工具增加候选库完整读取校验、重复字段和使用统计范围检查；新存储仍处于隔离验证阶段，生产切换和整套恢复验证完成前保持关闭。

历史和收藏继续保留独立内容快照；图片显示缓存与原始文件分离，不因淘汰显示缓存而删除原图。

## 下载

按 Windows 系统架构选择 ZIP，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。GPUI 原生界面无需 WebView2。

| Windows 架构 | 便携包 | SHA-256 校验文件 |
|---|---|---|
| x64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.16/eaclip-0.31.16-windows-x64.zip) | [x64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.16/eaclip-0.31.16-windows-x64.SHA256SUMS.txt) |
| ARM64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.16/eaclip-0.31.16-windows-arm64.zip) | [ARM64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.16/eaclip-0.31.16-windows-arm64.SHA256SUMS.txt) |

Quicker 入口继续复用已发布的 `0.31.14.1` DLL；x64 最低兼容要求保持 `0.31.7.0`，ARM64 保持 `0.31.14.1`。剪贴板 n10 与超级菜单 n10 同步更新最低宿主版本。

## 验证范围

正式附件按两种架构分别进行回归、隔离启动、真实文件身份和载荷校验，并对原始分发 ZIP 扫描后发布。隔离检查不代替完整中文输入法、物理拖放、第三方应用回贴或 ARM64 桌面操作验收。程序尚未配置可信发布者代码签名，也未取得卡巴斯基的复测确认。

问题反馈与功能建议请提交到 [统一反馈仓库](https://github.com/QuickerHub/cea-action-issues/issues)。
