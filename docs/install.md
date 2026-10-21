# 安装与配置

适用版本：eaclip / Rust ClipHost 0.31.12 及以后（GPUI 原生界面），按 0.31.26 编写。当前最新公开版本见 [eaclip Releases](https://github.com/ceastld/eaclip/releases/latest)。

GPUI 版本**不需要 WebView2**，也不需要 .NET 运行时。**不安装 Quicker 也能使用**剪贴板历史与收藏（便携包直接运行）；Quicker 动作只是安装和唤起入口之一。

## 选哪种架构

| Windows | 选择 |
| --- | --- |
| 普通 Intel / AMD 64 位电脑 | x64 |
| Windows on ARM（如骁龙笔记本） | ARM64 |

在 ARM64 Windows 上，即使 Quicker 以 x64 模拟方式运行，Quicker 安装入口也会按系统原生架构选择 ARM64 宿主。

ARM64 的已知边界见 [已知问题](known-issues.md#arm64)。

## 方式一：通过 Quicker 动作「剪贴板 n10」

1. 在 Quicker 中安装分享动作 [剪贴板 n10](https://getquicker.net/Sharedaction?code=df6cc8fc-1a67-4576-fcb0-08deace01986)。
2. 运行该动作。动作先从 Quicker 官方依赖库下载入口 DLL，再由入口 DLL 按系统架构下载并安装宿主（来源为 Quicker 官方依赖库，Bitiful 为备用源）。
3. 宿主安装在 `%LOCALAPPDATA%\Programs\ClipHostRust`（程序为其中的 `cliphost-app.exe`）。
4. 之后再运行该动作，会唤起已在运行的程序（显示 / 隐藏剪贴板窗口）。

版本要求（0.31.26）：

| 项目 | 要求 |
| --- | --- |
| 宿主版本 | 剪贴板 n10 与超级菜单 n10 使用宿主 `0.31.26.0` |
| Quicker V1 / V2 入口 | 0.31.24 起更新为 `0.31.24.1`（用于新的动作查询与搜索聚焦能力） |
| 基本兼容下限 | x64 入口最低 `0.31.7.0`，ARM64 入口最低 `0.31.14.1`（保持不变） |

公开分享动作从 0.31.14 起已改用"按系统架构下载宿主"的新版安装入口（0.31.13 说明里"尚未切换"的状态已过时）。

> 待确认：「剪贴板 n10」分享页当前标注的最低 Quicker 版本（仓库内没有记录，请以分享页为准）。

## 方式二：便携包

1. 打开 [eaclip Releases](https://github.com/ceastld/eaclip/releases/latest)，按架构下载 `eaclip-<版本>-windows-x64.zip` 或 `eaclip-<版本>-windows-arm64.zip`。
2. 可选：用同名的 `.SHA256SUMS.txt` 核对 ZIP 的 SHA-256。
3. **完整解压整个目录**，运行 `cliphost-app.exe`，不要单独拿出 EXE（同目录 DLL 需要保留）。

便携包里还附带启动诊断入口 `Start-ClipHostDiagnostics.cmd`，用法见 [常见问题](faq.md#启动失败或闪退)。

## 便携包与 Quicker 安装版能同时用吗？

按程序代码核对的结论：

- **不能同时运行两份。** 程序是"同一用户只允许一个实例"，与 EXE 放在哪里无关。已经有一份在运行时，再启动另一份（无论便携版还是安装版）只会把已运行的那份窗口唤出来，不会开第二个。
- **两者共用同一份数据。** 数据位置不跟随程序目录，而是由 `%LOCALAPPDATA%\BetterClipTauri` 里的位置记录决定（见下文"数据位置"），所以便携版和安装版看到的是同一份历史、收藏和配置。
- **开机自启指向最后一次启动的那份程序。** "开机自启"开启时，程序每次启动都会把自启项改写成当前这份 EXE 的路径。换用另一份程序后，先启动一次新的那份即可；删除某份程序目录前，见下文"卸载"。

## 升级

- 用 Quicker 安装的，运行「剪贴板 n10」时按动作要求的最低宿主版本自动更新。
- 便携包：先从托盘退出程序，再下载新版本 ZIP 解压替换。
- 0.31.x 各版本说明均写明：升级**不移动用户数据**，继续读取原有历史、收藏、配置及图片目录，也不会自动迁移到实验中的新存储格式。

## 数据位置

在 **设置 → 数据管理 → 数据文件夹** 查看或修改。

- 首次启动时，程序会按顺序检测已有的旧版数据并**直接使用**（不搬动、不复制）：旧版 ClipHost 记录的自定义数据位置 → `文档\Quicker\Ceastld` → `%LOCALAPPDATA%\BetterClip\userdata`（以存在 `userdata.db` 为准）。都没有时使用默认位置 `%LOCALAPPDATA%\BetterClipTauri`。选中的位置会记住，之后不再重新检测。
- 位置记录文件固定在 `%LOCALAPPDATA%\BetterClipTauri\application-data-root.json`。
- 在设置中留空即恢复默认位置。
- 自定义位置需填写**程序目录以外**的绝对路径；保存时会确认并重启。**更改位置不会自动迁移数据**，需要的话请先退出程序、自行复制数据文件夹。
- 数据文件夹里主要有：`userdata.db`（历史与收藏数据库）、`Configs`（配置）、`Image`（图片，含缩略图缓存）、`blobs`、`logs`（日志）、`LanShare\Inbox`（局域网收到的文件）。

## 开机自启与权限

在 **设置 → 全局 → 系统**：

| 选项 | 默认 | 说明 |
| --- | --- | --- |
| 开机自启 | 开启 | 当前用户登录 Windows 时在后台启动（只进托盘，不弹窗口）。写入当前用户的注册表启动项 `IntelliTools.ClipHost` |
| 以管理员身份运行 | 关闭 | 冷启动时尝试管理员权限；更改后按提示重启 |

## 卸载

程序没有单独的卸载程序，按下面步骤手动清理（依据程序代码整理）：

1. **先在设置里关掉会改系统的选项**：
   - **设置 → 全局 → 系统 → 开机自启**：关闭，程序会删除注册表启动项。
   - 如果开过 **设置 → 触发 → 使用 Win+V 打开剪贴板**：先关闭它，程序会恢复系统 Win+V 剪贴板历史的注册表项，并提示重启资源管理器。
2. **退出程序**：托盘图标右键 → 退出。
3. **删除程序文件**：
   - Quicker 安装版：删除 `%LOCALAPPDATA%\Programs\ClipHostRust`。
   - 便携版：删除解压出来的整个目录。
4. **（可选）删除数据**——删除后历史、收藏、配置无法恢复，建议先备份：
   - 数据文件夹（以 **设置 → 数据管理** 显示的位置为准，默认 `%LOCALAPPDATA%\BetterClipTauri`）。
   - `%LOCALAPPDATA%\BetterClipTauri`（即使数据放在别处，这里也存有位置记录文件）。
   - `%LOCALAPPDATA%\ClipHostRust`（诊断输出等）。
5. （可选）在 Quicker 里删除「剪贴板 n10」「超级菜单 n10」动作。

如果第 1 步之前程序已经被删掉：

- 自启项：在 `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` 里删除名为 `IntelliTools.ClipHost` 的值（也可在任务管理器"启动应用"里禁用）。
- Win+V：在 `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced` 的 `DisabledHotkeys` 值里去掉字母 `V`，然后重启资源管理器。修改注册表有风险，不熟悉的话可以重新运行程序、关掉选项后再删除。

> 待确认：Quicker 依赖库在本机缓存的入口 DLL / 安装包是否需要、以及如何手动清理。
