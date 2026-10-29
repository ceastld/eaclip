//! C# `QuickerActionRpcForwarder` URI fallback (`quicker:runaction:{id}?{params}`).
//! ClipHost starts without Quicker; ShellExecute of the protocol handler is optional, same as C#.

type BackgroundAction = Box<dyn FnOnce() + Send + 'static>;

/// A slow protocol handler must not hold the clipboard monitor. Keep ordering and
/// bound pending work; the worker owns no host reference while the queue is idle.
#[derive(Default)]
pub(crate) struct ClipboardActionQueue {
    sender: parking_lot::Mutex<Option<std::sync::mpsc::SyncSender<BackgroundAction>>>,
}

impl ClipboardActionQueue {
    pub(crate) fn submit(&self, action: impl FnOnce() + Send + 'static) -> Result<(), String> {
        let mut sender = self.sender.lock();
        if sender.is_none() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<BackgroundAction>(64);
            std::thread::Builder::new()
                .name("clipboard-actions".into())
                .spawn(move || {
                    for action in rx {
                        action();
                    }
                })
                .map_err(|error| format!("无法启动剪贴板动作线程：{error}"))?;
            *sender = Some(tx);
        }
        match sender.as_ref().unwrap().try_send(Box::new(action)) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Err("剪贴板动作队列已满。".into()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                *sender = None;
                Err("剪贴板动作线程已停止。".into())
            }
        }
    }
}

/// Practical ShellExecute URI size; keep action id intact and trim oversized clipboard summaries.
pub const MAX_URI_PARAM_LENGTH: usize = 1800;

/// Stored action payloads may be a plain ID or Quicker's SimpleActionItem JSON.
/// Keep metadata separate from the favorite's user-defined name, and never use
/// malformed JSON as the identifier passed to a protocol handler.
#[derive(Debug, Clone, Default)]
pub(crate) struct ActionReference {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub template_id: Option<String>,
    pub shared_action_id: Option<String>,
}

impl ActionReference {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if !raw.starts_with(['{', '[', '"']) {
            return Ok(Self {
                id: raw.into(),
                ..Self::default()
            });
        }
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|_| "Quicker 动作数据格式无效。".to_string())?;
        let mut object = value
            .as_object()
            .ok_or_else(|| "Quicker 动作数据必须是对象。".to_string())?;
        fn field<'a>(
            object: &'a serde_json::Map<String, serde_json::Value>,
            name: &str,
        ) -> Option<&'a serde_json::Value> {
            object
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
        }
        // JSON representations of ActionItemDragObject / ActionClipItem contain
        // the SimpleActionItem under Action / ActionItem. Do not recurse blindly.
        if field(object, "Id").is_none() {
            if let Some(value) = field(object, "Action").or_else(|| field(object, "ActionItem")) {
                object = value
                    .as_object()
                    .ok_or_else(|| "Quicker 动作数据必须是对象。".to_string())?;
            }
        }
        let text = |name: &str| field(object, name).and_then(|v| v.as_str().map(str::to_string));
        let nested_text = |parent: &str, name: &str| {
            field(object, parent)
                .and_then(|v| v.as_object())
                .and_then(|nested| field(nested, name))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        Ok(Self {
            id: text("Id").unwrap_or_default().trim().into(),
            // Quicker V2 ActionItem2 moved display and source data into nested records.
            title: text("Title")
                .or_else(|| text("Name"))
                .or_else(|| nested_text("Presentation", "Title")),
            description: text("Description").or_else(|| nested_text("Presentation", "Description")),
            icon: text("Icon").or_else(|| nested_text("Presentation", "Icon")),
            template_id: text("TemplateId").or_else(|| nested_text("SourceActionInfo", "Id")),
            shared_action_id: text("SharedActionId")
                .or_else(|| nested_text("SharedActionInfo", "SharedActionId")),
        })
    }

    pub(crate) fn require_id(&self) -> Result<&str, String> {
        if self.id.is_empty() {
            Err("未设置 Quicker 动作 ID。".into())
        } else if self.id.chars().any(char::is_control) {
            Err("Quicker 动作 ID 包含无效字符。".into())
        } else {
            Ok(&self.id)
        }
    }
}

pub(crate) fn action_id_from_payload(raw: &str) -> Result<String, String> {
    ActionReference::parse(raw)?
        .require_id()
        .map(str::to_string)
}

pub(crate) fn payload_menu_action_template(
    raw: &str,
    execute_param: &str,
) -> Option<MenuActionTemplate> {
    let action = ActionReference::parse(raw).ok()?;
    action.require_id().ok()?;
    // C# GetTemplateId uses TemplateId ?? SharedActionId. Plain ID records keep
    // the previously supported direct-template shortcut.
    let template = action
        .template_id
        .as_deref()
        .or(action.shared_action_id.as_deref())
        .unwrap_or(&action.id);
    menu_action_template(template, execute_param)
}

