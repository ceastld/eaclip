# 固定 GPUI 0.2.2 的本地补丁

来源：crates.io 发布包 `gpui-0.2.2.crate`，对应 Zed 仓库 `69e2130295c2649963eb639fc70b4f2ee8ea1624`，包内 `.cargo_vcs_info.json` 标记 `dirty: true`；以发布包字节而不是 Git 提交内容为准。

- 原始包 SHA-256：`979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`。
- 原始下载位置：<https://static.crates.io/crates/gpui/gpui-0.2.2.crate>。
- 许可：[Apache-2.0](gpui/LICENSE-APACHE)，原始 README、manifest、资源及来源记录均保留。
- workspace 通过 `[patch.crates-io]` 固定此目录，仍使用 GPUI `=0.2.2`，不升级其他 GPUI 组件。

本地补丁：

- `gpui/src/elements/text.rs` 配合 `gpui/src/text_system/line_wrapper.rs`，对同时指定折行、行数上限和省略号的文本，按实际换行位置和字形宽度裁切末行。原始逻辑行仍用于隐私遮挡。

- `gpui/src/text_system/line.rs` 和 `gpui/src/elements/text.rs` 提供可选的文字绘制遮挡。`TextStyle::mask_sensitive_text` 默认关闭，应用在剪贴板与收藏内容区域通过同名 `Styled` 方法显式启用。应用按原始逻辑行给出 UTF-8 字节范围，绘制时将同一可见行内连续的敏感字形合并为柔和模糊带，使用既有阴影模糊绘制。敏感原字形跳过绘制，截断显示使用原行识别，测量、选区、字节偏移和输入内容沿用原布局。可编辑控件通过 `ShapedLine::paint_with_text_mask` 跳过遮挡。

- `gpui/src/{platform,window}.rs`、`src/platform/windows.rs`、`src/platform/windows/{platform,window,events,frame_request}.rs`：Windows VSync 仅在脏视图、输入后的短暂呈现或下一帧回调有工作时唤醒。每窗合并请求，通过独立消息驱动隐藏窗的资源退役回调；处理回调前确认请求，允许动画继续预约下一帧，销毁窗口时取消请求并校验窗口代次。原 VSync 节奏和设备恢复逻辑保留。`crates/cliphost-gpui/tests/frame_request.rs` 验证空闲、合并、确认及窗口销毁；`examples/idle_frame_probe.rs` 用真实隐藏 renderer 验证停帧、回调链和脏视图唤醒，不启动业务宿主或访问剪贴板。
- `gpui/src/elements/{anchored,anchored_bounds,mod}.rs`：锚定弹层按真实子元素包围盒测量，不将窗口原点并入尺寸。修复滚动内容位于负坐标时弹层高度被放大、上展菜单被错误推到窗口顶部；`crates/cliphost-gpui/tests/anchored_bounds.rs` 引入同一测量源码验证负 X／Y、平移后多子元素及空集合。
- `gpui/src/window.rs`：新增返回 `()` 的 `Window::dispatch_input(PlatformInput, &mut App)`，调用原有 `dispatch_event`。原方法虽为 public，返回的 `DispatchEventResult` 却是 crate-private，宿主直接调用会触发 Rust 隐私检查。补丁不改事件派发行为，也不开放原结果类型。
- `gpui/src/platform.rs`：`Arc<Image>` 的 WebP 解码与现有 Resource 路径一致，保留动画全部帧、帧间隔和 BGRA 转换，避免受控历史图缓存把动画变成静态首帧。
- `gpui/Cargo.toml`、`src/platform.rs`、`src/window.rs`、`src/platform/windows/directx_atlas.rs`：新增默认关闭的 `image-cache-diagnostics`，供隔离原生绘制测试读回实际图片图块、纹理页及字节数；普通产品构建不包含该入口，不改变分配与淘汰行为。
- `gpui/build.rs`：Windows Release shader 编译优先验证显式 `GPUI_FXC_PATH`，从 `where.exe` 的多行输出选择单个有效路径，随后从已安装 SDK 的版本目录发现 `fxc.exe`。按 Cargo `HOST` 选择 ARM64／x64 工具并保留可运行架构的回退，不再固定 SDK 版本或以目标 CPU 决定构建工具架构；相关环境变化会触发重新构建。
- `gpui/src/platform/windows/{events,display,display_change}.rs`：显示器断连及息屏恢复的 `WM_DISPLAYCHANGE` 只刷新最近显示器信息，不再调用 `ShowWindow(SW_SHOWNORMAL)`，避免隐藏主窗和透明浮层自行显示、最小化窗口被恢复或夺取焦点。显示器暂时全部消失、枚举或 DPI 查询发生竞态时保留旧 display，等待后续通知重试；该路径使用可失败的构造方法。无 renderer 的原生 helper 由 `crates/cliphost-gpui/tests/display_change.rs` 引入验证；visible/minimized 采用隐藏自有 HWND 的样式位 fixture，不实际呈现测试窗口。
- `gpui/src/platform/windows/{directx_renderer,directx_resource_lifecycle}.rs`：DirectX 设备恢复和 swap-chain resize 改用明确的 `Option` 退役状态及普通 RAII；先退役旧资源、完整构造成功后才交付新资源，失败与重试不会重复释放 COM 或读取已释放状态。尺寸与 DirectComposition 选项在恢复外保留；resize 失败后暂停绘制并允许同尺寸重试。无 DirectX 的确定性故障注入通过 `crates/cliphost-gpui/tests/directx_resource_lifecycle.rs` 覆盖分阶段构造失败、重复恢复、失败后析构及 resize 部分构造。该修复来自代码审计，尚不能证明是本机某次进程消失的原因；测试不代表真实设备丢失与息屏恢复验收。
- `gpui/src/platform/windows/{directx_renderer,directx_shader}.rs`：Debug 将两个 HLSL 程序与共享 `alpha_correction.hlsl` 内嵌，按原 Debug 编译选项从内存调用 `D3DCompile`；不再在每次新建窗口时读取构建机的绝对源码路径，避免构建工作树清理后设置等新窗口报 `Creating DirectX renderer`。Release 沿用构建时预编译字节码。`crates/cliphost-gpui/tests/directx_embedded_shaders.rs` 在空目录子进程中编译全部八组 vertex／fragment 入口，检查真实 DXBC；测试模块的编译期 manifest 根也不含旧加载器所需的 shader 文件，不创建窗口或 GPU 设备。

Windows 动画通过 `request_animation_frame` 在隐藏／最小化时暂停，Show／restore 后强制重绘恢复；普通 `on_next_frame` 不受此限制。`idle_frame_probe` 另验证持续动画的显示恢复和隐藏停帧。

ClipHost 只用该入口向本进程已注册的收藏／面板窗口转发拖动事件，以及在自动滚动后重新命中目标；不向其他应用注入输入。调用仍在 GPUI UI 线程，跨窗口操作避开当前视图借用，COM 回调不调用此 API。

核验发布包与本地目录时，仅允许上述文件的明确补丁，以及新增的 `src/elements/anchored_bounds.rs`、`src/platform/windows/display_change.rs`、`src/platform/windows/directx_resource_lifecycle.rs`、`src/platform/windows/directx_shader.rs`、`src/platform/windows/frame_request.rs`。来源、版本和许可保持；扩展图片补丁须通过静态图、GIF、动态 WebP 及隔离 Windows 原生绘制回收测试，拖动补丁仍按既有验收核对。
