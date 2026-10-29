//! Settings catalog / form fields aligned with C# `SettingsFormFacade` + `ConfigFormFieldDto`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::i18n::{tr, Text};

use crate::dto::ClipConfigDto;
use crate::fifo::{
    DEFAULT_BURST_GAP_SECONDS, DEFAULT_MAX_QUEUE_LENGTH, DEFAULT_SESSION_IDLE_SECONDS,
};
use crate::quick_paste::{DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT, MIN_LIST_LIMIT};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QuickPasteConfig {
    pub list_limit: i32,
    #[serde(default = "default_mouse_wheel_trigger")]
    pub mouse_wheel_trigger: String,
    pub remember_selection: bool,
    pub selection_memory_seconds: i32,
    pub shortcut1_action: String,
    pub shortcut2_action: String,
    pub enable_delete_shortcut: bool,
    pub enable_paste_enter_shortcut: bool,
    pub enable_fifo_shortcut: bool,
    pub burst_gap_seconds: i32,
    pub max_queue_length: i32,
    pub session_idle_seconds: i32,
}

impl Default for QuickPasteConfig {
    fn default() -> Self {
        Self {
            list_limit: DEFAULT_LIST_LIMIT,
            mouse_wheel_trigger: default_mouse_wheel_trigger(),
            remember_selection: true,
            selection_memory_seconds: 10,
            shortcut1_action: "Auto".into(),
            shortcut2_action: "Auto".into(),
            enable_delete_shortcut: true,
            enable_paste_enter_shortcut: true,
            enable_fifo_shortcut: true,
            burst_gap_seconds: DEFAULT_BURST_GAP_SECONDS as i32,
            max_queue_length: DEFAULT_MAX_QUEUE_LENGTH as i32,
            session_idle_seconds: DEFAULT_SESSION_IDLE_SECONDS as i32,
        }
    }
}