pub fn build_run_action_uri(action_id: &str, quicker_in_params: &str) -> Option<String> {
    let id = action_id.trim();
    if id.is_empty() {
        return None;
    }
    let mut param = quicker_in_params.to_string();
    if param.len() > MAX_URI_PARAM_LENGTH {
        let mut end = MAX_URI_PARAM_LENGTH;
        while !param.is_char_boundary(end) {
            end -= 1;
        }
        param.truncate(end);
    }
    Some(format!("quicker:runaction:{id}?{param}"))
}

/// C# `ActionTemplateId` shared-menu GUIDs used by `FavorMenuItemWrapper.GetFavorActionMenuWrapper`.
pub const MENU_BOOKMARKS: &str = "a50d39ba-b07c-459e-347c-08da7d9afa4f";
pub const MENU_WINDOW: &str = "c3336884-f4cb-46b9-da4e-08da76a0d1ac";
pub const MENU_FILE: &str = "5042d4d2-2979-4fff-4a25-08da404596c9";
pub const MENU_REOPEN: &str = "347689f7-7bab-42b7-1b75-08da8ae0e8b9";
pub const MENU_TEXT: &str = "54dc3640-79ab-4223-a073-08da8515cad0";
pub const FAVOR_ADDER: &str = "51fc92f5-10c2-41f7-1c7c-08d95d50ce36";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuActionTemplate {
    Window,
    Bookmarks,
    File,
    Reopen,
    Text,
    Adder,
}

/// C# expand map when `ExecuteParam` is empty. Without Quicker RPC, only the template GUID itself matches.
pub fn menu_action_template(action_id: &str, execute_param: &str) -> Option<MenuActionTemplate> {
    if !execute_param.is_empty() {
        return None;
    }
    match action_id.trim().to_ascii_lowercase().as_str() {
        MENU_WINDOW => Some(MenuActionTemplate::Window),
        MENU_BOOKMARKS => Some(MenuActionTemplate::Bookmarks),
        MENU_FILE => Some(MenuActionTemplate::File),
        MENU_REOPEN => Some(MenuActionTemplate::Reopen),
        MENU_TEXT => Some(MenuActionTemplate::Text),
        FAVOR_ADDER => Some(MenuActionTemplate::Adder),
        _ => None,
    }
}

/// Success means Windows accepted the protocol launch; the URI handler cannot report action completion.
pub fn try_run_quicker_action(action_id: &str, quicker_in_params: &str) -> Result<String, String> {
    run_with_launcher(action_id, quicker_in_params, crate::shell_open::open)
}

