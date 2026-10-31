//! Extra settings pages aligned with C# GlobalConfig / PanelConfig / UserConfig / menu configs.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};

use crate::dto::ClipConfigDto;
use crate::i18n::{tr, LanguageMode, Text};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GlobalAppConfig {
    pub ui_theme_mode: String,
    #[serde(default = "default_ui_language", alias = "uiLanguage")]
    pub ui_language: String,
    #[serde(alias = "WindowBackground")]
    pub window_background_color: String,
    #[serde(alias = "FontForeground")]
    pub font_foreground_color: String,
    #[serde(alias = "WindowBackgroundImage")]
    pub window_background_image_path: String,
    pub window_background_blend: i32,
    pub window_background_frosted_glass: bool,
    pub launch_at_startup: bool,
    pub run_as_administrator: bool,
}

impl Default for GlobalAppConfig {
    fn default() -> Self {
        Self {
            ui_theme_mode: "Auto".into(),
            ui_language: default_ui_language(),
            window_background_color: "#FFFFFFFF".into(),
            font_foreground_color: "#FF000000".into(),
            window_background_image_path: String::new(),
            window_background_blend: 70,
            window_background_frosted_glass: true,
            launch_at_startup: true,
            run_as_administrator: false,
        }
    }
}

fn default_ui_language() -> String { "system".into() }

fn hex_white() -> String {
    "#FFFFFFFF".into()
}

fn hex_black() -> String {
    "#FF000000".into()
}

fn default_click_action() -> String {
    "OpenMenu".into()
}

fn default_big_icon_width() -> f64 {
    60.0
}

fn default_big_icon_height() -> f64 {
    60.0
}

fn default_big_icon_font_size() -> f64 {
    14.0
}

fn default_small_icon_font_size() -> f64 {
    15.0
}

fn default_small_width() -> f64 {
    100.0
}

fn default_small_min_height() -> f64 {
    20.0
}

fn default_only_icon_size() -> f64 {
    30.0
}

fn default_panel_show_window_location() -> String {
    "MouseRightBottom".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PanelSettings {
    pub show_window_location1: String,
    /// C# `PanelConfig.ShowWindowLocation2` (Browsable false; synced from loc1 after load).
    #[serde(default = "default_panel_show_window_location")]
    pub show_window_location2: String,
    pub folder_default_click_action: String,
    pub file_default_click_action: String,
    pub panel_style: String,
    #[serde(default)]
    pub default_panel_favor_id: String,
    #[serde(default = "hex_white")]
    pub window_background: String,
    #[serde(default = "hex_black")]
    pub font_color: String,
    #[serde(default = "hex_black")]
    pub default_icon_color: String,
    #[serde(default = "hex_white")]
    pub button_background: String,
    #[serde(default = "default_click_action")]
    pub default_click_action: String,
    #[serde(default = "default_big_icon_width")]
    pub big_icon_width: f64,
    #[serde(default = "default_big_icon_height")]
    pub big_icon_height: f64,
    #[serde(default = "default_big_icon_font_size")]
    pub big_icon_font_size: f64,
    #[serde(default = "default_small_icon_font_size")]
    pub small_icon_font_size: f64,
    #[serde(default = "default_small_width")]
    pub small_width: f64,
    #[serde(default = "default_small_min_height")]
    pub small_min_height: f64,
    #[serde(default = "default_only_icon_size")]
    pub only_icon_size: f64,
}

impl Default for PanelSettings {
    fn default() -> Self {
        Self {
            show_window_location1: "MouseRightBottom".into(),
            show_window_location2: "MouseRightBottom".into(),
            folder_default_click_action: "Open".into(),
            file_default_click_action: "Open".into(),
            panel_style: "PanelStyle2".into(),
            default_panel_favor_id: String::new(),
            window_background: hex_white(),
            font_color: hex_black(),
            default_icon_color: hex_black(),
            button_background: hex_white(),
            default_click_action: default_click_action(),
            big_icon_width: default_big_icon_width(),
            big_icon_height: default_big_icon_height(),
            big_icon_font_size: default_big_icon_font_size(),
            small_icon_font_size: default_small_icon_font_size(),
            small_width: default_small_width(),
            small_min_height: default_small_min_height(),
            only_icon_size: default_only_icon_size(),
        }
    }
}

/// C# `InsertPositon` (`Start=0`, `End=1`). Newtonsoft writes the integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InsertPosition {
    #[default]
    Start,
    End,
}

impl InsertPosition {
    pub fn insert_index(self) -> i32 {
        match self {
            Self::Start => 0,
            Self::End => -1,
        }
    }
}

impl Serialize for InsertPosition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i32(match self {
            Self::Start => 0,
            Self::End => 1,
        })
    }
}

impl<'de> Deserialize<'de> for InsertPosition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(match value {
            Value::Number(n) if n.as_i64() == Some(1) || n.as_u64() == Some(1) => Self::End,
            Value::String(s) if s.eq_ignore_ascii_case("end") => Self::End,
            _ => Self::Start,
        })
    }
}

/// C# `IntelliTools.Domain.Favorite.UserIcon` on `FavorConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct UserIcon {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub quicker_action_steps: String,
    #[serde(default)]
    pub favorites: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub html: String,
    #[serde(default)]
    pub quicker_action: String,
    #[serde(default)]
    pub qq_rich_text: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub cmd: String,
}

impl UserIcon {
    /// C# `FavorIcons.GetIconByType` (`TryGetPValue(type.GetName())` then Directory/Favorites folder).
    pub fn icon_by_type_name(&self, name: &str) -> String {
        let custom = match name {
            "File" => self.file.as_str(),
            "QuickerActionSteps" => self.quicker_action_steps.as_str(),
            "Favorites" => self.favorites.as_str(),
            "Link" => self.link.as_str(),
            "Directory" => self.directory.as_str(),
            "Text" => self.text.as_str(),
            "Html" => self.html.as_str(),
            "QuickerAction" => self.quicker_action.as_str(),
            "QQRichText" => self.qq_rich_text.as_str(),
            "Image" => self.image.as_str(),
            "Cmd" => self.cmd.as_str(),
            _ => "",
        };
        if !custom.trim().is_empty() {
            return custom.trim().to_string();
        }
        if name == "Directory" || name == "Favorites" {
            return "fa:Light_Folder".into();
        }
        String::new()
    }
}

