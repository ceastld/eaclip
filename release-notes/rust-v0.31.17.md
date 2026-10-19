# Rust ClipHost 0.31.17

发布日期：2026-10-19。本版改善局域网传输与列表操作，补充异常退出诊断，并修正显卡恢复时的一处资源生命周期风险。

## 稳定性与诊断

- 修正显卡设备重建时的资源释放顺序，降低显示器恢复、显卡驱动重置等场景下的崩溃风险。
- 增加主窗口关闭、主动退出及正常结束的诊断记录，帮助区分主动退出和异常终止；这些记录不包含剪贴板正文。
- 本机反馈的“进程自行退出”目前缺少足够的退出证据，根因尚未确定。本版修复的显卡恢复风险不能据此认定为该次退出的原因。

## 使用与界面

- 完善原生局域网设备与传输界面，补充设备、任务和传输状态的显示与操作。
- 历史列表删除后局部更新列表与选中位置，减少连续删除和跨页操作中的跳动。
- 调整暗色图标、文本编辑及原生窗口交互细节。

## 数据与兼容

继续读取原有历史、收藏、配置及图片目录，**不自动转换为新的存储格式，也不移动用户数据**。图片显示缓存与原始文件保持分离。

Quicker 入口继续使用已发布的 `0.31.14.1` DLL；x64 最低兼容要求保持 `0.31.7.0`，ARM64 保持 `0.31.14.1`。剪贴板 n10 与超级菜单 n10 同步更新最低宿主版本。

## 下载

按 Windows 系统架构选择 ZIP，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。GPUI 原生界面无需 WebView2。

| Windows 架构 | 便携包 | SHA-256 校验文件 |
|---|---|---|
| x64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.17/eaclip-0.31.17-windows-x64.zip) | [x64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.17/eaclip-0.31.17-windows-x64.SHA256SUMS.txt) |
| ARM64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.17/eaclip-0.31.17-windows-arm64.zip) | [ARM64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.17/eaclip-0.31.17-windows-arm64.SHA256SUMS.txt) |

## 验证范围

正式附件按两种架构分别完成回归、隔离启动、文件身份和载荷校验，并对原始分发 ZIP 扫描后发布。隔离检查不代替中文输入法、物理拖放、第三方应用回贴、真实显卡重置或 ARM64 桌面操作验收。程序尚未配置可信发布者代码签名，也未取得卡巴斯基复测确认。

问题反馈与功能建议请提交到 [统一反馈仓库](https://github.com/QuickerHub/cea-action-issues/issues)。