pub(crate) fn run_with_launcher(
    action_id: &str,
    quicker_in_params: &str,
    launch: impl FnOnce(&str) -> Result<(), String>,
) -> Result<String, String> {
    let uri = build_run_action_uri(action_id, quicker_in_params)
        .ok_or_else(|| "未设置 Quicker 动作 ID。".to_string())?;
    launch(&uri)?;
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quicker_wire_v2_presentation_and_source_metadata_match_actionitem2() {
        let raw = include_str!("../tests/fixtures/quicker-action-v2.json");
        let action = ActionReference::parse(raw).unwrap();
        assert_eq!(
            action.require_id().unwrap(),
            "a65f76bc-8561-4f52-b691-ec732db10bad"
        );
        assert_eq!(action.title.as_deref(), Some("Rust 动作 🦀"));
        assert_eq!(
            action.description.as_deref(),
            Some("native-clipboard-fixture")
        );
        assert_eq!(action.icon.as_deref(), Some("fa:Light_Bolt"));
        assert_eq!(
            payload_menu_action_template(raw, ""),
            Some(MenuActionTemplate::File)
        );
        let shared = serde_json::json!({"id":"v2-action", "presentation":{"title":"共享动作"},
            "sharedActionInfo":{"sharedActionId":MENU_TEXT}})
        .to_string();
        assert_eq!(
            ActionReference::parse(&shared).unwrap().title.as_deref(),
            Some("共享动作")
        );
        assert_eq!(
            payload_menu_action_template(&shared, ""),
            Some(MenuActionTemplate::Text)
        );
        let explicit = serde_json::json!({"Id":"v2-action", "TemplateId":"", "Title":"",
            "SourceActionInfo":{"Id":MENU_FILE}, "Presentation":{"Title":"hidden"}})
        .to_string();
        assert_eq!(
            ActionReference::parse(&explicit).unwrap().title.as_deref(),
            Some("")
        );
        assert_eq!(payload_menu_action_template(&explicit, ""), None);
    }

    #[test]
    fn quicker_json_metadata_shapes_and_template_precedence_match_action_records() {
        for raw in [
            "fixture-id",
            r#"{"Id":"fixture-id","Title":"动作"}"#,
            r#"{"id":"fixture-id","title":"动作"}"#,
            r#"{"Action":{"Id":"fixture-id","Title":"动作"}}"#,
            r#"{"actionItem":{"id":"fixture-id","title":"动作"}}"#,
        ] {
            assert_eq!(action_id_from_payload(raw).unwrap(), "fixture-id");
        }
        for invalid in [
            "",
            "  ",
            "{broken",
            "[]",
            r#""quoted-id""#,
            r#"{"Id":12}"#,
            r#"{"Id":null}"#,
            r#"{"Action":null}"#,
        ] {
            assert!(action_id_from_payload(invalid).is_err(), "{invalid}");
        }
        let make = |template: serde_json::Value| {
            serde_json::json!({
                "Id":"local-action", "TemplateId":template, "SharedActionId":MENU_FILE,
                "Icon":null, "Description":null,
            })
            .to_string()
        };
        assert_eq!(
            payload_menu_action_template(&make(serde_json::Value::Null), ""),
            Some(MenuActionTemplate::File)
        );
        assert_eq!(
            payload_menu_action_template(&make(serde_json::json!(MENU_TEXT)), ""),
            Some(MenuActionTemplate::Text)
        );
        assert_eq!(
            payload_menu_action_template(&make(serde_json::json!("")), ""),
            None
        );
        assert_eq!(
            payload_menu_action_template(&make(serde_json::Value::Null), " "),
            None
        );
        assert_eq!(
            payload_menu_action_template(
                &serde_json::json!({"Title":"no ID", "TemplateId":MENU_FILE}).to_string(),
                ""
            ),
            None
        );
    }

    #[test]
    fn clipboard_action_queue_preserves_order_and_rejects_overflow_without_blocking() {
        use std::sync::mpsc;
        use std::time::Duration;
        let queue = ClipboardActionQueue::default();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        queue
            .submit(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished_tx, finished_rx) = mpsc::channel();
        for index in 0..64 {
            let finished = finished_tx.clone();
            queue
                .submit(move || {
                    finished.send(index).unwrap();
                })
                .unwrap();
        }
        let overflow = queue.submit(|| panic!("rejected job must not run"));
        release_tx.send(()).unwrap();
        assert_eq!(overflow.unwrap_err(), "剪贴板动作队列已满。");
        for index in 0..64 {
            assert_eq!(
                finished_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                index
            );
        }
    }

    #[test]
    fn uri_keeps_id_and_truncates_params() {
        assert_eq!(build_run_action_uri("  ", "x"), None);
        assert_eq!(
            build_run_action_uri("abc-id", "hello"),
            Some("quicker:runaction:abc-id?hello".into())
        );
        let long = "x".repeat(MAX_URI_PARAM_LENGTH + 40);
        let uri = build_run_action_uri("id", &long).unwrap();
        let param = uri.strip_prefix("quicker:runaction:id?").unwrap();
        assert_eq!(param.len(), MAX_URI_PARAM_LENGTH);
    }

    #[test]
    fn invalid_action_target_fails_before_headless_success() {
        crate::clipboard::force_headless(true);
        assert_eq!(
            try_run_quicker_action(" ", "text").unwrap_err(),
            "未设置 Quicker 动作 ID。"
        );
        assert!(try_run_quicker_action("invalid\0action", "text").is_err());
        assert!(try_run_quicker_action("fixture-action", "invalid\0input").is_err());
    }

    #[test]
    fn menu_action_template_matches_csharp_empty_execute_param() {
        assert_eq!(
            menu_action_template(MENU_WINDOW, ""),
            Some(MenuActionTemplate::Window)
        );
        assert_eq!(menu_action_template(MENU_FILE, "x"), None);
        assert_eq!(menu_action_template("not-a-template", ""), None);
        // C# `string.IsNullOrEmpty` — whitespace still blocks template expand.
        assert_eq!(menu_action_template(MENU_TEXT, "  "), None);
        assert_eq!(
            menu_action_template(&MENU_TEXT.to_ascii_uppercase(), ""),
            Some(MenuActionTemplate::Text)
        );
    }

    #[test]
    fn action_uri_long_unicode_parameters_never_split_a_character() {
        for params in [
            format!("x{}", "中文".repeat(MAX_URI_PARAM_LENGTH)),
            format!("ab{}", "🦀".repeat(MAX_URI_PARAM_LENGTH)),
            format!("界{}", "é".repeat(MAX_URI_PARAM_LENGTH)),
        ] {
            let uri = build_run_action_uri("fixture-action", &params).unwrap();
            let clipped = uri
                .strip_prefix("quicker:runaction:fixture-action?")
                .unwrap();
            assert!(params.starts_with(clipped));
            assert!(clipped.len() <= MAX_URI_PARAM_LENGTH);
            let next = params[clipped.len()..].chars().next().unwrap();
            assert!(
                clipped.len() + next.len_utf8() > MAX_URI_PARAM_LENGTH,
                "must retain every complete character within the limit"
            );
        }
    }
}