fn default_favor_font_size() -> f64 {
    14.0
}

fn default_show_tooltip() -> bool {
    true
}

/// C# `FavorConfig` (`FavorConfig.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FavorSettings {
    #[serde(default = "default_favor_font_size")]
    pub font_size: f64,
    #[serde(default)]
    pub user_icon: UserIcon,
    #[serde(default = "default_show_tooltip")]
    pub show_tooltip_for_node: bool,
    #[serde(default)]
    pub return_to_home_page_while_open: bool,
    #[serde(default)]
    pub once_fold_double_enter: bool,
    #[serde(default)]
    pub donot_show_search_bar: bool,
    #[serde(default)]
    pub default_insert_pos: InsertPosition,
    #[serde(default)]
    pub default_favorite: String,
    #[serde(default)]
    pub root_children: Vec<String>,
}

impl Default for FavorSettings {
    fn default() -> Self {
        Self {
            font_size: default_favor_font_size(),
            user_icon: UserIcon::default(),
            show_tooltip_for_node: true,
            return_to_home_page_while_open: false,
            once_fold_double_enter: false,
            donot_show_search_bar: false,
            default_insert_pos: InsertPosition::Start,
            default_favorite: String::new(),
            root_children: Vec::new(),
        }
    }
}

impl FavorSettings {
    /// C# `FavorConfig.InsertIndex`: End → `-1`, else `0`.
    pub fn insert_index(&self) -> i32 {
        self.default_insert_pos.insert_index()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserSettings {
    pub save_re_open_history: bool,
    #[serde(default = "default_reopen_history_limit")]
    pub re_open_history_limit: i32,
    #[serde(default = "default_reopen_menu_limit")]
    pub re_open_menu_limit: i32,
    #[serde(default)]
    pub re_open_show_menu: bool,
    #[serde(default)]
    pub re_open_legacy_preference_migrated: bool,
}

pub const DEFAULT_REOPEN_HISTORY_LIMIT: i32 = 200;
pub const MAX_REOPEN_HISTORY_LIMIT: i32 = 2000;
pub const DEFAULT_REOPEN_MENU_LIMIT: i32 = 10;
pub const MAX_REOPEN_MENU_LIMIT: i32 = 100;

fn default_reopen_history_limit() -> i32 { DEFAULT_REOPEN_HISTORY_LIMIT }
fn default_reopen_menu_limit() -> i32 { DEFAULT_REOPEN_MENU_LIMIT }

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            save_re_open_history: true,
            re_open_history_limit: DEFAULT_REOPEN_HISTORY_LIMIT,
            re_open_menu_limit: DEFAULT_REOPEN_MENU_LIMIT,
            re_open_show_menu: false,
            re_open_legacy_preference_migrated: false,
        }
    }
}

