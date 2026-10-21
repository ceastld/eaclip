# Rust ClipHost 0.31.24

发布日期：2026-10-21。

## 更新

- 新增聚合搜索窗口，可同时查找收藏、剪贴板历史及已安装的 Quicker 动作，支持关键词与拼音。搜索结果分批显示，较慢的动作查询不会阻塞本地结果。
- 改进搜索窗口的唤起聚焦、搜索栏拖动、默认居中和中英文显示；保留查询及同一会话内调整过的窗口位置。
- 保全截图软件等产生的已知临时图片，避免原文件回收后历史记录无法使用。普通文件和多文件记录继续保留原引用。
- 文件历史仅在展示时检查；普通未置顶记录全部失效并经过至少 24 小时后，再次确认才清理记录，保留收藏与外部文件。
- 修复部分 Windows 位图的读取兼容性，改进局域网图片同步的像素一致性；文本、图片和第三方重复写入继续按内容去重。
- 调整超级菜单展开按钮与行高，改善文件级联菜单的紧凑布局。

剪贴板 n10 与超级菜单 n10 同步使用宿主 `0.31.24.0`。Quicker V1/V2 入口更新为 `0.31.24.1`，用于新的动作查询与搜索聚焦能力；基本宿主兼容下限保持不变。无需安装 Quicker 也可使用剪贴板历史与收藏。

## 下载

按 Windows 系统架构选择 ZIP，完整解压后运行 `cliphost-app.exe`，保留同目录 DLL。GPUI 原生界面无需 WebView2，沿用原有数据目录。

| Windows 架构 | 便携包 | SHA-256 校验文件 |
|---|---|---|
| x64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.24/eaclip-0.31.24-windows-x64.zip) | [x64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.24/eaclip-0.31.24-windows-x64.SHA256SUMS.txt) |
| ARM64 | [GitHub 下载](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.24/eaclip-0.31.24-windows-arm64.zip) | [ARM64 SHA256SUMS.txt](https://github.com/ceastld/eaclip/releases/download/rust-v0.31.24/eaclip-0.31.24-windows-arm64.SHA256SUMS.txt) |

## 验证范围

Windows 双机已验证文本与图片双向同步、重复内容去重、接收类型开关，以及仅历史／仅剪贴板两种处理方式。正式包另按两种架构检查回归、隔离启动、文件身份和原始 ZIP 扫描。

设备同步配置的简化界面仍处于设计预览，本版继续使用现有设置入口。真实微信客户端、三台以上设备、物理输入法及 ARM64 第三方应用回贴未由上述检查替代。

问题反馈与功能建议请提交到 [统一反馈仓库](https://github.com/QuickerHub/cea-action-issues/issues)。
