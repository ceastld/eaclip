/**
 * Static command documentation for commands.mdx (DocCommandCatalog).
 * Mirrors IntelliTools.Public.Verb (*Verb.cs) and CommandRunner.Convert legacy aliases.
 */

export interface DocCommandExampleRow {
  line: string;
  hint: string;
}

export interface DocCommandExampleSection {
  title?: string | null;
  rows: DocCommandExampleRow[];
}

export interface DocCommandItem {
  command: string;
  label: string;
  description?: string | null;
  exampleSections?: DocCommandExampleSection[] | null;
}

export const commandDocCatalogItems: DocCommandItem[] = [
  {
    command: "paste",
    label: "粘贴",
    description:
      "从剪贴板历史粘贴条目。索引从 0 起；也可用 -n 表示「第几条」（= 索引+1）。加 -c / --current 按「当前排序」粘贴。",
    exampleSections: [
      {
        title: null,
        rows: [
          { line: "paste", hint: "按界面当前选中项粘贴" },
          { line: "paste --next", hint: "粘贴后自动选下一项" },
          { line: "paste 0", hint: "粘贴索引 0" },
          { line: "paste -i 2", hint: "粘贴索引 2" },
          { line: "paste -n 1", hint: "粘贴第 1 条（同索引 0）" },
          { line: "paste -c", hint: "按最近使用等当前排序粘贴选中项" },
          { line: "paste 3 -c", hint: "索引 3 + 当前排序" },
          { line: "paste -n 2 -c", hint: "第 2 条 + 当前排序" }
        ]
      },
      {
        title: "从 clip 旧写法转换",
        rows: [
          { line: "clip paste 0", hint: "等价于 paste 0" },
          { line: "clip paste_current", hint: "等价于 paste -c" },
          { line: "clip paste_current 1", hint: "等价于 paste 1 -c" }
        ]
      }
    ]
  },
  {
    command: "clip",
    label: "剪贴板",
    description:
      "剪贴板窗口显示/隐藏、监听开关、手动处理变更、钉住等。实际由 ClipHost 执行时需能连上宿主进程。",
    exampleSections: [
      {
        title: "窗口与页签",
        rows: [
          { line: "clip show", hint: "显示窗口" },
          { line: "clip hide", hint: "隐藏窗口" },
          { line: "clip toggle", hint: "显示或隐藏" },
          { line: "clip show_or_hide", hint: "与 toggle 兼容的旧写法" },
          { line: "clip close", hint: "关闭（实现同 hide）" },
          { line: "clip show -t clip", hint: "打开并切到剪贴板页" },
          { line: "clip show -t favor", hint: "打开并切到收藏页" },
          { line: "clip show --focus-search", hint: "打开后聚焦工具栏搜索（宿主 SPA）" },
          { line: "clip show -t clip --focus-search", hint: "剪贴板页并聚焦搜索" }
        ]
      },
      {
        title: "监听与数据",
        rows: [
          { line: "clip start", hint: "开启剪贴板监听" },
          { line: "clip stop", hint: "关闭监听" },
          { line: "clip start_or_stop", hint: "切换监听开关" },
          { line: "clip handle", hint: "手动处理一次剪贴板变更" },
          { line: "clip import_old_data", hint: "历史占位，无实际操作" }
        ]
      },
      {
        title: "钉住与其它参数",
        rows: [
          { line: "clip -p true", hint: "钉住窗口（同 clip pin）" },
          { line: "clip -p false", hint: "取消钉住（同 clip pin_cancel）" },
          { line: "clip -p toggle", hint: "切换钉住（同 clip pin_change）" },
          { line: "clip -s", hint: "等价于 -p true" },
          { line: "clip hide --fold", hint: "折叠代替隐藏（-f / --fold）" },
          { line: "clip show --loc …", hint: "窗口位置（与宿主解析一致）" }
        ]
      },
      {
        title: "search 兼容（会转成 clip / favor）",
        rows: [
          { line: "search", hint: "等价 clip show -t clip --focus-search" },
          { line: "search clip", hint: "同上" },
          { line: "search favor", hint: "等价 favor show --focus-search" },
          { line: "search show", hint: "默认剪贴板页，可加 clip|favor 再跟其它参数" },
          { line: "search hide", hint: "默认 clip 页 hide，可接 favor" },
          { line: "search show_or_hide favor", hint: "收藏页切换显示，可加聚焦搜索" }
        ]
      }
    ]
  },
  {
    command: "favor",
    label: "收藏夹",
    description: "打开、隐藏或关闭收藏夹侧栏面板；可按收藏夹 id 指定目标，-a 关闭全部。",
    exampleSections: [
      {
        title: null,
        rows: [
          { line: "favor show", hint: "打开面板（无 id 时依赖默认/空项行为）" },
          { line: "favor show <id>", hint: "第二个位置参数作为收藏夹 id（同 -i）" },
          { line: "favor show -i <id>", hint: "按 id 打开" },
          { line: "favor show_or_hide", hint: "与 show 相同分支（兼容旧写法）" },
          { line: "favor hide", hint: "隐藏指定面板" },
          { line: "favor hide -i <id>", hint: "按 id 隐藏" },
          { line: "favor close", hint: "关闭指定面板" },
          { line: "favor close -a", hint: "关闭全部收藏夹面板" }
        ]
      }
    ]
  },
  {
    command: "collect",
    label: "收藏",
    description: "把剪贴板、文本、链接或图标等写入收藏夹；部分子类型仍为占位。",
    exampleSections: [
      {
        title: null,
        rows: [
          { line: "collect clip", hint: "从剪贴板收藏（同 clipboard）" },
          { line: "collect clipboard", hint: "同上" },
          { line: "collect text", hint: "收藏文本（可接 -d / 位置参数数据）" },
          { line: "collect text -t 标题 -d 内容", hint: "指定标题与正文" },
          { line: "collect link", hint: "收藏当前链接" },
          { line: "collect icon", hint: "收藏图标（配合 Title/Data）" },
          { line: "collect file", hint: "预留，尚未实现" }
        ]
      }
    ]
  },
  {
    command: "setting",
    label: "设置",
    description:
      "打开设置窗口并导航到对应页；页名为 SettingPageEnum 枚举名（不区分大小写），无法解析时回到 global。",
    exampleSections: [
      {
        title: "常用页面",
        rows: [
          { line: "setting", hint: "空页名时解析为 global" },
          { line: "setting global", hint: "全局" },
          { line: "setting favor", hint: "收藏夹" },
          { line: "setting panel", hint: "面板" },
          { line: "setting menu", hint: "菜单总览" },
          { line: "setting clip", hint: "剪贴板" },
          { line: "setting clip_filter", hint: "剪贴板（同 clip 页）" },
          { line: "setting user", hint: "用户 · 杂项偏好（含资源管理器重开）" },
          { line: "setting explorer", hint: "同 setting user（兼容旧页名）" },
          { line: "setting textAction", hint: "文本操作" },
          { line: "setting magic", hint: "魔改（导航映射）" }
        ]
      },
      {
        title: "菜单细项",
        rows: [
          { line: "setting menu_global", hint: "菜单 · 全局选项" },
          { line: "setting menu_favor", hint: "菜单 · 超级菜单" },
          { line: "setting menu_file", hint: "菜单 · 文件菜单" },
          { line: "setting menu_window", hint: "菜单 · 超级菜单（兼容旧命令）" }
        ]
      }
    ]
  },
  {
    command: "menu",
    label: "超级菜单",
    description: "弹出各类超级菜单；推荐热键直接绑专用动作「文件菜单」/「窗口菜单」/「超级菜单」，勿手改 var:cmd。file 类型也可用 -f 传入分号分隔的文件路径列表。",
    exampleSections: [
      {
        title: null,
        rows: [
          { line: "menu window", hint: "窗口菜单（专用动作「窗口菜单」）" },
          { line: "menu text", hint: "文本操作菜单" },
          { line: "menu bookmarks", hint: "书签菜单" },
          { line: "menu clipboard", hint: "剪贴板超级菜单（列表 + 预览粘贴）" },
          { line: "menu file", hint: "文件菜单（专用动作「文件菜单」；勿改超级菜单里的 cmd）" },
          { line: "menu file -f C:\\a.txt;C:\\b.txt", hint: "自定义文件菜单路径列表" },
          { line: "menu favor", hint: "收藏相关（专用动作「超级菜单」默认）" },
          { line: "menu super", hint: "同上" },
          { line: "menu reopen", hint: "执行重新打开（非仅显示菜单）" },
          { line: "menu reopen --show", hint: "显示重新打开菜单" },
          { line: "menu reopen --autorun", hint: "自动运行场景下跳过" }
        ]
      }
    ]
  },
  {
    command: "panel",
    label: "超级面板",
    description: "按收藏项打开/切换/隐藏超级面板；-i 可为新版 id 或旧版数字（会映射）。",
    exampleSections: [
      {
        title: null,
        rows: [
          { line: "panel show <id>", hint: "第二个位置参数作为面板 id（同 -i）" },
          { line: "panel show -i <id>", hint: "打开" },
          { line: "panel toggle -i <id>", hint: "显示或隐藏" },
          { line: "panel hide -i <id>", hint: "隐藏" }
        ]
      }
    ]
  },
  {
    command: "winp",
    label: "窗口位置（调试）",
    description: "对前台窗口或指定 hwnd 移动位置、改尺寸；多用于调试或自动化。",
    exampleSections: [
      {
        title: null,
        rows: [
          { line: "winp", hint: "默认操作前台窗口" },
          { line: "winp -h 0x12345", hint: "指定窗口句柄" },
          { line: "winp -m left", hint: "移动到命名区域（与 ShowLoc 解析一致）" },
          { line: "winp -p 10 20", hint: "移动左上角到 (10,20)" },
          { line: "winp -p 0 0 100 200", hint: "SetWindowPos 四元组" },
          { line: "winp -s 800 600", hint: "设置客户区宽高" }
        ]
      }
    ]
  }
];
