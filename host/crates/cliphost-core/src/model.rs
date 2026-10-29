use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// C# DTO `kind` is the concrete clip type name (`TextClipItem`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClipKind {
    TextClipItem,
    HtmlClipItem,
    ImageClipItem,
    FileClipItem,
    QQRichTextItem,
    LinkClipItem,
    CmdClipItem,
    StepClipItem,
    ActionClipItem,
    FavoriteClipItem,
}

impl ClipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TextClipItem => "TextClipItem",
            Self::HtmlClipItem => "HtmlClipItem",
            Self::ImageClipItem => "ImageClipItem",
            Self::FileClipItem => "FileClipItem",
            Self::QQRichTextItem => "QQRichTextItem",
            Self::LinkClipItem => "LinkClipItem",
            Self::CmdClipItem => "CmdClipItem",
            Self::StepClipItem => "StepClipItem",
            Self::ActionClipItem => "ActionClipItem",
            Self::FavoriteClipItem => "FavoriteClipItem",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "HtmlClipItem" | "html" | "richtext" | "rich" => Self::HtmlClipItem,
            "ImageClipItem" | "image" => Self::ImageClipItem,
            "FileClipItem" | "file" => Self::FileClipItem,
            "QQRichTextItem" => Self::QQRichTextItem,
            "LinkClipItem" => Self::LinkClipItem,
            "CmdClipItem" => Self::CmdClipItem,
            "StepClipItem" | "step" | "other" => Self::StepClipItem,
            "ActionClipItem" => Self::ActionClipItem,
            "FavoriteClipItem" => Self::FavoriteClipItem,
            _ => Self::TextClipItem,
        }
    }

    pub fn is_text_like(self) -> bool {
        matches!(
            self,
            Self::TextClipItem | Self::HtmlClipItem | Self::QQRichTextItem | Self::LinkClipItem
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipRecord {
    /// Opaque original-document revision, internal to in-place storage.
    #[serde(default, skip_serializing)]
    pub storage_revision: Option<String>,
    pub id: String,
    pub kind: ClipKind,
    pub title: Option<String>,
    pub body_text: Option<String>,
    pub html_data: Option<String>,
    pub html_source_url: Option<String>,
    pub qq_rich_text: Option<String>,
    pub file_paths: Vec<String>,
    pub image_path: Option<String>,
    pub is_top: bool,
    /// C# `ClipItem.SubIndex` — burst copies sharing `CreateTime` stay FIFO-stable.
    #[serde(default)]
    pub sub_index: i32,
    pub create_time: DateTime<Utc>,
    pub update_time: DateTime<Utc>,
    pub source_process_name: String,
    pub content_hash: String,
    pub last_access: Option<DateTime<Utc>>,
    pub usage_count: i32,
    /// C# `ActionClipItem.ExecuteParam` (empty for other kinds).
    #[serde(default)]
    pub execute_param: String,
    /// C# `ClipItem.Description` (also `ActionClipItem.ActionItem.Description` for FilterBy).
    #[serde(default)]
    pub description: String,
    /// C# `ImageClipItem.PerceptualHash` (8×8 average hash). Empty for other kinds.
    #[serde(default)]
    pub perceptual_hash: String,
}

impl ClipRecord {
    pub fn new_text(text: impl Into<String>, source_process: impl Into<String>) -> Self {
        let body = text.into();
        let now = Utc::now();
        let id = uuid::Uuid::new_v4().simple().to_string();
        let content_hash = hash_text(&body);
        Self {
            storage_revision: None,
            id,
            kind: ClipKind::TextClipItem,
            title: None,
            body_text: Some(body),
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            file_paths: Vec::new(),
            image_path: None,
            is_top: false,
            sub_index: 0,
            create_time: now,
            update_time: now,
            source_process_name: source_process.into(),
            content_hash,
            last_access: None,
            usage_count: 0,
            execute_param: String::new(),
            description: String::new(),
            perceptual_hash: String::new(),
        }
    }

    /// C# `ClipItemBase.GetSummary` (used as Quicker `clipChangedAction` input).
    pub fn csharp_summary(&self) -> String {
        match self.kind {
            ClipKind::TextClipItem | ClipKind::HtmlClipItem => {
                self.body_text.clone().unwrap_or_default()
            }
            ClipKind::FileClipItem => self.file_paths.join(","),
            ClipKind::ImageClipItem => self.image_path.clone().unwrap_or_default(),
            ClipKind::LinkClipItem => {
                let title = self.title.clone().unwrap_or_default();
                let url = self.body_text.clone().unwrap_or_default();
                format!("{title}\r\n{url}")
            }
            ClipKind::ActionClipItem => {
                // C# `ActionClipItem.GetSummary` uses `AppendLine` twice (trailing `\r\n`).
                let title = self.action_title();
                format!("动作：{title}\r\n参数：{}\r\n", self.execute_param)
            }
            // C# `StepClipItem.GetSummary` → `StepClipboardPresentation.BuildSummary`.
            ClipKind::StepClipItem => {
                crate::quicker_steps::build_summary(self.body_text.as_deref().unwrap_or(""))
            }
            // C# `CmdClipItem` has no GetSummary override → `【命令】`.
            ClipKind::CmdClipItem => "【命令】".into(),
            ClipKind::QQRichTextItem => self.body_text.clone().unwrap_or_default(),
            ClipKind::FavoriteClipItem => "【收藏夹】".into(),
        }
    }

    /// C# `ClipItem.VisualTitle`: stored Title if not empty, else `GetVisualName`.
    pub fn visual_title(&self) -> String {
        if let Some(title) = self.title.as_ref().filter(|s| !s.is_empty()) {
            return title.clone();
        }
        self.csharp_visual_name()
    }

    /// C# `ClipItemBase.GetVisualName`.
    pub fn csharp_visual_name(&self) -> String {
        match self.kind {
            ClipKind::FileClipItem => self
                .file_paths
                .first()
                .and_then(|p| std::path::Path::new(p).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            ClipKind::ImageClipItem => self.image_path.clone().unwrap_or_default(),
            ClipKind::ActionClipItem => {
                let mut name = self.action_title();
                if !self.execute_param.is_empty() {
                    let first = self
                        .execute_param
                        .split('\n')
                        .next()
                        .unwrap_or("")
                        .trim_end_matches('\r');
                    name.push(':');
                    name.push_str(first);
                }
                name
            }
            ClipKind::CmdClipItem => "【命令】".into(),
            ClipKind::LinkClipItem => {
                let title = self.title.as_deref().unwrap_or("");
                if title.is_empty() {
                    self.csharp_summary()
                } else {
                    title.to_string()
                }
            }
            _ => self.csharp_summary(),
        }
    }

    pub fn preview(&self, max_chars: usize) -> String {
        if let Some(title) = self.title.as_deref().filter(|s| !s.trim().is_empty()) {
            return truncate(title, max_chars);
        }
        match self.kind {
            ClipKind::ActionClipItem => truncate(&self.csharp_visual_name(), max_chars),
            ClipKind::ImageClipItem => self
                .image_path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("[image]")
                .to_string(),
            ClipKind::FileClipItem => {
                if self.file_paths.is_empty() {
                    "[files]".into()
                } else if self.file_paths.len() == 1 {
                    self.file_paths[0].clone()
                } else {
                    format!("{} (+{})", self.file_paths[0], self.file_paths.len() - 1)
                }
            }
            _ => truncate(self.body_text.as_deref().unwrap_or(""), max_chars),
        }
    }

    pub fn text_length(&self) -> Option<i32> {
        // C# `TryGetTextLength`: `item is TextClipItem` (Html/QQ inherit); empty → 0; Unicode scalars.
        if !matches!(
            self.kind,
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
        ) {
            return None;
        }
        Some(self.body_text.as_deref().unwrap_or("").chars().count() as i32)
    }

    /// C# `ClipItemDistinctHelper.GetDistinctKey` — first occurrence wins when de-duping.
    pub fn distinct_key(&self) -> String {
        match self.kind {
            ClipKind::TextClipItem => {
                format!("text:{}", normalize_distinct_text(self.body_text.as_deref()))
            }
            ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                if !has_rich_payload(self) {
                    return format!("rich:{}:unknown:{}", self.kind.as_str(), self.id);
                }
                let payload = serde_json::to_string(&(normalize_distinct_text(self.body_text.as_deref()),
                    &self.html_data, &self.html_source_url, &self.qq_rich_text))
                    .expect("Rich text string fields serialize");
                format!("rich:{}:{}", self.kind.as_str(), hash_text(&payload))
            }
            ClipKind::FileClipItem => {
                let mut paths: Vec<String> = self
                    .file_paths
                    .iter()
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| normalize_distinct_path(p))
                    .collect();
                paths.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
                format!("file:FileClipItem:{}", paths.join("|"))
            }
            ClipKind::ImageClipItem => {
                if !self.perceptual_hash.trim().is_empty() {
                    format!("image:phash:{}", self.perceptual_hash.trim())
                } else if !self.content_hash.is_empty() {
                    format!("image:hash:{}", self.content_hash.trim())
                } else {
                    format!(
                        "image:path:{}",
                        normalize_distinct_path(self.image_path.as_deref().unwrap_or(""))
                    )
                }
            }
            ClipKind::LinkClipItem => format!(
                "link:{}|{}",
                self.body_text.as_deref().unwrap_or("").trim(),
                normalize_distinct_text(self.title.as_deref())
            ),
            ClipKind::CmdClipItem => format!("cmd:{}", normalize_distinct_text(self.body_text.as_deref())),
            ClipKind::ActionClipItem => match crate::quicker_action::action_id_from_payload(self.body_text.as_deref().unwrap_or("")) {
                Ok(id) => format!("action:{id}"),
                Err(_) => format!("strict:{}:{}", self.kind.as_str(), self.content_hash),
            },
            _ => format!("strict:{}:{}", self.kind.as_str(), self.content_hash),
        }
    }

    /// C# `ClipItemDistinctHelper.LooksEqual` (Val_Equals, then normalized distinct keys).
    pub fn looks_equal(&self, other: &Self) -> bool {
        if self.id == other.id {
            return true;
        }
        // HTML and QQ are different persistent payloads, not aliases for their
        // plain preview text. Match the shared C# capture comparison policy.
        if is_text_clip_item(self.kind) && is_text_clip_item(other.kind) {
            if self.kind != other.kind { return false; }
            if normalize_distinct_text(self.body_text.as_deref()) != normalize_distinct_text(other.body_text.as_deref()) {
                return false;
            }
            return self.kind == ClipKind::TextClipItem || (has_rich_payload(self) && has_rich_payload(other)
                && self.html_data == other.html_data && self.html_source_url == other.html_source_url
                && self.qq_rich_text == other.qq_rich_text);
        }
        if val_equals(self, other) {
            return true;
        }
        if self.kind == ClipKind::FileClipItem && other.kind == ClipKind::FileClipItem {
            return self.distinct_key().eq_ignore_ascii_case(&other.distinct_key());
        }
        if self.kind == ClipKind::ImageClipItem && other.kind == ClipKind::ImageClipItem {
            return looks_like_same_image(self, other);
        }
        if self.kind == ClipKind::LinkClipItem && other.kind == ClipKind::LinkClipItem {
            return self.distinct_key() == other.distinct_key();
        }
        if self.kind == ClipKind::CmdClipItem && other.kind == ClipKind::CmdClipItem {
            return self.distinct_key() == other.distinct_key();
        }
        false
    }

    fn action_title(&self) -> String {
        crate::quicker_action::ActionReference::parse(self.body_text.as_deref().unwrap_or(""))
            .ok().and_then(|action| action.title).or_else(|| self.title.clone()).unwrap_or_default()
    }
}

/// C# `item is TextClipItem` (Html / QQ inherit TextClipItem).
fn is_text_clip_item(kind: ClipKind) -> bool {
    matches!(
        kind,
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
    )
}

fn has_rich_payload(item: &ClipRecord) -> bool {
    item.html_data.as_deref().is_some_and(|html| !html.trim().is_empty())
        || (item.kind == ClipKind::QQRichTextItem && item.qq_rich_text.as_deref().is_some_and(|rich| !rich.trim().is_empty()))
}

fn val_equals(a: &ClipRecord, b: &ClipRecord) -> bool {
    match (a.kind, b.kind) {
        (ClipKind::ImageClipItem, ClipKind::ImageClipItem) => looks_like_same_image(a, b),
        (ClipKind::FileClipItem, ClipKind::FileClipItem) => a.file_paths == b.file_paths,
        (ClipKind::LinkClipItem, ClipKind::LinkClipItem) => {
            a.body_text == b.body_text && a.title == b.title
        }
        (ClipKind::CmdClipItem, ClipKind::CmdClipItem) => a.body_text == b.body_text,
        (ClipKind::ActionClipItem, ClipKind::ActionClipItem) => {
            match (
                crate::quicker_action::action_id_from_payload(a.body_text.as_deref().unwrap_or("")),
                crate::quicker_action::action_id_from_payload(b.body_text.as_deref().unwrap_or("")),
            ) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        }
        _ => false,
    }
}

/// C# `ImageClipItem.LooksLikeSameImage` — SHA-256 ImageHash (ignore-case) or PerceptualHash.
fn looks_like_same_image(a: &ClipRecord, b: &ClipRecord) -> bool {
    if !a.content_hash.is_empty()
        && !b.content_hash.is_empty()
        && a.content_hash.eq_ignore_ascii_case(&b.content_hash)
    {
        return true;
    }
    !a.perceptual_hash.trim().is_empty()
        && !b.perceptual_hash.trim().is_empty()
        && a.perceptual_hash == b.perceptual_hash
}

fn normalize_distinct_text(value: Option<&str>) -> String {
    value.unwrap_or("").replace("\r\n", "\n").replace('\r', "\n").trim().to_string()
}

fn normalize_distinct_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    std::path::Path::new(trimmed)
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| trimmed.replace('/', "\\").trim_end_matches('\\').to_string())
}

