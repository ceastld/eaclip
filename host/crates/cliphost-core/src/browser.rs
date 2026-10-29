//! Browser metadata supplied by the optional Quicker extension bridge.
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct BrowserTab {
    pub title: String,
    pub url: String,
    pub icon: String,
}

impl BrowserTab {
    pub fn from_rpc(value: &Value) -> Option<Self> {
        let text = |camel, pascal| value.get(camel).or_else(|| value.get(pascal))
            .and_then(Value::as_str).unwrap_or("").trim().to_string();
        let url = text("url", "Url");
        if url.is_empty() { return None; }
        let title = text("title", "Title");
        Some(Self { title: if title.is_empty() { url.clone() } else { title },
            url, icon: text("icon", "Icon") })
    }
}

pub const UNAVAILABLE: &str = "无法获取浏览器标签，请确认 Quicker 插件与浏览器扩展已连接";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tab_metadata_accepts_both_wire_casings_and_filters_empty_urls() {
        for value in [json!({"Title":" 页面🦀 ","Url":" https://example.test/?x=中文&y=1 ","Icon":" data:image/png;base64,AA== "}),
            json!({"title":" 页面🦀 ","url":" https://example.test/?x=中文&y=1 ","icon":" data:image/png;base64,AA== "})] {
            let tab = BrowserTab::from_rpc(&value).unwrap();
            assert_eq!(tab.title, "页面🦀");
            assert_eq!(tab.url, "https://example.test/?x=中文&y=1");
            assert_eq!(tab.icon, "data:image/png;base64,AA==");
        }
        assert_eq!(BrowserTab::from_rpc(&json!({"Url":"chrome://settings", "Title":null})).unwrap().title, "chrome://settings");
        for value in [Value::Null, json!({"url":"  ","title":"invalid"}), json!({"url":17})] {
            assert!(BrowserTab::from_rpc(&value).is_none());
        }
    }
}