fn default_mouse_wheel_trigger() -> String {
    "none".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GlobalHotkeys {
    #[serde(default = "default_clip_hotkey")]
    pub hotkey_show_clipboard: String,
    #[serde(default = "default_favor_hotkey")]
    pub hotkey_favor_toggle: String,
    #[serde(default = "default_quick_paste_hotkey")]
    pub hotkey_quick_paste: String,
    #[serde(default = "default_super_menu_hotkey")]
    pub hotkey_super_menu: String,
}

fn default_favor_hotkey() -> String {
    crate::global_hotkey::DEFAULT_FAVOR_TOGGLE.into()
}

fn default_clip_hotkey() -> String {
    crate::global_hotkey::DEFAULT_CLIP_TOGGLE.into()
}

fn default_quick_paste_hotkey() -> String {
    crate::global_hotkey::DEFAULT_QUICK_PASTE.into()
}

fn default_super_menu_hotkey() -> String {
    crate::global_hotkey::DEFAULT_SUPER_MENU.into()
}

impl Default for GlobalHotkeys {
    fn default() -> Self {
        Self {
            hotkey_show_clipboard: default_clip_hotkey(),
            hotkey_favor_toggle: default_favor_hotkey(),
            hotkey_quick_paste: default_quick_paste_hotkey(),
            hotkey_super_menu: default_super_menu_hotkey(),
        }
    }
}

fn field(
    key: &str,
    label: &str,
    description: &str,
    group: &str,
    input_type: &str,
    order: i32,
    value: Value,
) -> Value {
    json!({
        "key": key,
        "label": label,
        "description": description,
        "group": group,
        "inputType": if input_type == "checkbox" {
            "boolean"
        } else if input_type == "select" {
            "enum"
        } else {
            input_type
        },
        "order": order,
        "min": Value::Null,
        "max": Value::Null,
        "step": Value::Null,
        "presets": [],
        "options": [],
        "selectionItems": [],
        "stretchControl": false,
        "formEditor": "default",
        "value": value,
    })
}

fn number_field(
    key: &str,
    label: &str,
    description: &str,
    group: &str,
    order: i32,
    value: i32,
    min: i32,
    max: i32,
    step: i32,
) -> Value {
    let mut f = field(key, label, description, group, "number", order, json!(value));
    f["min"] = json!(min);
    f["max"] = json!(max);
    f["step"] = json!(step);
    f
}

/// C# `SettingsFormFacade.SerializeValue` for enums: `ToString().ToLowerInvariant()`.
fn enum_form_value(raw: &str) -> String {
    raw.to_lowercase()
}

fn enum_field(
    key: &str,
    label: &str,
    description: &str,
    group: &str,
    order: i32,
    value: &str,
    items: &[(&str, Text, Text)],
) -> Value {
    let mut f = field(
        key,
        label,
        description,
        group,
        "enum",
        order,
        json!(enum_form_value(value)),
    );
    f["options"] = json!(items.iter().map(|(k, _, _)| *k).collect::<Vec<_>>());
    f["selectionItems"] = json!(items
        .iter()
        .map(|(k, name, des)| json!({ "key": k, "name": name.get(), "des": des.get() }))
        .collect::<Vec<_>>());
    f
}

fn process_names_field(
    key: &str,
    label: &str,
    description: &str,
    group: &str,
    order: i32,
    value: &str,
) -> Value {
    let mut f = field(key, label, description, group, "text", order, json!(value));
    f["formEditor"] = json!("processnames");
    f["stretchControl"] = json!(true);
    f
}

const DISMISS_ITEMS: &[(&str, Text, Text)] = &[
    ("hide", Text("普通隐藏", "Hide"), Text("失去焦点后完全隐藏窗口。", "Hide the window completely when it loses focus.")),
    ("edgehide", Text("贴边隐藏", "Hide at edge"), Text("窗口贴屏幕边缘时自动收起，鼠标移入边缘再次展开。", "Collapse at the screen edge; move the pointer to the edge to expand.")),
    ("fold", Text("自动折叠", "Fold"), Text("失去焦点后收成标题栏，鼠标移入再展开。", "Fold to the title bar when focus is lost; hover to expand.")),
];

const TOAST_PLACEMENT_ITEMS: &[(&str, Text, Text)] = &[
    ("bottomcenter", Text("屏幕底部居中", "Bottom center"), Text("在工作区底部水平居中显示提示。", "Show notifications at the bottom center of the work area.")),
    ("topright", Text("屏幕右上", "Top right"), Text("在工作区右上角显示提示。", "Show notifications at the top right of the work area.")),
    ("bottomright", Text("屏幕右下", "Bottom right"), Text("在工作区右下角显示提示。", "Show notifications at the bottom right of the work area.")),
];

const EXECUTE_MODE_ITEMS: &[(&str, Text, Text)] = &[
    ("singleclickpaste", Text("单击粘贴", "Click to paste"), Text("单击列表项即粘贴。", "Click a list item to paste it.")),
    (
        "copythendoubleclickpaste",
        Text("单击复制后双击粘贴", "Click to copy, double-click to paste"),
        Text("第一次单击复制到剪贴板，第二次双击粘贴。", "Click to copy to the clipboard; double-click to paste."),
    ),
    ("doubleclickonly", Text("仅双击粘贴", "Double-click to paste"), Text("仅双击列表项时粘贴。", "Paste only when a list item is double-clicked.")),
];

const LAYOUT_DENSITY_ITEMS: &[(&str, Text, Text)] = &[
    ("compact", Text("紧凑", "Compact"), Text("默认列表间距。", "Default list spacing.")),
    ("relaxed", Text("宽松", "Relaxed"), Text("增大条目内边距与区块间距，便于阅读与点击。", "Add padding and space between sections for easier reading and clicking.")),
];

const SHOW_WINDOW_LOCATION_ITEMS: &[(&str, Text, Text)] = &[
    ("na", Text("自动，一般是上次位置", "Auto (usually last position)"), Text("", "")),
    ("centerscreen", Text("屏幕中心", "Screen center"), Text("", "")),
    ("mousearound", Text("鼠标周围", "Near pointer"), Text("", "")),
    ("mouserightbottom", Text("鼠标右下", "Below-right of pointer"), Text("", "")),
];

const NUMBER_KEY_PASTE_DESCRIPTION: Text = Text("例如，主键盘上的数字，或者小键盘上的数字键都可以\n输入1就粘贴第1项\n输入9就粘贴第9项\n输入0就粘贴第10项", "Use number keys on the keyboard or numpad.\n1 pastes the first item.\n9 pastes the ninth item.\n0 pastes the tenth item.");

pub fn catalog() -> Value {
    json!({
        "configs": [
            { "id": "global", "label": tr("全局", "Global") },
            { "id": "trigger", "label": tr("触发", "Triggers") },
            { "id": "quickPaste", "label": tr("便捷粘贴", "Quick paste") },
            { "id": "clip", "label": tr("剪贴板", "Clipboard") },
            { "id": "panel", "label": tr("面板", "Panel") },
            { "id": "user", "label": tr("资源管理器", "File Explorer") },
            { "id": "menu_global", "label": tr("超级菜单", "Super menu") },
            { "id": "menu_file", "label": tr("文件菜单", "File menu") },
            { "id": "dataLocation", "label": tr("数据管理", "Data management") },
            { "id": "textAction", "label": tr("文本操作", "Text actions") }
        ]
    })
}

/// C# `SettingsFormFacade.ResolveConfigId` then `ConfigMap` / virtual page lookup.
pub fn resolve_form_config_id(raw: &str) -> String {
    let page = raw.trim().replace('.', "_");
    if page.eq_ignore_ascii_case("hotkeys") {
        return "trigger".into();
    }
    // C# `ResolveMenuSettingsConfigId` string patterns are ordinal (case-sensitive).
    match page.as_str() {
        "menu" | "menu_global" | "menu_favor" | "menu_window" => "menu_global".into(),
        "menu_file" => "menu_file".into(),
        _ => page,
    }
}

const KNOWN_FORM_IDS: &[(&str, &str)] = &[
    ("clip", "clip"),
    ("quickPaste", "quickPaste"),
    ("trigger", "trigger"),
    ("global", "global"),
    ("homeGlobal", "homeGlobal"),
    ("dataLocation", "dataLocation"),
    ("panel", "panel"),
    ("user", "user"),
    ("menu_global", "menu_global"),
    ("menu_file", "menu_file"),
    ("textAction", "textAction"),
];

/// C# `BuildForm` / `ApplyForm`: unknown → `Unknown config id: {resolved}`.
pub fn canonical_form_config_id(raw: &str) -> Result<String, String> {
    let resolved = resolve_form_config_id(raw);
    for (known, canonical) in KNOWN_FORM_IDS {
        if resolved.eq_ignore_ascii_case(known) {
            return Ok((*canonical).into());
        }
    }
    Err(format!("Unknown config id: {resolved}"))
}

pub fn build_form(
    config_id: &str,
    clip: &ClipConfigDto,
    quick_paste: &QuickPasteConfig,
    hotkeys: &GlobalHotkeys,
    data_root: &str,
    extras: &crate::extra_settings::ExtraSettings,
    filters: &[crate::dto::ClipFilterItemDto],
) -> Value {
    let (label, fields) = match config_id {
        "clip" => (tr("剪贴板", "Clipboard"), clip_fields(clip, filters)),
        "quickPaste" => (tr("便捷粘贴", "Quick paste"), quick_paste_fields(quick_paste, hotkeys)),
        "trigger" | "hotkeys" => (tr("触发", "Triggers"), trigger_fields(hotkeys, clip)),
        "global" => (tr("全局", "Global"), crate::extra_settings::global_appearance_fields(&extras.global)),
        "homeGlobal" => (tr("系统", "System"), crate::extra_settings::home_global_fields(&extras.global)),
        "dataLocation" => (tr("数据管理", "Data management"), data_location_fields(data_root)),
        "panel" => (tr("面板", "Panel"), crate::extra_settings::panel_fields(&extras.panel)),
        "user" => (tr("资源管理器", "File Explorer"), crate::extra_settings::user_fields(&extras.user)),
        "menu_global" => (tr("超级菜单", "Super menu"), crate::extra_settings::menu_global_fields(&extras.menu_global)),
        "menu_file" => (tr("文件菜单", "File menu"), crate::extra_settings::file_menu_fields(&extras.file_menu)),
        "textAction" => (tr("文本操作", "Text actions"), Vec::new()),
        _ => (tr("全局", "Global"), crate::extra_settings::global_appearance_fields(&extras.global)),
    };
    json!({
        "configId": config_id,
        "configLabel": label,
        "fields": fields,
        "messages": [],
    })
}

fn clip_fields(cfg: &ClipConfigDto, filters: &[crate::dto::ClipFilterItemDto]) -> Vec<Value> {
    // C# `SettingsFormFacade.BuildFields(ClipConfig)` declaration order, then
    // `ExcludeFieldGroups(Const_Trigger)` + `InsertClipFilterItemsField` (Order 0 prepend).
    let fields = vec![
        {
            let mut filter = field(
                "FilterItems",
                tr("分类标签", "Category tabs"),
                tr("剪贴板顶部分类选项（文本、图片、文件等）。可增删、改名、改类型与排序。置顶分栏由「单独显示置顶项」控制。", "Categories above the clipboard list (text, images, files, etc.). Add, remove, rename, change types or reorder them. The pinned tab is controlled by the separate pinned items option."),
                tr("列表筛选", "List filters"),
                "text",
                1,
                json!(crate::clip_filter::serialize_form_value(filters)),
            );
            filter["formEditor"] = json!("filterItems");
            filter["stretchControl"] = json!(true);
            filter
        },
        process_names_field(
            "BlackList",
            tr("黑名单", "Blocked processes"),
            tr("当前台进程为黑名单中的进程时，不收集剪贴板内容。进程名不区分大小写，可带或不带 .exe，多个用逗号或分号分隔。", "Skip clipboard capture while a listed process is in the foreground. Names are case-insensitive, with or without .exe; separate them with commas or semicolons."),
            tr("控制", "Behavior"),
            2,
            &cfg.black_list,
        ),
        {
            let mut max_items = number_field(
                "MaxItemSize",
                tr("记录历史条目上限", "History item limit"),
                tr("仅约束非置顶历史；置顶不计入此上限。超出后大约每 10 秒自动清理较早的非置顶项；在设置中调低上限会先确认再永久删除超出部分。", "Limits unpinned history only. Older unpinned items are cleaned up about every 10 seconds. Lowering the limit asks for confirmation before permanently deleting excess items."),
                tr("控制", "Behavior"),
                3,
                cfg.max_item_size,
                1,
                1,
                1,
            );
            // C# `NumEditorProperty(1, double.MaxValue)` → `NormalizeNumericBoundMax` null.
            max_items["max"] = Value::Null;
            max_items["presets"] = json!([25.0, 100.0, 500.0, 9999.0]);
            max_items["formEditor"] = json!("numberPresets");
            max_items["stretchControl"] = json!(true);
            max_items
        },
        field(
            "ClearFilterWhileHide",
            tr("隐藏窗口后清空筛选", "Clear filters when hidden"),
            "",
            tr("控制", "Behavior"),
            "checkbox",
            4,
            json!(cfg.clear_filter_while_hide),
        ),
        enum_field(
            "DismissBehavior",
            tr("窗口失焦行为", "When focus is lost"),
            tr("失去焦点时的窗口表现：普通隐藏、贴边隐藏或自动折叠为标题栏。", "Hide, collapse at the screen edge, or fold to the title bar when focus is lost."),
            tr("控制", "Behavior"),
            5,
            &cfg.dismiss_behavior,
            DISMISS_ITEMS,
        ),
        field(
            "EnableNoFocusMode",
            tr("无焦点模式", "No-focus mode"),
            tr("开启时剪贴板窗口不抢焦点即可输入；关闭后打开窗口会激活并聚焦搜索框。", "Allow typing without taking focus. When off, opening the window activates it and focuses search."),
            tr("控制", "Behavior"),
            "checkbox",
            6,
            json!(cfg.enable_no_focus_mode),
        ),
        field(
            "AutoScrollToTop",
            tr("闲置后自动回到顶部", "Return to top after inactivity"),
            tr("列表无键盘、点击或滚动操作达到闲置时间后自动回到顶部。隐藏、折叠和贴边期间继续计时；编辑或弹层打开时暂缓，不清除搜索和筛选。", "Return to the top after no list keyboard, click or scroll activity for the idle period. Time continues while hidden, folded or edge-hidden. Defer while editing or a popup is open; keep search and filters."),
            tr("控制", "Behavior"),
            "checkbox",
            7,
            json!(cfg.auto_scroll_to_top),
        ),
        number_field(
            "AutoScrollToTopIdleSeconds",
            tr("闲置时间（秒）", "Idle time (seconds)"),
            tr("启用自动回到顶部时生效，范围为 1–86400 秒。", "Applies when automatic return to top is enabled. Range: 1–86400 seconds."),
            tr("控制", "Behavior"),
            8,
            cfg.auto_scroll_to_top_idle_seconds as i32,
            1,
            86_400,
            1,
        ),
        field(
            "CollectQuickerSteps",
            tr("收集Quicker步骤", "Capture Quicker steps"),
            "",
            tr("复制", "Copy"),
            "checkbox",
            7,
            json!(cfg.collect_quicker_steps),
        ),
        field(
            "AllowDuplicateCopy",
            tr("允许重复复制", "Allow duplicate copies"),
            "",
            tr("复制", "Copy"),
            "checkbox",
            8,
            json!(cfg.allow_duplicate_copy),
        ),
        field(
            "ShowCopyMessage",
            tr("剪贴板改变时提示", "Notify on clipboard changes"),
            tr("剪贴板改变时显示提示（需 ClipHost 在运行）。位置由下方「提示位置」控制。与「触发自定义动作」互不影响，可同时开启。", "Show a notification when the clipboard changes while ClipHost is running. The position option below controls its placement. Notifications and custom actions can both be enabled."),
            tr("复制", "Copy"),
            "checkbox",
            9,
            json!(cfg.show_copy_message),
        ),
        enum_field(
            "CopyToastPlacement",
            tr("提示位置", "Notification position"),
            tr("剪贴板改变提示与宿主通知的屏幕位置。默认右下角；也可选右上或底部居中。", "Position of clipboard and host notifications. Defaults to bottom right; top right and bottom center are also available."),
            tr("复制", "Copy"),
            10,
            &cfg.copy_toast_placement,
            TOAST_PLACEMENT_ITEMS,
        ),
        field(
            "ClipChangedActionId",
            tr("剪贴板改变时触发自定义动作", "Run an action on clipboard changes"),
            tr("填写 Quicker 自定义动作的 ID（不是动作名称）。与上方「剪贴板改变时提示」互不影响，可同时开启。优先经 Quicker 插件 RPC 调用；插件未连接时回退到 quicker:runaction 协议（需本机已安装并注册 Quicker）。", "Enter the Quicker action ID. Works independently of notifications. Uses Quicker plugin RPC, or falls back to quicker:runaction when the plugin is disconnected (requires Quicker installed and registered)."),
            tr("复制", "Copy"),
            "text",
            11,
            json!(cfg.clip_changed_action_id),
        ),
        field(
            "IgnoreWhiteSpace",
            tr("忽略空白剪贴板内容", "Ignore blank clipboard content"),
            tr("当剪贴板内容是换行符, 制表符, 空白符等字符的时候, 忽略, 不收藏", "Skip content containing only whitespace, tabs or line breaks."),
            tr("复制", "Copy"),
            "checkbox",
            12,
            json!(cfg.ignore_white_space),
        ),
        field(
            "TextProcessExpresstion",
            tr("预处理表达式", "Preprocessing expression"),
            tr("剪贴板出现文本时先执行表达式再收藏。须以 $= 或 $$ 开头（与 Quicker 相同）；$= 内可用变量 text 表示剪贴板文本。", "Evaluate an expression before saving clipboard text. Start with $= or $$, as in Quicker; $= can use the text variable for clipboard content."),
            tr("复制", "Copy"),
            "text",
            13,
            json!(cfg.text_process_expression),
        ),
        number_field(
            "DefaultPasteDelay",
            tr("粘贴前延迟", "Delay before paste"),
            tr("执行粘贴前的等待毫秒数，可避免目标窗口未就绪。", "Wait this many milliseconds before pasting so the target window can become ready."),
            tr("粘贴", "Paste"),
            14,
            cfg.default_paste_delay,
            0,
            2000,
            50,
        ),
        process_names_field(
            "UseShiftInsertToPasteProc",
            tr("使用 Shift+Insert 粘贴的进程", "Use Shift+Insert in these processes"),
            tr("从剪贴板历史来源进程中选择；与内置默认（cmd、ConsoleWindowClass 控制台等）合并生效。", "Choose from history source processes. Combined with built-in defaults such as cmd and ConsoleWindowClass consoles."),
            tr("粘贴", "Paste"),
            15,
            &cfg.use_shift_insert_to_paste_proc,
        ),
        field(
            "PasteWithNewLine",
            tr("粘贴文本时自动换行", "Append a line break when pasting text"),
            "",
            tr("粘贴", "Paste"),
            "checkbox",
            16,
            json!(cfg.paste_with_new_line),
        ),
        field(
            "PlainTextPasteByDefault",
            tr("富文本默认处理方式", "Rich text output"),
            tr("用于复制、粘贴和拖放；仅文字去掉格式及内嵌图片，独立图片、文件、动作步骤照常。", "Used for copy, paste and drag. Text only removes formatting and embedded images; standalone images, files and action steps keep their types."),
            tr("粘贴", "Paste"),
            "checkbox",
            17,
            json!(cfg.plain_text_paste_by_default),
        ),
        field(
            "StandardPaste",
            tr("默认规范粘贴", "Use standard paste by default"),
            "",
            tr("粘贴", "Paste"),
            "checkbox",
            18,
            json!(cfg.standard_paste),
        ),
        field(
            "UsingNumberKeyToPaste",
            tr("使用数字键进行粘贴", "Paste with number keys"),
            NUMBER_KEY_PASTE_DESCRIPTION.get(),
            tr("粘贴", "Paste"),
            "checkbox",
            19,
            json!(cfg.using_number_key_to_paste),
        ),
        enum_field(
            "ClipListExecuteMode",
            tr("列表项粘贴触发方式", "List paste trigger"),
            tr("单击粘贴、单击复制后双击粘贴，或仅双击粘贴。", "Click to paste, click to copy then double-click to paste, or double-click only."),
            tr("粘贴", "Paste"),
            20,
            cfg.clip_list_execute_mode_name(),
            EXECUTE_MODE_ITEMS,
        ),
        field(
            "ClipListSeparatePinnedUi",
            tr("单独分栏显示置顶项", "Show pinned items in a separate tab"),
            tr("开启后使用「置顶」分栏；关闭后置顶项混排于各分类列表。", "Show a Pinned tab when enabled; otherwise include pinned items in each category."),
            tr("粘贴", "Paste"),
            "boolean",
            21,
            json!(cfg.clip_list_separate_pinned_ui),
        ),
        field(
            "ClipListImageHoverPreviewEnabled",
            tr("图片与视频悬停预览", "Preview images and videos on hover"),
            tr("悬停图片、GIF 或视频文件时显示预览。GIF 自动播放，视频点击后静音播放；关闭后保留文字提示。", "Preview images, GIFs and video files on hover. GIFs animate automatically; click a video to play it muted. Text tooltips remain when disabled."),
            tr("粘贴", "Paste"),
            "boolean",
            22,
            json!(cfg.clip_list_image_hover_preview_enabled),
        ),
        number_field(
            "ClipListImagePreviewMaxHeightPx",
            tr("列表图片预览最大高度", "Maximum image preview height"),
            tr("历史列表中图片缩略图的最大显示高度（像素）。", "Maximum height of image thumbnails in history, in pixels."),
            tr("粘贴", "Paste"),
            23,
            cfg.clip_list_image_preview_max_height_px,
            40,
            640,
            10,
        ),
        enum_field(
            "ClipListLayoutDensity",
            tr("列表布局密度", "List density"),
            tr("紧凑为默认列表间距；宽松模式增大条目内边距与标题/正文区块间距，便于阅读与点击。", "Compact uses the default spacing. Relaxed adds padding and space between title and content for easier reading and clicking."),
            tr("粘贴", "Paste"),
            24,
            &cfg.clip_list_layout_density,
            LAYOUT_DENSITY_ITEMS,
        ),
        field(
            "DoNotRemindOnClear",
            tr("清空时不提示", "Skip confirmation when clearing"),
            "",
            tr("常规", "General"),
            "boolean",
            25,
            json!(cfg.do_not_remind_on_clear),
        ),
        number_field(
            "SplitLinesMaxCount",
            tr("按行拆分最大行数，超过了会提示", "Confirm before splitting many lines"),
            tr("按行拆分为多条历史时，超过此行数将提示确认。", "Ask for confirmation when splitting text into more than this many history items."),
            tr("常规", "General"),
            26,
            cfg.split_lines_max_count,
            10,
            5000,
            10,
        ),
    ];
    // C# never puts ClipMonitorOn / EnableEdgeHide / UiThemeMode / FontFamilyString on this form.
    debug_assert!(!fields.iter().any(|f| {
        matches!(
            f.get("key").and_then(|v| v.as_str()),
            Some("ClipMonitorOn" | "EnableEdgeHide" | "UiThemeMode" | "FontFamilyString" | "ShowWindowLocation" | "UseWinVShortcut")
        )
    }));
    fields
}

fn quick_paste_fields(cfg: &QuickPasteConfig, hotkeys: &GlobalHotkeys) -> Vec<Value> {
    // C# `BuildQuickPasteForm`: global HotkeyQuickPaste (formEditor hotkey) then
    // `BuildFields(QuickPasteConfig)` excluding Browsable(false) Shortcut1/2 / D / E / Q.
    vec![
        {
            let mut hotkey = field(
                "HotkeyQuickPaste",
                tr("便捷粘贴", "Quick paste"),
                tr("按住快捷键弹出最近剪贴板预览，松开即粘贴选中项；滚轮或方向键切换条目。留空表示禁用。", "Hold the shortcut to preview recent clipboard items; release to paste. Use the wheel or arrow keys to select. Leave blank to disable."),
                tr("快捷键", "Shortcuts"),
                "text",
                1,
                json!(hotkeys.hotkey_quick_paste),
            );
            hotkey["formEditor"] = json!("hotkey");
            hotkey
        },
        enum_field(
            "MouseWheelTrigger",
            tr("鼠标侧键 + 滚轮", "Mouse side button + wheel"),
            tr("按住所选侧键并滚动滚轮，打开便捷粘贴并切换条目；松开侧键粘贴选中项，右键取消。未滚动的侧键单击在松开时执行原操作；滚轮手势由 ClipHost 接管。", "Hold the selected side button and scroll to open quick paste and select an item. Release to paste; right-click to cancel. A side-button click without scrolling keeps its original action."),
            tr("快捷键", "Shortcuts"),
            2,
            &cfg.mouse_wheel_trigger,
            &[
                ("none", Text("关闭", "Off"), Text("保留鼠标侧键原有行为。", "Keep the mouse side button's original behavior.")),
                ("x1", Text("鼠标侧键 X1", "Mouse side button X1"), Text("按住 X1 滚动滚轮打开或切换，松开 X1 粘贴，右键取消。", "Hold X1 and scroll to open or select; release X1 to paste; right-click to cancel.")),
                ("x2", Text("鼠标侧键 X2", "Mouse side button X2"), Text("按住 X2 滚动滚轮打开或切换，松开 X2 粘贴，右键取消。", "Hold X2 and scroll to open or select; release X2 to paste; right-click to cancel.")),
            ],
        ),
        number_field(
            "ListLimit",
            tr("非置顶条数", "Unpinned item count"),
            tr("便捷粘贴浮窗中最多显示的最近（非置顶）剪贴板条目数；置顶条目全部排在末尾且不计入此上限。", "Maximum recent unpinned items in quick paste. All pinned items appear at the end and do not count toward this limit."),
            tr("常规", "General"),
            3,
            cfg.list_limit,
            MIN_LIST_LIMIT,
            MAX_LIST_LIMIT,
            1,
        ),
        field(
            "RememberSelection",
            tr("记忆选中项", "Remember selection"),
            tr("启用后，在记忆有效期内再次打开便捷粘贴时，自动切换到上次条目的下一项；关闭后每次从第一项开始。", "Within the memory duration, reopen quick paste at the item after the last selection. When off, always start at the first item."),
            tr("选择记忆", "Selection memory"),
            "checkbox",
            4,
            json!(cfg.remember_selection),
        ),
        number_field(
            "SelectionMemorySeconds",
            tr("记忆有效期（秒）", "Memory duration (seconds)"),
            tr("启用记忆选中项时，在此秒数内再次打开时应用记忆；超时后回到第一项。", "Remember the selection for this many seconds; after that, return to the first item."),
            tr("选择记忆", "Selection memory"),
            5,
            cfg.selection_memory_seconds,
            0,
            300,
            1,
        ),
        number_field(
            "BurstGapSeconds",
            tr("爆发间隔（秒）", "Burst interval (seconds)"),
            tr("从终点往更早推断队列时，相邻两条创建时间超过该间隔则停止收入。", "Stop extending the queue backward when adjacent items were created farther apart than this interval."),
            tr("连续粘贴", "Sequential paste"),
            6,
            cfg.burst_gap_seconds,
            0,
            300,
            1,
        ),
        number_field(
            "MaxQueueLength",
            tr("队列上限", "Queue limit"),
            tr("一次连续粘贴最多包含的条目数（含终点）。", "Maximum items in one sequential paste queue, including the final item."),
            tr("连续粘贴", "Sequential paste"),
            7,
            cfg.max_queue_length,
            1,
            50,
            1,
        ),
        number_field(
            "SessionIdleSeconds",
            tr("空闲超时（秒）", "Idle timeout (seconds)"),
            tr("武装后若超过该时间没有下一次粘贴，则自动取消连续粘贴。", "Cancel sequential paste if no further paste occurs within this time after arming."),
            tr("连续粘贴", "Sequential paste"),
            8,
            cfg.session_idle_seconds,
            10,
            600,
            1,
        ),
    ]
}

fn trigger_fields(hotkeys: &GlobalHotkeys, clip: &ClipConfigDto) -> Vec<Value> {
    vec![
        {
            let mut hotkey = field(
                "HotkeyClipToggle",
                tr("显示/隐藏剪贴板窗口", "Show/hide clipboard window"),
                tr("全局快捷键，执行 clip toggle。留空表示禁用。", "Global shortcut for clip toggle. Leave blank to disable."),
                tr("快捷键", "Shortcuts"),
                "text",
                1,
                json!(hotkeys.hotkey_show_clipboard),
            );
            hotkey["formEditor"] = json!("hotkey");
            hotkey
        },
        {
            let mut hotkey = field(
                "HotkeyFavorToggle",
                tr("收藏面板", "Favorites panel"),
                tr("全局快捷键，显示或隐藏收藏面板。留空表示禁用。", "Global shortcut to show or hide favorites. Leave blank to disable."),
                tr("快捷键", "Shortcuts"),
                "text",
                2,
                json!(hotkeys.hotkey_favor_toggle),
            );
            hotkey["formEditor"] = json!("hotkey");
            hotkey
        },
        {
            let mut hotkey = field(
                "HotkeySuperMenu",
                tr("打开超级菜单", "Open Super menu"),
                tr("在任意应用中直接打开超级菜单，可切换窗口、文件、收藏、书签、文本和剪贴板。无需先打开剪贴板窗口，也不依赖 Quicker。留空表示禁用。", "Open Super menu from any app, with Windows, Files, Favorites, Bookmarks, Text and Clipboard categories. ClipHost must be running; Quicker is optional. Leave blank to disable."),
                tr("快捷键", "Shortcuts"),
                "text",
                3,
                json!(hotkeys.hotkey_super_menu),
            );
            hotkey["formEditor"] = json!("hotkey");
            hotkey
        },
        enum_field(
            "ShowWindowLocation",
            tr("打开剪贴板窗口时的位置", "Clipboard window position"),
            tr("通过 Quicker 动作、快捷键等方式打开时，窗口出现的位置；鼠标与键盘触发共用此项。", "Window position when opened by Quicker actions or shortcuts. Shared by mouse and keyboard triggers."),
            tr("触发", "Triggers"),
            4,
            &clip.show_window_location,
            SHOW_WINDOW_LOCATION_ITEMS,
        ),
        field(
            "UseWinVShortcut",
            tr("使用 Win+V 打开剪贴板", "Open clipboard with Win+V"),
            tr("通过注册表禁用系统 Win+V 剪贴板历史并接管该快捷键。修改后需重启 Windows 资源管理器后生效。", "Disable Windows clipboard history through the registry and use Win+V for ClipHost. Requires restarting Windows Explorer to take effect."),
            tr("触发", "Triggers"),
            "boolean",
            5,
            json!(clip.use_win_v_shortcut),
        ),
        field(
            "EnableVvTabTextInstruction",
            tr("文本指令 vv+Tab / vvv", "Text command vv+Tab / vvv"),
            tr("在任意输入框输入 vv 后按 Tab，或连按三次 v，自动退格删除已输入字母并显示剪贴板窗口。需 ClipHost 在运行。", "Type vv then Tab, or v three times, in any text field. ClipHost removes the typed letters and opens the clipboard window. Requires ClipHost running."),
            tr("触发", "Triggers"),
            "boolean",
            6,
            json!(clip.enable_vv_tab_text_instruction),
        ),
    ]
}

fn data_location_fields(data_root: &str) -> Vec<Value> {
    vec![field(
        "dataFolder",
        tr("数据文件夹", "Data folder"),
        tr("首次启动会检测并直接使用旧版数据文件夹，之后记住该位置，不搬动已有数据。填写程序目录以外的绝对路径，保存时确认并重启；更改位置不会自动迁移数据。留空恢复默认 BetterClipTauri。", "First launch detects and uses the existing data folder, then remembers it. Enter an absolute path outside the program folder; saving asks for confirmation and restart. Changing the folder does not move data. Leave blank to restore BetterClipTauri."),
        tr("位置", "Location"),
        "folder",
        1,
        json!(data_root),
    )]
}

pub fn apply_quick_paste(cfg: &mut QuickPasteConfig, values: &Value) {
    if let Some(v @ ("none" | "x1" | "x2")) = values.get("MouseWheelTrigger").and_then(|v| v.as_str()) {
        cfg.mouse_wheel_trigger = v.into();
    }
    if let Some(v) = values.get("ListLimit").and_then(|v| v.as_i64()) {
        cfg.list_limit = crate::quick_paste::clamp_list_limit(v as i32);
    }
    if let Some(v) = values.get("RememberSelection").and_then(|v| v.as_bool()) {
        cfg.remember_selection = v;
    }
    if let Some(v) = values.get("SelectionMemorySeconds").and_then(|v| v.as_i64()) {
        cfg.selection_memory_seconds = (v as i32).clamp(0, 300);
    }
    if let Some(v) = values.get("EnableDeleteShortcut").and_then(|v| v.as_bool()) {
        cfg.enable_delete_shortcut = v;
    }
    if let Some(v) = values.get("EnablePasteEnterShortcut").and_then(|v| v.as_bool()) {
        cfg.enable_paste_enter_shortcut = v;
    }
    if let Some(v) = values.get("EnableFifoShortcut").and_then(|v| v.as_bool()) {
        cfg.enable_fifo_shortcut = v;
    }
    if let Some(v) = values.get("BurstGapSeconds").and_then(|v| v.as_i64()) {
        cfg.burst_gap_seconds = (v as i32).clamp(0, 300);
    }
    if let Some(v) = values.get("MaxQueueLength").and_then(|v| v.as_i64()) {
        cfg.max_queue_length = crate::fifo::clamp_max_queue_length(v as usize) as i32;
    }
    if let Some(v) = values.get("SessionIdleSeconds").and_then(|v| v.as_i64()) {
        cfg.session_idle_seconds = crate::fifo::clamp_session_idle_seconds(v as u32) as i32;
    }
}

pub fn apply_hotkeys(hotkeys: &mut GlobalHotkeys, values: &Value) {
    if let Some(v) = values.get("HotkeyQuickPaste").and_then(|v| v.as_str()) {
        hotkeys.hotkey_quick_paste = crate::global_hotkey::normalize(v);
    }
    if let Some(v) = values
        .get("HotkeyClipToggle")
        .or_else(|| values.get("HotkeyShowClipboard"))
        .and_then(|v| v.as_str())
    {
        hotkeys.hotkey_show_clipboard = crate::global_hotkey::normalize(v);
    }
    if let Some(v) = values.get("HotkeyFavorToggle").and_then(|v| v.as_str()) {
        hotkeys.hotkey_favor_toggle = crate::global_hotkey::normalize(v);
    }
    if let Some(v) = values.get("HotkeySuperMenu").and_then(|v| v.as_str()) {
        hotkeys.hotkey_super_menu = crate::global_hotkey::normalize(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{ClipConfigDto, ClipFilterItemDto};
    use crate::extra_settings::ExtraSettings;

    #[test]
    fn catalog_matches_csharp_page_ids() {
        let ids: Vec<_> = catalog()["configs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ids,
            vec![
                "global",
                "trigger",
                "quickPaste",
                "clip",
                "panel",
                "user",
                "menu_global",
                "menu_file",
                "dataLocation",
                "textAction",
            ]
        );
        assert!(!ids.iter().any(|id| id == "homeGlobal" || id == "hotkeys"));
    }

    #[test]
    fn data_management_renames_the_category_without_changing_its_folder_contract() {
        let catalog = catalog();
        let page = catalog["configs"].as_array().unwrap().iter()
            .find(|page| page["id"] == "dataLocation").unwrap();
        assert!(matches!(page["label"].as_str(), Some("数据管理" | "Data management")));
        let form = build_form("dataLocation", &ClipConfigDto::default(), &QuickPasteConfig::default(),
            &GlobalHotkeys::default(), "D:/fixture-data", &ExtraSettings::default(), &[]);
        assert_eq!(form["configId"], "dataLocation");
        assert!(matches!(form["configLabel"].as_str(), Some("数据管理" | "Data management")));
        assert_eq!(form["fields"][0]["key"], "dataFolder");
        assert_eq!(form["fields"][0]["inputType"], "folder");
        assert_eq!(form["fields"][0]["value"], "D:/fixture-data");
        assert!(matches!(form["fields"][0]["label"].as_str(), Some("数据文件夹" | "Data folder")));
    }

    #[test]
    fn clip_form_uses_frontend_input_types_and_filter_items() {
        let form = build_form(
            "clip",
            &ClipConfigDto::default(),
            &QuickPasteConfig::default(),
            &GlobalHotkeys::default(),
            "D:/tmp",
            &ExtraSettings::default(),
            &[ClipFilterItemDto {
                id: "text".into(),
                display_name: "文本".into(),
                r#type: "text".into(),
                item_type: 0,
                star_type: 0,
                icon: String::new(),
            }],
        );
        assert_eq!(form["configId"], "clip");
        let fields = form["fields"].as_array().unwrap();
        assert_eq!(fields[0]["key"], "FilterItems");
        assert_eq!(fields[0]["formEditor"], "filterItems");
        assert!(fields[0]["value"].as_str().unwrap().contains("\"displayName\":\"文本\""));
        for field in fields {
            let input = field["inputType"].as_str().unwrap();
            assert!(
                matches!(input, "boolean" | "number" | "text" | "enum" | "folder"),
                "unexpected inputType {input} on {}",
                field["key"]
            );
        }
        assert!(fields.iter().any(|f| f["key"] == "IgnoreWhiteSpace" && f["inputType"] == "boolean"));
        assert!(!fields.iter().any(|f| f["group"] == "触发"));
        let keys: Vec<_> = fields
            .iter()
            .map(|f| f["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            keys,
            vec![
                "FilterItems",
                "BlackList",
                "MaxItemSize",
                "ClearFilterWhileHide",
                "DismissBehavior",
                "EnableNoFocusMode",
                "AutoScrollToTop",
                "AutoScrollToTopIdleSeconds",
                "CollectQuickerSteps",
                "AllowDuplicateCopy",
                "ShowCopyMessage",
                "CopyToastPlacement",
                "ClipChangedActionId",
                "IgnoreWhiteSpace",
                "TextProcessExpresstion",
                "DefaultPasteDelay",
                "UseShiftInsertToPasteProc",
                "PasteWithNewLine",
                "PlainTextPasteByDefault",
                "StandardPaste",
                "UsingNumberKeyToPaste",
                "ClipListExecuteMode",
                "ClipListSeparatePinnedUi",
                "ClipListImageHoverPreviewEnabled",
                "ClipListImagePreviewMaxHeightPx",
                "ClipListLayoutDensity",
                "DoNotRemindOnClear",
                "SplitLinesMaxCount",
            ]
        );
        assert!(!keys.iter().any(|k| {
            matches!(
                k.as_str(),
                "ClipMonitorOn" | "EnableEdgeHide" | "UiThemeMode" | "FontFamilyString" | "ShowWindowLocation"
            )
        }));
        let max_item = fields.iter().find(|f| f["key"] == "MaxItemSize").unwrap();
        assert_eq!(max_item["formEditor"], "numberPresets");
        assert!(max_item["max"].is_null());
        assert_eq!(max_item["presets"], json!([25.0, 100.0, 500.0, 9999.0]));
        let dismiss = fields.iter().find(|f| f["key"] == "DismissBehavior").unwrap();
        assert_eq!(dismiss["value"], "edgehide");
        assert_eq!(dismiss["options"][1], "edgehide");
        let auto_scroll = fields.iter().find(|f| f["key"] == "AutoScrollToTop").unwrap();
        assert_eq!(auto_scroll["inputType"], "boolean");
        assert_eq!(auto_scroll["value"], false);
        assert_eq!(auto_scroll["group"], dismiss["group"]);
        let idle = fields.iter().find(|f| f["key"] == "AutoScrollToTopIdleSeconds").unwrap();
        assert_eq!(idle["inputType"], "number");
        assert_eq!(idle["value"], 60);
        assert_eq!(idle["min"], 1);
        assert_eq!(idle["max"], 86_400);
        assert_eq!(idle["step"], 1);
        assert_eq!(idle["group"], dismiss["group"]);
        assert_eq!(
            fields.iter().find(|f| f["key"] == "BlackList").unwrap()["formEditor"],
            "processnames"
        );
        assert_eq!(
            fields.iter().find(|f| f["key"] == "SplitLinesMaxCount").unwrap()["min"],
            10
        );
        assert_eq!(
            fields
                .iter()
                .find(|f| f["key"] == "DefaultPasteDelay")
                .unwrap()["step"],
            50
        );
    }

    #[test]
    fn trigger_form_carries_win_v_and_favor_hotkey() {
        let form = build_form(
            "trigger",
            &ClipConfigDto::default(),
            &QuickPasteConfig::default(),
            &GlobalHotkeys::default(),
            "D:/tmp",
            &ExtraSettings::default(),
            &[],
        );
        let keys: Vec<_> = form["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            keys,
            vec![
                "HotkeyClipToggle",
                "HotkeyFavorToggle",
                "HotkeySuperMenu",
                "ShowWindowLocation",
                "UseWinVShortcut",
                "EnableVvTabTextInstruction",
            ]
        );
        assert_eq!(form["fields"][0]["value"], "Ctrl+Oem3");
        assert_eq!(form["fields"][1]["value"], "Ctrl+Shift+Oem3");
        assert_eq!(form["fields"][2]["value"], "Win+Oem3");
        assert_eq!(form["fields"][2]["formEditor"], "hotkey");
        assert_eq!(form["fields"][3]["value"], "mouserightbottom");
        assert_eq!(form["fields"][3]["options"][3], "mouserightbottom");
    }

    #[test]
    fn quick_paste_form_omits_browsable_false_shortcuts_like_csharp() {
        let form = build_form(
            "quickPaste",
            &ClipConfigDto::default(),
            &QuickPasteConfig::default(),
            &GlobalHotkeys::default(),
            "D:/tmp",
            &ExtraSettings::default(),
            &[],
        );
        let keys: Vec<_> = form["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            keys,
            vec![
                "HotkeyQuickPaste",
                "MouseWheelTrigger",
                "ListLimit",
                "RememberSelection",
                "SelectionMemorySeconds",
                "BurstGapSeconds",
                "MaxQueueLength",
                "SessionIdleSeconds",
            ]
        );
        assert_eq!(form["fields"][0]["formEditor"], "hotkey");
        assert_eq!(form["fields"][0]["group"], "快捷键");
        assert!(!keys.iter().any(|k| {
            matches!(
                k.as_str(),
                "EnableDeleteShortcut"
                    | "EnablePasteEnterShortcut"
                    | "EnableFifoShortcut"
                    | "Shortcut1Action"
                    | "Shortcut2Action"
            )
        }));
    }

    #[test]
    fn quick_paste_old_json_defaults_mouse_trigger_without_resetting_values() {
        let previous = QuickPasteConfig {
            list_limit: 12,
            remember_selection: false,
            selection_memory_seconds: 45,
            burst_gap_seconds: 8,
            ..QuickPasteConfig::default()
        };
        let mut old_json = serde_json::to_value(&previous).unwrap();
        old_json.as_object_mut().unwrap().remove("MouseWheelTrigger");
        let restored: QuickPasteConfig = serde_json::from_value(old_json.clone()).unwrap();
        assert_eq!(restored.mouse_wheel_trigger, "none");
        let mut restored_json = serde_json::to_value(restored).unwrap();
        restored_json.as_object_mut().unwrap().remove("MouseWheelTrigger");
        assert_eq!(restored_json, old_json, "adding the trigger must preserve all existing settings");
    }

    #[test]
    fn quick_paste_mouse_trigger_form_and_save_accept_only_known_values() {
        let mut config = QuickPasteConfig::default();
        for selected in ["none", "x1", "x2"] {
            apply_quick_paste(&mut config, &json!({ "MouseWheelTrigger": selected }));
            let saved = serde_json::to_value(&config).unwrap();
            assert_eq!(saved["MouseWheelTrigger"], selected);
            config = serde_json::from_value(saved).unwrap();
            let form = build_form(
                "quickPaste",
                &ClipConfigDto::default(),
                &config,
                &GlobalHotkeys::default(),
                "D:/tmp",
                &ExtraSettings::default(),
                &[],
            );
            let field = form["fields"].as_array().unwrap().iter()
                .find(|field| field["key"] == "MouseWheelTrigger").unwrap();
            assert_eq!(field["inputType"], "enum");
            assert_eq!(field["value"], selected);
            assert_eq!(field["options"], json!(["none", "x1", "x2"]));
            assert_eq!(field["selectionItems"][0]["name"], "关闭");
        }
        for invalid in [json!("x3"), json!("X1"), json!(""), Value::Null, json!(1), json!(true)] {
            apply_quick_paste(&mut config, &json!({ "MouseWheelTrigger": invalid }));
            assert_eq!(config.mouse_wheel_trigger, "x2", "invalid input must retain the saved trigger");
        }
    }

    #[test]
    fn default_hotkeys_match_csharp_global_config() {
        let hotkeys = GlobalHotkeys::default();
        assert_eq!(hotkeys.hotkey_show_clipboard, "Ctrl+Oem3");
        assert_eq!(hotkeys.hotkey_favor_toggle, "Ctrl+Shift+Oem3");
        assert_eq!(hotkeys.hotkey_quick_paste, "Alt+Oem3");
        assert_eq!(hotkeys.hotkey_super_menu, "Win+Oem3");
    }

    #[test]
    fn apply_hotkeys_normalizes_backtick_alias_and_allows_empty() {
        let mut hotkeys = GlobalHotkeys::default();
        apply_hotkeys(
            &mut hotkeys,
            &json!({
                "HotkeyClipToggle": "Ctrl+`",
                "HotkeyFavorToggle": "Ctrl+Shift+`",
                "HotkeyQuickPaste": "",
                "HotkeySuperMenu": "Win+`"
            }),
        );
        assert_eq!(hotkeys.hotkey_show_clipboard, "Ctrl+Oem3");
        assert_eq!(hotkeys.hotkey_favor_toggle, "Ctrl+Shift+Oem3");
        assert_eq!(hotkeys.hotkey_quick_paste, "");
        assert_eq!(hotkeys.hotkey_super_menu, "Win+Oem3");
        apply_hotkeys(&mut hotkeys, &json!({"HotkeySuperMenu":""}));
        assert_eq!(hotkeys.hotkey_super_menu, "");
    }

    #[test]
    fn canonical_form_config_id_matches_csharp_resolve_and_unknown() {
        assert_eq!(canonical_form_config_id("CLIP").unwrap(), "clip");
        assert_eq!(canonical_form_config_id("hotkeys").unwrap(), "trigger");
        assert_eq!(canonical_form_config_id("menu.favor").unwrap(), "menu_global");
        assert_eq!(canonical_form_config_id("MENU_FILE").unwrap(), "menu_file");
        assert_eq!(
            canonical_form_config_id("clip_filter").unwrap_err(),
            "Unknown config id: clip_filter"
        );
        assert_eq!(
            canonical_form_config_id("nope").unwrap_err(),
            "Unknown config id: nope"
        );
    }
}