pub fn hash_text(text: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"text:");
    hasher.update(text.as_bytes());
    hasher.finalize().to_hex().to_string()
}

pub fn hash_files(paths: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"files:");
    for path in paths {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}

/// C# `ImageComparer.GetFileHash`: SHA-256 lowercase hex of the file bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

/// C# `ImageComparer.GetPerceptualHash`: 8×8 luma vs average, `'1'`/`'0'`.
pub fn perceptual_hash_png(png: &[u8]) -> Option<String> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    if img.width() == 0 || img.height() == 0 {
        return None;
    }
    let small = image::imageops::resize(&img, 8, 8, image::imageops::FilterType::Triangle);
    let mut gray = [0u8; 64];
    for (i, pixel) in small.pixels().enumerate() {
        if i >= 64 {
            break;
        }
        gray[i] = ((u32::from(pixel[0]) * 299 + u32::from(pixel[1]) * 587 + u32::from(pixel[2]) * 114) / 1000)
            as u8;
    }
    let sum: u32 = gray.iter().map(|&x| u32::from(x)).sum();
    let average = (sum / 64) as u8;
    Some(gray.iter().map(|&x| if x > average { '1' } else { '0' }).collect())
}

fn truncate(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(max_chars).collect::<String>())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavorRecord {
    #[serde(default, skip_serializing)]
    pub storage_revision: Option<String>,
    pub id: String,
    pub parent_id: Option<String>,
    pub is_folder: bool,
    pub name: String,
    pub kind: ClipKind,
    pub body_text: Option<String>,
    pub file_paths: Vec<String>,
    pub image_path: Option<String>,
    pub children: Vec<String>,
    pub create_time: DateTime<Utc>,
    /// C# `FavorItem.UpdateTime`; search orders by this then `CreateTime`.
    #[serde(default = "utc_now")]
    pub update_time: DateTime<Utc>,
    pub source_clip_id: Option<String>,
    /// C# `FavorItem.Item` keeps the full clip payload (HTML / QQ envelope).
    #[serde(default)]
    pub html_data: Option<String>,
    #[serde(default)]
    pub html_source_url: Option<String>,
    #[serde(default)]
    pub qq_rich_text: Option<String>,
    /// C# `FavorItem.SourceProcessName`.
    #[serde(default)]
    pub source_process_name: String,
    /// C# `FavorItem.SubIndex` (search `ThenBy SubIndex`; FavoriteById does not copy it).
    #[serde(default)]
    pub sub_index: i32,
    /// C# `FavorItem.Icon` (HostGetIcon first; collect-commit may set it).
    #[serde(default)]
    pub icon: String,
    /// C# `ActionClipItem.ExecuteParam`.
    #[serde(default)]
    pub execute_param: String,
}

