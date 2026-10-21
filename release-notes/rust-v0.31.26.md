# Rust ClipHost 0.31.26

发布日期：2026-10-21。

## 更新

- 简化设备同步设置：在设置中直接提供同步总开关，文本和图片可分别选择。新配置默认开启，已配对电脑可直接双向同步；原先明确暂停的状态继续保留。
- 移除重复的设备方向及接收处理选项。开启后，收到的内容同时进入剪贴板历史和当前剪贴板，局域网设备列表共用同一套设置。
- 修复同一图片经其他同步工具重新编码后出现重复条目的问题。标准 Windows PNG 附加信息不会再让像素相同的图片被当成新内容，也不会触发同步回传。
- 图片像素或透明度确实发生变化时仍保留为新内容；复制其他内容后再复制原图片，也能正常同步。

剪贴板 n10 与超级菜单 n10 同步使用宿主 `0.31.26.0`，继续使用已发布的 Quicker V1/V2 入口 `0.31.24.1`。无需安装 Quicker 也可使用剪贴板历史与收藏。

## 下载

按 Windows 系统架构选择 ZIP，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。GPUI 原生界面无需 WebView2，沿用原有数据目录。

| Windows 架构 | 便携包 | SHA-256 校验文件 |
|---|---|---|
| x64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.26/eaclip-0.31.26-windows-x64.zip) | [x64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.26/eaclip-0.31.26-windows-x64.SHA256SUMS.txt) |
| ARM64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.26/eaclip-0.31.26-windows-arm64.zip) | [ARM64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.26/eaclip-0.31.26-windows-arm64.SHA256SUMS.txt) |

## 验证范围

Windows 双机日常测试已验证文本、图片同步与类型开关，并模拟第三方重新发布同图：两端各保留一个匹配条目，发送序号不增长。正式发布包另按两种架构检查回归、隔离启动、文件身份和原始 ZIP 扫描。

上述模拟不代表真实微信客户端已经验收，也不保证同时启用多个同步工具时所有内容变化都能合并。

问题反馈与功能建议请提交到 [统一反馈仓库](https://github.com/QuickerHub/cea-action-issues/issues)。