impl UserSettings {
    pub fn normalize_limits(&mut self) {
        self.re_open_history_limit = self.re_open_history_limit.clamp(1, MAX_REOPEN_HISTORY_LIMIT);
        self.re_open_menu_limit = self.re_open_menu_limit.clamp(1, MAX_REOPEN_MENU_LIMIT.min(self.re_open_history_limit));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MenuGlobalSettings {
    pub menu_font_size: f64,
    pub menu_max_width: f64,
    pub max_single_load_count: i32,
    pub show_menu_location: String,
    pub show_setting_item: bool,
    pub show_adder_menu_item: bool,
    pub show_window_menu_item: bool,
    pub expand_window_menu: bool,
    pub show_book_marks_menu: bool,
    #[serde(default)]
    pub bookmark_browser: String,
    /// Profile directory (`Default`, `Profile 1`) within `bookmark_browser`.
    #[serde(default)]
    pub bookmark_profile: String,
    #[serde(default)]
    pub bookmark_file_path: String,
    pub grouping: bool,
    pub quick_load_submenu: bool,
    /// Small window thumbnail atop the window actions submenu (2026-10-23, off by default).
    #[serde(default)]
    pub show_window_thumbnail: bool,
}

impl Default for MenuGlobalSettings {
    fn default() -> Self {
        Self {
            menu_font_size: 12.0,
            menu_max_width: 300.0,
            max_single_load_count: 15,
            show_menu_location: "NA".into(),
            show_setting_item: true,
            show_adder_menu_item: true,
            show_window_menu_item: true,
            expand_window_menu: true,
            show_book_marks_menu: false,
            bookmark_browser: String::new(),
            bookmark_profile: String::new(),
            bookmark_file_path: String::new(),
            grouping: false,
            quick_load_submenu: false,
            show_window_thumbnail: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FileMenuSettings {
    pub prefer_internal_browser: bool,
    pub expand_opened_folder: bool,
    pub files: String,
    pub file_sort_type: String,
    pub show_type: String,
    pub show_extension: bool,
    pub file_search_parttern: String,
    pub show_hidden_files: bool,
    pub split_dir_and_file: bool,
    /// 2026-10-28 top-edge file line; a global edge trigger, so off by default.
    pub top_edge_line: bool,
    /// Milliseconds the pointer rests at the edge before the line drops.
    pub top_edge_line_delay: i32,
}

pub const TOP_EDGE_LINE_DELAY_MAX: i32 = 1000;

impl Default for FileMenuSettings {
    fn default() -> Self {
        Self {
            prefer_internal_browser: false,
            expand_opened_folder: true,
            files: String::new(),
            file_sort_type: "NA".into(),
            show_type: "dir_file".into(),
            show_extension: true,
            file_search_parttern: "*".into(),
            show_hidden_files: false,
            split_dir_and_file: true,
            top_edge_line: false,
            top_edge_line_delay: 200,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtraSettings {
    pub global: GlobalAppConfig,
    pub panel: PanelSettings,
    pub user: UserSettings,
    pub menu_global: MenuGlobalSettings,
    pub file_menu: FileMenuSettings,
    pub favor: FavorSettings,
    pub favor_menu: crate::favor_menu::FavorMenuData,
}

pub fn sync_clip_from_global(clip: &mut ClipConfigDto, global: &GlobalAppConfig) {
    clip.ui_theme_mode = global.ui_theme_mode.to_ascii_lowercase();
    clip.ui_language = LanguageMode::from_wire(&global.ui_language).wire_name().into();
    clip.window_background = Some(global.window_background_color.clone());
    clip.font_foreground = Some(global.font_foreground_color.clone());
    clip.window_background_image = if global.window_background_image_path.is_empty() {
        None
    } else {
        Some(global.window_background_image_path.clone())
    };
    clip.window_background_blend = global.window_background_blend;
    clip.window_background_frosted_glass = global.window_background_frosted_glass;
    clip.run_as_administrator = global.run_as_administrator;
}

fn f(
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
        "inputType": input_type,
        "order": order,
        "min": Value::Null,
        "max": Value::Null,
        "step": Value::Null,
        "selectionItems": Value::Null,
        "value": value,
        "messages": [],
    })
}

fn enum_f(
    key: &str,
    label: &str,
    description: &str,
    group: &str,
    order: i32,
    value: &str,
    items: &[(&str, Text)],
) -> Value {
    let mut field = f(key, label, description, group, "enum", order, json!(value.to_ascii_lowercase()));
    field["selectionItems"] = json!(items
        .iter()
        .map(|(k, name)| json!({ "key": k.to_ascii_lowercase(), "name": name.get(), "des": "" }))
        .collect::<Vec<_>>());
    field
}

fn num_f(
    key: &str,
    label: &str,
    description: &str,
    group: &str,
    order: i32,
    value: Value,
    min: f64,
    max: f64,
    step: f64,
) -> Value {
    let mut field = f(key, label, description, group, "number", order, value);
    field["min"] = json!(min);
    field["max"] = json!(max);
    field["step"] = json!(step);
    field
}

pub fn global_appearance_fields(cfg: &GlobalAppConfig) -> Vec<Value> {
    vec![
        {
            let mut field = enum_f(
                "UiThemeMode",
                tr("界面主题", "Interface theme"),
                tr("应用与宿主界面使用的主题；修改背景色或前景色后会自动切换为「自定义」。", "Theme for the app and host interface. Changing background or foreground colors switches to Custom."),
                tr("外观", "Appearance"),
                1,
                &cfg.ui_theme_mode,
                &[("auto", Text("跟随系统", "System")), ("light", Text("浅色", "Light")), ("dark", Text("深色", "Dark")), ("custom", Text("自定义", "Custom"))],
            );
            field["selectionItems"] = json!([
                {
                    "key": "auto",
                    "name": tr("跟随系统", "System"),
                    "des": tr("自动跟随系统明暗主题设置。", "Follow the system light or dark appearance.")
                },
                { "key": "light", "name": tr("浅色", "Light"), "des": tr("始终使用浅色主题。", "Always use the light theme.") },
                { "key": "dark", "name": tr("深色", "Dark"), "des": tr("始终使用深色主题。", "Always use the dark theme.") },
                {
                    "key": "custom",
                    "name": tr("自定义", "Custom"),
                    "des": tr("使用「背景色」「前景色」推导 Web 与宿主界面主题。", "Use background and foreground colors to derive the web and host theme.")
                }
            ]);
            field
        },
        {
            let mut field = f(
                "WindowBackgroundColor",
                tr("背景色", "Background color"),
                tr("WPF 颜色格式：#AARRGGBB 或 #RRGGBB（无 Alpha 时视为 FF）。", "Color format: #AARRGGBB or #RRGGBB (FF alpha if omitted)."),
                tr("外观", "Appearance"),
                "text",
                2,
                json!(cfg.window_background_color),
            );
            field["formEditor"] = json!("color");
            field
        },
        {
            let mut field = f(
                "FontForegroundColor",
                tr("前景色", "Foreground color"),
                tr("WPF 颜色格式：#AARRGGBB 或 #RRGGBB（无 Alpha 时视为 FF）。", "Color format: #AARRGGBB or #RRGGBB (FF alpha if omitted)."),
                tr("外观", "Appearance"),
                "text",
                3,
                json!(cfg.font_foreground_color),
            );
            field["formEditor"] = json!("color");
            field
        },
        {
            let mut field = f(
                "WindowBackgroundImagePath",
                tr("窗口背景图", "Window background image"),
                tr("ClipHost 剪贴板/面板 WebView 页面全屏背景图（由前端 CSS 渲染）；留空表示仅使用背景色。", "Background image for clipboard and panel web pages, rendered by CSS. Leave blank to use only the background color."),
                tr("外观", "Appearance"),
                "text",
                4,
                json!(cfg.window_background_image_path),
            );
            field["formEditor"] = json!("imagefile");
            field["stretchControl"] = json!(true);
            field
        },
        {
            let mut field = num_f(
                "WindowBackgroundBlend",
                tr("背景混合比例", "Background blend"),
                tr("有背景图时生效：0 偏主题色（背景较淡），100 偏背景图（更透出图片）。", "With a background image: 0 favors the theme color; 100 favors the image."),
                tr("外观", "Appearance"),
                5,
                json!(cfg.window_background_blend),
                0.0,
                100.0,
                1.0,
            );
            field["formEditor"] = json!("slider");
            field["stretchControl"] = json!(true);
            field
        },
        f(
            "WindowBackgroundFrostedGlass",
            tr("背景毛玻璃", "Frosted glass background"),
            tr("有背景图时生效：开启后对窗口外壳使用毛玻璃模糊；关闭则仅半透明混合，性能更好、边缘也更干净。", "With a background image, blur the window surface. Turn off for simple translucent blending, better performance and cleaner edges."),
            tr("外观", "Appearance"),
            "boolean",
            6,
            json!(cfg.window_background_frosted_glass),
        ),
        {
            let mut field = f(
                "UiLanguage",
                tr("界面语言", "Interface language"),
                tr("默认跟随 Windows 显示语言；已打开的窗口立即切换。", "Follows the Windows display language by default; open windows switch immediately."),
                tr("外观", "Appearance"),
                "enum",
                7,
                json!(LanguageMode::from_wire(&cfg.ui_language).wire_name()),
            );
            field["selectionItems"] = json!([
                { "key": "system", "name": tr("跟随系统", "System"), "des": "" },
                { "key": "zh-CN", "name": "中文", "des": "" },
                { "key": "en", "name": "English", "des": "" },
            ]);
            field
        },
    ]
}

pub fn home_global_fields(cfg: &GlobalAppConfig) -> Vec<Value> {
    vec![
        f(
            "LaunchAtStartup",
            tr("开机自启", "Launch at sign-in"),
            tr("启用后将在当前用户登录 Windows 时自动启动 BetterClip。", "Start BetterClip when the current user signs in to Windows."),
            tr("系统", "System"),
            "boolean",
            1,
            json!(cfg.launch_at_startup),
        ),
        f(
            "RunAsAdministrator",
            tr("以管理员身份运行", "Run as administrator"),
            tr("启用后冷启动将尝试管理员权限；更改后可按提示重启。DEBUG 构建默认不自动提权，需设置环境变量 CEA_CLIPHOST_ALLOW_DEBUG_ELEVATION=1 或使用 Release。", "Request administrator rights on cold start. Restart when prompted after changing. Debug builds need CEA_CLIPHOST_ALLOW_DEBUG_ELEVATION=1; Release builds support automatic elevation."),
            tr("系统", "System"),
            "boolean",
            2,
            json!(cfg.run_as_administrator),
        ),
    ]
}

pub fn panel_fields(cfg: &PanelSettings) -> Vec<Value> {
    vec![
        enum_f(
            "ShowWindowLocation1",
            tr("打开超级面板时的位置", "Super panel position"),
            tr("通过 Quicker 动作、快捷键等方式打开时，窗口出现的位置；鼠标与键盘触发共用此项。", "Window position when opened by Quicker actions or shortcuts. Shared by mouse and keyboard triggers."),
            tr("触发", "Triggers"),
            1,
            &cfg.show_window_location1,
            &[
                ("NA", Text("自动，一般是上次位置", "Auto (usually last position)")),
                ("CenterScreen", Text("屏幕中心", "Screen center")),
                ("MouseAround", Text("鼠标周围", "Near pointer")),
                ("MouseRightBottom", Text("鼠标右下", "Below-right of pointer")),
            ],
        ),
        enum_f(
            "FolderDefaultClickAction",
            tr("文件夹单击默认行为", "Folder click action"),
            "",
            tr("触发", "Triggers"),
            2,
            &cfg.folder_default_click_action,
            &[("Open", Text("打开", "Open")), ("Copy", Text("复制", "Copy")), ("Paste", Text("粘贴", "Paste"))],
        ),
        enum_f(
            "FileDefaultClickAction",
            tr("文件单击默认行为", "File click action"),
            "",
            tr("触发", "Triggers"),
            3,
            &cfg.file_default_click_action,
            &[("Open", Text("打开", "Open")), ("Copy", Text("复制", "Copy")), ("Paste", Text("粘贴", "Paste"))],
        ),
        enum_f(
            "PanelStyle",
            tr("面板样式", "Panel style"),
            "",
            tr("外观", "Appearance"),
            4,
            &cfg.panel_style,
            &[
                ("PanelStyle1", Text("网格", "Grid")),
                ("PanelStyle2", Text("列表", "List")),
                ("PanelStyle3", Text("紧凑", "Compact")),
                ("PanelStyle4", Text("仅图标", "Icons only")),
            ],
        ),
    ]
}

pub fn user_fields(cfg: &UserSettings) -> Vec<Value> {
    vec![
        f(
            "SaveReOpenHistory",
            tr("记录资源管理器历史", "Keep File Explorer history"),
            tr("关闭文件夹窗口或标签页后记录该目录；修改立即生效。关闭后停止新增，已有记录仍可在「重开」菜单中使用。", "Record folders when their File Explorer window or tab closes. Changes apply immediately. Turning this off stops new records while keeping existing history available in Reopen."),
            tr("资源管理器", "File Explorer"),
            "boolean",
            1,
            json!(cfg.save_re_open_history),
        ),
        num_f(
            "ReOpenHistoryLimit",
            tr("历史记录上限", "History limit"),
            tr("最多保存的目录数，重复目录更新到最近位置。调小后不会立即删除已有历史，下一次记录目录时清理超出部分。", "Maximum saved folders. Repeated folders move to the most recent position. Reducing the limit keeps existing history until the next folder is recorded."),
            tr("资源管理器", "File Explorer"),
            2,
            json!(cfg.re_open_history_limit),
            1.0,
            MAX_REOPEN_HISTORY_LIMIT as f64,
            1.0,
        ),
        num_f(
            "ReOpenMenuLimit",
            tr("重开菜单显示数量", "Folders in Reopen"),
            tr("「重开」菜单最多显示的最近关闭目录数，不包含当前已打开的目录，且不超过历史记录上限。", "Maximum recently closed folders shown in Reopen, excluding folders that are currently open. Cannot exceed the history limit."),
            tr("资源管理器", "File Explorer"),
            3,
            json!(cfg.re_open_menu_limit),
            1.0,
            MAX_REOPEN_MENU_LIMIT.min(cfg.re_open_history_limit.max(1)) as f64,
            1.0,
        ),
        f(
            "ReOpenShowMenu",
            tr("运行重开动作时显示快捷窗", "Show the quick window when running Reopen"),
            tr("关闭时直接重开最近目录；按住 Ctrl 仍可打开快捷窗。", "When off, reopen the latest folder directly. Holding Ctrl still opens the quick window."),
            tr("资源管理器", "File Explorer"),
            "boolean", 4, json!(cfg.re_open_show_menu),
        ),
    ]
}

pub fn menu_global_fields(cfg: &MenuGlobalSettings) -> Vec<Value> {
    vec![
        num_f("MenuFontSize", tr("字体大小", "Font size"), "", "", 1, json!(cfg.menu_font_size), 12.0, 30.0, 1.0),
        num_f("MenuMaxWidth", tr("菜单最大宽度", "Maximum menu width"), "", "", 2, json!(cfg.menu_max_width), 300.0, 500.0, 10.0),
        num_f(
            "MaxSingleLoadCount",
            tr("子菜单加载条目", "Submenu batch size"),
            tr("子菜单首次显示条数；在子菜单内向下滚动滚轮时每次追加相同条数", "Items shown when a submenu opens. Scrolling down loads another batch of the same size."),
            "",
            3,
            json!(cfg.max_single_load_count),
            5.0,
            50.0,
            5.0,
        ),
        enum_f(
            "ShowMenuLocation",
            tr("显示菜单位置", "Menu position"),
            "",
            "",
            4,
            &cfg.show_menu_location,
            &[
                ("NA", Text("自动，一般是上次位置", "Auto (usually last position)")),
                ("CenterScreen", Text("屏幕中心", "Screen center")),
                ("MouseAround", Text("鼠标周围", "Near pointer")),
                ("MouseRightBottom", Text("鼠标右下", "Below-right of pointer")),
            ],
        ),
        f("ShowSettingItem", tr("显示设置按钮", "Show Settings button"), "", tr("显示条目", "Visible items"), "boolean", 5, json!(cfg.show_setting_item)),
        f("ShowAdderMenuItem", tr("显示添加收藏菜单", "Show Add to favorites"), "", tr("显示条目", "Visible items"), "boolean", 6, json!(cfg.show_adder_menu_item)),
        f("ShowWindowMenuItem", tr("显示窗口菜单", "Show window menu"), "", tr("显示条目", "Visible items"), "boolean", 7, json!(cfg.show_window_menu_item)),
        f("ExpandWindowMenu", tr("展开窗口菜单", "Expand window menu"), "", tr("显示条目", "Visible items"), "boolean", 8, json!(cfg.expand_window_menu)),
        f("ShowBookMarksMenu", tr("显示浏览器书签", "Show browser bookmarks"), "", tr("显示条目", "Visible items"), "boolean", 9, json!(cfg.show_book_marks_menu)),
        enum_f("BookmarkBrowser", tr("书签浏览器", "Bookmark browser"), tr("无需扩展，自动读取本机书签", "Read local bookmarks without an extension"), tr("浏览器书签", "Bookmarks"), 13, &cfg.bookmark_browser,
            &[("", Text("自动（第一个发现的浏览器）", "Automatic (first found)")), ("msedge", Text("Microsoft Edge", "Microsoft Edge")), ("chrome", Text("Google Chrome", "Google Chrome")), ("chromium", Text("Chromium", "Chromium")), ("vivaldi", Text("Vivaldi", "Vivaldi")), ("brave", Text("Brave", "Brave")), ("360chromex", Text("360 极速浏览器", "360 Chrome"))]),
        f("BookmarkFilePath", tr("书签文件路径", "Bookmarks file"), tr("可选：指定 Bookmarks 文件；留空发现 Default 和 Profile *", "Optional Bookmarks file; otherwise discover Default and Profile *"), tr("浏览器书签", "Bookmarks"), "text", 14, json!(cfg.bookmark_file_path)),
        f("Grouping", tr("按进程分组", "Group by process"), "", tr("窗口菜单分组", "Window menu grouping"), "boolean", 10, json!(cfg.grouping)),
        f(
            "QuickLoadSubmenu",
            tr("快速加载子菜单", "Load submenus at once"),
            tr("开启后展开子菜单时一次性加载全部条目（条目很多时可能卡顿）", "Load every item when a submenu opens (large menus may be slow)."),
            "",
            "boolean",
            11,
            json!(cfg.quick_load_submenu),
        ),
        f(
            "ShowWindowThumbnail",
            tr("悬停显示窗口缩略图", "Show window thumbnail on hover"),
            tr("在窗口操作菜单顶部显示小尺寸缩略图", "Show a small thumbnail atop the window actions menu."),
            tr("窗口菜单分组", "Window menu grouping"),
            "boolean",
            12,
            json!(cfg.show_window_thumbnail),
        ),
    ]
}

pub fn file_menu_fields(cfg: &FileMenuSettings) -> Vec<Value> {
    vec![
        f("PreferInternalBrowser", tr("文件夹优先在自有浏览器打开", "Prefer the built-in folder browser"),
            tr("在超级菜单内连续浏览目录；普通文件仍使用系统默认应用打开。", "Browse folders continuously inside Super menu; files still open in their default applications."),
            "", "boolean", 0, json!(cfg.prefer_internal_browser)),
        f("ExpandOpenedFolder", tr("展开 打开的文件夹", "Expand open folders"), "", "", "boolean", 1, json!(cfg.expand_opened_folder)),
        {
            let mut field = f(
                "Files",
                tr("自定义目录", "Custom folders"),
                tr("出现在文件菜单根部；可插入 --- 分割线。与收藏无关。", "Appear at the root of the file menu. Use --- for separators. Independent of favorites."),
                "",
                "text",
                2,
                json!(cfg.files),
            );
            field["formEditor"] = json!("fileMenuPaths");
            field
        },
        enum_f(
            "FileSortType",
            tr("排序方式", "Sort order"),
            "",
            "",
            3,
            &cfg.file_sort_type,
            &[
                ("NA", Text("默认", "Default")),
                ("name", Text("名称", "Name")),
                ("createTime", Text("创建时间", "Created")),
                ("accessTime", Text("访问时间", "Accessed")),
                ("editTime", Text("编辑时间", "Modified")),
            ],
        ),
        enum_f(
            "ShowType",
            tr("显示方式", "Display"),
            "",
            "",
            4,
            &cfg.show_type,
            &[("dir_file", Text("文件&文件夹", "Files and folders")), ("dir", Text("只显示文件夹", "Folders only")), ("file", Text("只显示文件", "Files only"))],
        ),
        f("ShowExtension", tr("显示扩展名", "Show extensions"), "", "", "boolean", 5, json!(cfg.show_extension)),
        f("FileSearchParttern", tr("筛选文件", "File filter"), "", "", "text", 6, json!(cfg.file_search_parttern)),
        f("ShowHiddenFiles", tr("显示隐藏文件", "Show hidden files"), "", "", "boolean", 7, json!(cfg.show_hidden_files)),
        f("SplitDirAndFile", tr("文件夹和文件分开显示", "Separate folders and files"), "", "", "boolean", 8, json!(cfg.split_dir_and_file)),
        f("TopEdgeLine", tr("贴顶晾衣绳", "Top-edge file line"),
            tr("指针停在屏幕顶边时落下文件菜单的绳子；全屏程序时不触发。", "Resting the pointer at the top screen edge drops a line of file menu folders; off while a full-screen program is in front."),
            "", "boolean", 9, json!(cfg.top_edge_line)),
        num_f("TopEdgeLineDelay", tr("晾衣绳展开延迟（毫秒）", "File line delay (ms)"),
            tr("指针在顶边停留多久后落下；越小越快，但沿顶边移动时更易误触。", "How long to rest at the top edge before the line drops. Shorter is faster but triggers more easily while moving along the edge."),
            "", 10, json!(cfg.top_edge_line_delay), 0.0, TOP_EDGE_LINE_DELAY_MAX as f64, 50.0),
    ]
}

pub fn apply_global(cfg: &mut GlobalAppConfig, values: &Value) {
    let prev_bg = cfg.window_background_color.clone();
    let prev_fg = cfg.font_foreground_color.clone();
    take_str(values, "UiThemeMode").inspect(|v| cfg.ui_theme_mode = v.clone());
    if let Some(value) = take_str(values, "UiLanguage").or_else(|| take_str(values, "uiLanguage")) {
        cfg.ui_language = LanguageMode::from_wire(&value).wire_name().into();
    }
    take_str(values, "WindowBackground")
        .or_else(|| take_str(values, "WindowBackgroundColor"))
        .inspect(|v| cfg.window_background_color = v.clone());
    take_str(values, "FontForeground")
        .or_else(|| take_str(values, "FontForegroundColor"))
        .inspect(|v| cfg.font_foreground_color = v.clone());
    take_str(values, "WindowBackgroundImage")
        .or_else(|| take_str(values, "WindowBackgroundImagePath"))
        .inspect(|v| cfg.window_background_image_path = v.clone());
    if let Some(v) = take_i32(values, "WindowBackgroundBlend") {
        cfg.window_background_blend = v.clamp(0, 100);
    }
    if let Some(v) = take_bool(values, "WindowBackgroundFrostedGlass") {
        cfg.window_background_frosted_glass = v;
    }
    if let Some(v) = take_bool(values, "LaunchAtStartup") {
        cfg.launch_at_startup = v;
    }
    if let Some(v) = take_bool(values, "RunAsAdministrator") {
        cfg.run_as_administrator = v;
    }
    // C# `SettingsFormFacade.ApplyForm` global: color change forces UiThemeModes.Custom.
    if cfg.window_background_color != prev_bg || cfg.font_foreground_color != prev_fg {
        cfg.ui_theme_mode = "Custom".into();
    }
}

/// C# `PanelWindowPlacementSettings.ApplyAfterLoad`.
pub fn apply_panel_placement_after_load(cfg: &mut PanelSettings) -> bool {
    if cfg.show_window_location1 != cfg.show_window_location2 {
        cfg.show_window_location2 = cfg.show_window_location1.clone();
        true
    } else {
        false
    }
}

/// C# `PanelWindowPlacementSettings.SyncUnifiedManualLocation`.
pub fn sync_unified_panel_show_location(cfg: &mut PanelSettings, location: &str) {
    cfg.show_window_location1 = location.to_string();
    cfg.show_window_location2 = location.to_string();
}

/// The settings picker uses lowercase keys; the panel DTO uses C# enum names.
pub fn panel_item_click_action(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "copy" => "Copy",
        "paste" => "Paste",
        _ => "Open",
    }
}

pub fn apply_panel(cfg: &mut PanelSettings, values: &Value) {
    take_str(values, "ShowWindowLocation1").inspect(|v| {
        sync_unified_panel_show_location(cfg, v);
    });
    take_str(values, "FolderDefaultClickAction").inspect(|v| cfg.folder_default_click_action = panel_item_click_action(v).into());
    take_str(values, "FileDefaultClickAction").inspect(|v| cfg.file_default_click_action = panel_item_click_action(v).into());
    take_str(values, "PanelStyle").inspect(|v| cfg.panel_style = v.clone());
    take_str(values, "DefaultPanelFavorId").inspect(|v| cfg.default_panel_favor_id = v.clone());
}

pub fn apply_user(cfg: &mut UserSettings, values: &Value) {
    if let Some(v) = take_bool(values, "ReOpenShowMenu") {
        cfg.re_open_show_menu = v;
        // Once the user chooses a native preference, a late Legacy action
        // must never overwrite it during its first call after migration.
        cfg.re_open_legacy_preference_migrated = true;
    }
    if let Some(v) = take_bool(values, "SaveReOpenHistory") {
        cfg.save_re_open_history = v;
    }
    if let Some(v) = values.get("ReOpenHistoryLimit").and_then(Value::as_i64) {
        cfg.re_open_history_limit = v.clamp(1, MAX_REOPEN_HISTORY_LIMIT as i64) as i32;
    }
    if let Some(v) = values.get("ReOpenMenuLimit").and_then(Value::as_i64) {
        cfg.re_open_menu_limit = v.clamp(1, MAX_REOPEN_MENU_LIMIT as i64) as i32;
    }
    cfg.normalize_limits();
}

pub fn apply_menu_global(cfg: &mut MenuGlobalSettings, values: &Value) {
    if let Some(v) = take_str(values, "BookmarkBrowser") {
        if ["", "msedge", "chrome", "chromium", "vivaldi", "brave", "360chromex"].contains(&v.as_str()) {
            // Another browser's profile name means nothing here.
            if cfg.bookmark_browser != v { cfg.bookmark_profile.clear(); }
            cfg.bookmark_browser = v;
        }
    }
    take_str(values, "BookmarkProfile").inspect(|v| cfg.bookmark_profile = v.trim().into());
    take_str(values, "BookmarkFilePath").inspect(|v| cfg.bookmark_file_path = v.trim().into());

    if let Some(v) = values
        .get("MenuFontSize")
        .and_then(|v| v.as_f64())
    {
        cfg.menu_font_size = v.clamp(12.0, 30.0);
    }
    if let Some(v) = values.get("MenuMaxWidth").and_then(|v| v.as_f64()) {
        cfg.menu_max_width = v.clamp(300.0, 500.0);
    }
    if let Some(v) = take_i32(values, "MaxSingleLoadCount") {
        cfg.max_single_load_count = v.clamp(5, 50);
    }
    take_str(values, "ShowMenuLocation").inspect(|v| cfg.show_menu_location = v.clone());
    if let Some(v) = take_bool(values, "ShowSettingItem") {
        cfg.show_setting_item = v;
    }
    if let Some(v) = take_bool(values, "ShowAdderMenuItem") {
        cfg.show_adder_menu_item = v;
    }
    if let Some(v) = take_bool(values, "ShowWindowMenuItem") {
        cfg.show_window_menu_item = v;
    }
    if let Some(v) = take_bool(values, "ExpandWindowMenu") {
        cfg.expand_window_menu = v;
    }
    if let Some(v) = take_bool(values, "ShowBookMarksMenu") {
        cfg.show_book_marks_menu = v;
    }
    if let Some(v) = take_bool(values, "Grouping") {
        cfg.grouping = v;
    }
    if let Some(v) = take_bool(values, "QuickLoadSubmenu") {
        cfg.quick_load_submenu = v;
    }
    if let Some(v) = take_bool(values, "ShowWindowThumbnail") {
        cfg.show_window_thumbnail = v;
    }
}

pub fn apply_file_menu(cfg: &mut FileMenuSettings, values: &Value) {
    if let Some(v) = take_bool(values, "PreferInternalBrowser") {
        cfg.prefer_internal_browser = v;
    }
    if let Some(v) = take_bool(values, "ExpandOpenedFolder") {
        cfg.expand_opened_folder = v;
    }
    take_str(values, "Files").inspect(|v| cfg.files = v.clone());
    take_str(values, "FileSortType").inspect(|v| cfg.file_sort_type = v.clone());
    take_str(values, "ShowType").inspect(|v| cfg.show_type = v.clone());
    if let Some(v) = take_bool(values, "ShowExtension") {
        cfg.show_extension = v;
    }
    take_str(values, "FileSearchParttern").inspect(|v| cfg.file_search_parttern = v.clone());
    if let Some(v) = take_bool(values, "ShowHiddenFiles") {
        cfg.show_hidden_files = v;
    }
    if let Some(v) = take_bool(values, "SplitDirAndFile") {
        cfg.split_dir_and_file = v;
    }
    if let Some(v) = take_bool(values, "TopEdgeLine") {
        cfg.top_edge_line = v;
    }
    if let Some(v) = values.get("TopEdgeLineDelay").and_then(Value::as_f64) {
        cfg.top_edge_line_delay = (v.round() as i32).clamp(0, TOP_EDGE_LINE_DELAY_MAX);
    }
}

fn take_bool(values: &Value, key: &str) -> Option<bool> {
    values.get(key).and_then(|v| v.as_bool())
}

fn take_str(values: &Value, key: &str) -> Option<String> {
    values.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn take_i32(values: &Value, key: &str) -> Option<i32> {
    values.get(key).and_then(|v| v.as_i64()).map(|v| v as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_browser_preference_preserves_old_config_and_roundtrips_with_menu_fields() {
        let mut cfg: FileMenuSettings = serde_json::from_value(json!({
            "Files": "D:\\owned-folder", "ShowHiddenFiles": true
        })).unwrap();
        assert!(!cfg.prefer_internal_browser);
        apply_file_menu(&mut cfg, &json!({"PreferInternalBrowser": true}));
        assert!(cfg.prefer_internal_browser);
        assert_eq!(cfg.files, "D:\\owned-folder");
        assert!(cfg.show_hidden_files);
        let saved = serde_json::to_value(&cfg).unwrap();
        assert_eq!(saved["PreferInternalBrowser"], true);
        assert!(saved.get("prefer_internal_browser").is_none());
        let restored: FileMenuSettings = serde_json::from_value(saved).unwrap();
        assert!(restored.prefer_internal_browser);
        let field = file_menu_fields(&restored).into_iter()
            .find(|field| field["key"] == "PreferInternalBrowser").unwrap();
        assert_eq!(field["value"], true);
        apply_file_menu(&mut cfg, &json!({"ShowExtension": false}));
        assert!(cfg.prefer_internal_browser, "partial writes must preserve the preference");
        apply_file_menu(&mut cfg, &json!({"PreferInternalBrowser": false}));
        assert!(!cfg.prefer_internal_browser);
    }

    #[test]
    fn top_edge_line_defaults_off_and_roundtrips() {
        let mut cfg: FileMenuSettings = serde_json::from_value(json!({"Files": r"D:\owned-folder"})).unwrap();
        assert!(!cfg.top_edge_line, "older configs must not gain a global edge trigger");
        apply_file_menu(&mut cfg, &json!({"TopEdgeLine": true}));
        let saved = serde_json::to_value(&cfg).unwrap();
        assert_eq!(saved["TopEdgeLine"], true);
        let restored: FileMenuSettings = serde_json::from_value(saved).unwrap();
        let field = file_menu_fields(&restored).into_iter().find(|field| field["key"] == "TopEdgeLine").unwrap();
        assert_eq!(field["value"], true);
        apply_file_menu(&mut cfg, &json!({"ShowExtension": false}));
        assert!(cfg.top_edge_line, "partial writes must preserve the line");
    }

    #[test]
    fn top_edge_line_delay_defaults_quick_and_clamps() {
        let mut cfg: FileMenuSettings = serde_json::from_value(json!({"TopEdgeLine": true})).unwrap();
        assert_eq!(cfg.top_edge_line_delay, 200, "older configs take the quicker default");
        apply_file_menu(&mut cfg, &json!({"TopEdgeLineDelay": 350}));
        let restored: FileMenuSettings = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(restored.top_edge_line_delay, 350);
        let field = file_menu_fields(&restored).into_iter().find(|field| field["key"] == "TopEdgeLineDelay").unwrap();
        assert_eq!(field["value"], 350);
        apply_file_menu(&mut cfg, &json!({"TopEdgeLineDelay": -20}));
        assert_eq!(cfg.top_edge_line_delay, 0);
        apply_file_menu(&mut cfg, &json!({"TopEdgeLineDelay": 99_999}));
        assert_eq!(cfg.top_edge_line_delay, TOP_EDGE_LINE_DELAY_MAX);
    }

    #[test]
    fn explorer_history_settings_keep_old_defaults_and_bound_both_limits() {
        let mut cfg: UserSettings = serde_json::from_value(json!({"SaveReOpenHistory":false})).unwrap();
        assert!(!cfg.save_re_open_history);
        assert_eq!(cfg.re_open_history_limit, 200);
        assert_eq!(cfg.re_open_menu_limit, 10);
        apply_user(&mut cfg, &json!({"ReOpenHistoryLimit":5,"ReOpenMenuLimit":90}));
        assert_eq!(cfg.re_open_history_limit, 5);
        assert_eq!(cfg.re_open_menu_limit, 5);
        let fields = user_fields(&cfg);
        assert_eq!(fields[2]["key"], "ReOpenMenuLimit");
        assert_eq!(fields[2]["max"], 5.0);
        apply_user(&mut cfg, &json!({"ReOpenHistoryLimit":i64::MAX,"ReOpenMenuLimit":i64::MAX}));
        assert_eq!(cfg.re_open_history_limit, MAX_REOPEN_HISTORY_LIMIT);
        assert_eq!(cfg.re_open_menu_limit, MAX_REOPEN_MENU_LIMIT);
        apply_user(&mut cfg, &json!({"ReOpenHistoryLimit":-20,"ReOpenMenuLimit":0}));
        assert_eq!(cfg.re_open_history_limit, 1);
        assert_eq!(cfg.re_open_menu_limit, 1);
    }

    #[test]
    fn show_window_location_options_match_csharp_enum() {
        let menu = MenuGlobalSettings::default();
        assert_eq!(menu.show_menu_location, "NA");
        let loc = menu_global_fields(&menu)
            .into_iter()
            .find(|f| f["key"] == "ShowMenuLocation")
            .expect("ShowMenuLocation");
        assert_eq!(loc["value"], "na");
        let keys: Vec<&str> = loc["selectionItems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            ["na", "centerscreen", "mousearound", "mouserightbottom"]
        );

        let panel = PanelSettings::default();
        assert_eq!(panel.show_window_location1, "MouseRightBottom");
        assert_eq!(panel.show_window_location2, "MouseRightBottom");
        let p = panel_fields(&panel)
            .into_iter()
            .find(|f| f["key"] == "ShowWindowLocation1")
            .expect("ShowWindowLocation1");
        assert_eq!(p["value"], "mouserightbottom");
        let pkeys: Vec<&str> = p["selectionItems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"].as_str().unwrap())
            .collect();
        assert_eq!(pkeys, keys);
    }

    #[test]
    fn panel_placement_after_load_syncs_loc2_like_csharp() {
        let mut panel = PanelSettings::default();
        panel.show_window_location1 = "CenterScreen".into();
        panel.show_window_location2 = "MouseAround".into();
        assert!(apply_panel_placement_after_load(&mut panel));
        assert_eq!(panel.show_window_location2, "CenterScreen");
        assert!(!apply_panel_placement_after_load(&mut panel));
        apply_panel(
            &mut panel,
            &json!({ "ShowWindowLocation1": "MouseAround" }),
        );
        assert_eq!(panel.show_window_location1, "MouseAround");
        assert_eq!(panel.show_window_location2, "MouseAround");
    }

    #[test]
    fn global_appearance_form_keys_match_csharp_property_names() {
        let fields = global_appearance_fields(&GlobalAppConfig::default());
        let keys: Vec<_> = fields
            .iter()
            .map(|f| f["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "UiThemeMode",
                "WindowBackgroundColor",
                "FontForegroundColor",
                "WindowBackgroundImagePath",
                "WindowBackgroundBlend",
                "WindowBackgroundFrostedGlass",
                "UiLanguage",
            ]
        );
        assert_eq!(fields[1]["formEditor"], "color");
        assert_eq!(fields[3]["formEditor"], "imagefile");
        assert_eq!(fields[3]["stretchControl"], true);
        assert_eq!(fields[4]["formEditor"], "slider");
        assert_eq!(fields[4]["stretchControl"], true);
        assert_eq!(fields[0]["value"], "auto");
        assert_eq!(fields[0]["selectionItems"][3]["des"], "使用「背景色」「前景色」推导 Web 与宿主界面主题。");
    }

    #[test]
    fn language_changes_preserve_unrelated_global_preferences() {
        let mut cfg = GlobalAppConfig::default();
        cfg.ui_theme_mode = "Dark".into();
        cfg.window_background_image_path = "用户自己的路径.png".into();
        for (input, expected) in [("en", "en"), ("zh-cn", "zh-CN"), ("unknown", "system")] {
            apply_global(&mut cfg, &json!({ "UiLanguage": input }));
            assert_eq!(cfg.ui_language, expected);
            assert_eq!(cfg.ui_theme_mode, "Dark");
            assert_eq!(cfg.window_background_image_path, "用户自己的路径.png");
        }
        let mut clip = ClipConfigDto::default();
        sync_clip_from_global(&mut clip, &cfg);
        assert_eq!(clip.ui_language, "system");
    }

    #[test]
    fn apply_global_color_change_switches_theme_to_custom_like_csharp() {
        let mut cfg = GlobalAppConfig::default();
        assert_eq!(cfg.ui_theme_mode, "Auto");
        apply_global(
            &mut cfg,
            &json!({ "WindowBackgroundColor": "#FF112233" }),
        );
        assert_eq!(cfg.window_background_color, "#FF112233");
        assert_eq!(cfg.ui_theme_mode, "Custom");
    }

    #[test]
    fn favor_config_insert_pos_and_user_icon_match_csharp() {
        let empty: FavorSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.font_size, 14.0);
        assert!(empty.show_tooltip_for_node);
        assert_eq!(empty.insert_index(), 0);
        assert_eq!(empty.user_icon.icon_by_type_name("Text"), "");
        assert_eq!(
            empty.user_icon.icon_by_type_name("Favorites"),
            "fa:Light_Folder"
        );

        let end: FavorSettings = serde_json::from_str(
            r#"{"DefaultInsertPos":1,"UserIcon":{"Text":"fa:Solid_Font","Favorites":"fa:Solid_Star"}}"#,
        )
        .unwrap();
        assert_eq!(end.insert_index(), -1);
        assert_eq!(end.user_icon.icon_by_type_name("Text"), "fa:Solid_Font");
        assert_eq!(end.user_icon.icon_by_type_name("Favorites"), "fa:Solid_Star");

        let named: FavorSettings = serde_json::from_str(r#"{"DefaultInsertPos":"End"}"#).unwrap();
        assert_eq!(named.default_insert_pos, InsertPosition::End);
    }
}