impl FavorRecord {
    pub fn from_clip(clip: &ClipRecord, parent_id: Option<String>) -> Self {
        Self {
            storage_revision: None,
            id: uuid::Uuid::new_v4().simple().to_string(),
            parent_id,
            is_folder: false,
            name: clip
                .title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| clip.preview(80)),
            kind: clip.kind,
            body_text: clip.body_text.clone(),
            file_paths: clip.file_paths.clone(),
            image_path: clip.image_path.clone(),
            children: Vec::new(),
            create_time: Utc::now(),
            update_time: Utc::now(),
            source_clip_id: Some(clip.id.clone()),
            html_data: clip.html_data.clone(),
            html_source_url: clip.html_source_url.clone(),
            qq_rich_text: clip.qq_rich_text.clone(),
            source_process_name: clip.source_process_name.clone(),
            sub_index: 0,
            icon: String::new(),
            execute_param: clip.execute_param.clone(),
        }
    }

    /// C# `FavorItem.VisualTitle`.
    pub fn visual_title(&self) -> String {
        let name = self.name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
        match self.kind {
            ClipKind::ActionClipItem => {
                let mut name = self.action_title();
                if !self.execute_param.is_empty() {
                    name.push(':');
                    name.push_str(self.execute_param.split('\n').next().unwrap_or("").trim_end_matches('\r'));
                }
                name
            }
            ClipKind::FileClipItem => self
                .file_paths
                .first()
                .and_then(|p| std::path::Path::new(p).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ClipKind::ImageClipItem => self.image_path.clone().unwrap_or_default(),
            _ => self.summary(),
        }
    }

    /// C# `FavorItem.Summary` / `ClipItemBase.GetSummary`.
    pub fn summary(&self) -> String {
        if self.is_folder {
            return "【收藏夹】".into();
        }
        match self.kind {
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                self.body_text.clone().unwrap_or_default()
            }
            ClipKind::StepClipItem => {
                crate::quicker_steps::build_summary(self.body_text.as_deref().unwrap_or(""))
            }
            ClipKind::CmdClipItem => "【命令】".into(),
            ClipKind::FileClipItem => self.file_paths.join(","),
            ClipKind::ImageClipItem => self.image_path.clone().unwrap_or_default(),
            ClipKind::LinkClipItem => {
                format!("{}\r\n{}", self.name, self.body_text.as_deref().unwrap_or(""))
            }
            ClipKind::ActionClipItem => {
                // C# `ActionClipItem.GetSummary` uses `AppendLine` twice (trailing `\r\n`).
                format!("动作：{}\r\n参数：{}\r\n", self.action_title(), self.execute_param)
            }
            ClipKind::FavoriteClipItem => "【收藏夹】".into(),
        }
    }

    /// C# `ClipItemBase.GetIcon` (File `icon:`+path, Image `FullPath` when `File.Exists`).
    pub fn item_get_icon(&self) -> String {
        match self.kind {
            ClipKind::ActionClipItem => crate::quicker_action::ActionReference::parse(self.body_text.as_deref().unwrap_or(""))
                .ok().and_then(|action| action.icon).unwrap_or_default(),
            ClipKind::FileClipItem => self
                .file_paths
                .first()
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(|p| format!("icon:{p}"))
                .unwrap_or_default(),
            ClipKind::ImageClipItem => self
                .image_path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty() && std::path::Path::new(p).is_file())
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        }
    }

    fn action_title(&self) -> String {
        crate::quicker_action::ActionReference::parse(self.body_text.as_deref().unwrap_or(""))
            .ok().and_then(|action| action.title).unwrap_or_else(|| self.name.clone())
    }

    /// C# `FavorItemType` name used by `UserIcon.TryGetPValue(type.GetName())`.
    pub fn obj_type_name(&self) -> &'static str {
        if self.is_folder || self.kind == ClipKind::FavoriteClipItem {
            return "Favorites";
        }
        match self.kind {
            ClipKind::TextClipItem => "Text",
            ClipKind::FileClipItem => {
                if self
                    .file_paths
                    .first()
                    .map(|p| std::path::Path::new(p).is_dir())
                    .unwrap_or(false)
                {
                    "Directory"
                } else {
                    "File"
                }
            }
            ClipKind::ImageClipItem => "Image",
            ClipKind::HtmlClipItem => "Html",
            ClipKind::QQRichTextItem => "QQRichText",
            ClipKind::LinkClipItem => "Link",
            ClipKind::CmdClipItem => "Cmd",
            ClipKind::StepClipItem => "QuickerActionSteps",
            ClipKind::ActionClipItem => "QuickerAction",
            ClipKind::FavoriteClipItem => "Favorites",
        }
    }

    /// C# `FavorMenuItemWrapper.HostGetIcon` / `FavorIconResolver` with empty `UserIcon`.
    pub fn host_icon(&self) -> String {
        self.host_icon_with(&crate::extra_settings::UserIcon::default())
    }

    /// C# `HostGetIcon`: `FavorItem.Icon` then `Item.GetIcon()` then `FavorIcons.GetIconByType`.
    pub fn host_icon_with(&self, user_icon: &crate::extra_settings::UserIcon) -> String {
        if !self.icon.trim().is_empty() {
            return self.icon.trim().to_string();
        }
        let item_icon = self.item_get_icon();
        if !item_icon.trim().is_empty() {
            return item_icon.trim().to_string();
        }
        user_icon.icon_by_type_name(self.obj_type_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DismissBehavior {
    Hide,
    Fold,
    #[default]
    EdgeHide,
}

impl DismissBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hide => "Hide",
            Self::Fold => "Fold",
            Self::EdgeHide => "EdgeHide",
        }
    }

    pub fn parse(value: &str) -> Self {
        Self::try_parse(value).unwrap_or(Self::EdgeHide)
    }

    /// C# `Enum.TryParse<ClipHostDismissBehavior>(..., ignoreCase: true)`.
    pub fn try_parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "hide" => Some(Self::Hide),
            "fold" => Some(Self::Fold),
            "edgehide" => Some(Self::EdgeHide),
            _ => None,
        }
    }
}
