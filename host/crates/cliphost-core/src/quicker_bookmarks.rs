//! Quicker adapter for the transport-independent browser bookmark provider.
use std::{sync::Arc, time::Duration};
use serde_json::Value;
use crate::bookmarks::{BookmarkConnection, BookmarkNode, BookmarkProvider};

pub(crate) struct QuickerBookmarks(pub Option<Arc<crate::quicker_rpc::Session>>);

impl QuickerBookmarks {
    fn call(&self, method: &str, payload: Value, timeout: Duration) -> Result<Value, String> {
        let session = self.0.as_ref().ok_or("未连接 Quicker，请通过 Quicker 动作连接宿主后读取浏览器书签。")?;
        session.call(method, payload, timeout).map_err(|error| {
            if error.contains("method_not_found") || error.to_ascii_lowercase().contains("unknown method") {
                "Quicker 适配插件尚不支持指定浏览器读取书签，请更新插件。".into()
            } else { error }
        })
    }
}

impl BookmarkProvider for QuickerBookmarks {
    fn connections(&self) -> Result<Vec<BookmarkConnection>, String> {
        let value = self.call("quicker.getBookmarkConnections", Value::Null, Duration::from_secs(2))?;
        serde_json::from_value(value).map_err(|_| "无法识别 Quicker 浏览器连接信息。".into())
    }

    fn tree(&self, target: &BookmarkConnection) -> Result<BookmarkNode, String> {
        let value = self.call("quicker.getBookmarksForTarget", serde_json::to_value(target).unwrap(), Duration::from_secs(12))?;
        value.as_str().and_then(crate::bookmarks::parse_rpc_tree)
            .ok_or_else(|| "所选浏览器扩展未返回有效书签，请检查书签权限。".into())
    }
}
