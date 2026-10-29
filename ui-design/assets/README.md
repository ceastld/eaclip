# 对齐样例资源

`clip-main-thumbnail.png` 是本仓库生成的确定性色块、圆形与网格，尺寸 320 × 180，不含用户剪贴板数据。两端嵌入相同 PNG，不读取实际图片目录。

## 图标

2026-10-12 用户认为原 Font Awesome 5 Light 图标在 GPUI 中过粗、变形，要求重新设计。现有 SVG 为本仓库原创绘制，替换此前从 `clipListItemFaIcons.tsx` / `ClipHostTitleBar.tsx` 提取的路径；文件名与 `clip-main.tokens.json` 的 `icons` 键保持不变。

2026-10-14 用户反馈 GPUI 图标发糊，确认入口重排提案后全部改为 16 网格（Quicker 便捷粘贴专用的 `quick-paste-*` 仍为原 Font Awesome 路径）。

绘制规则：

- `viewBox="0 0 16 16"`，必须为正方形。GPUI 0.2.2 按**宽度**统一缩放 SVG 并绘入正方形区域，非正方形 viewBox 会被拉伸或裁切。
- 只用描边：`fill="none" stroke="currentColor" stroke-width="1"`，端点与转角为圆角；横线 y、竖线 x 落在 `.5`，16 逻辑像素在 100% 下正好占一列像素。少量实心点（`more`、`list`）用 `fill="currentColor" stroke="none"`。
- 两端都把 SVG 当单色蒙版使用（前端 `mask-image`，GPUI alpha 蒙版），颜色由调用处决定，不在 SVG 内写死颜色。
- 独立面板的刷新、后退入口使用共享 `refresh.svg` / `chevron-left.svg`，沿同一 16 网格与 1 描边规则。
- 互传设备使用 `desktop.svg`、`laptop.svg`、`phone.svg`，分别表示显示器、笔记本与手机，按设备类型选择；同样使用 16 网格、1 描边和单色蒙版。
- 默认按 16 逻辑像素显示，不再用 14/15 这类在 125% 缩放下落到半像素的尺寸。GPUI 统一经 `crisp_svg::svg()` 绘制：先把尺寸和起点取整到物理像素再交给 `paint_svg`，避免 2 倍栅格贴到半像素后的二次模糊。

前端 `/dev/gpui-alignment` 舞台下方的「共享图标」面板列出全部图标的三种尺寸，修改后先在此预览，再用 `host-tauri/scripts/Test-GpuiPreview.ps1 -Capture` 核对原生渲染（脚本使用现有 `target/debug` 程序，需先 `cargo build -p cliphost-gpui`）。
