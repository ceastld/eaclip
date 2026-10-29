//! Optional LAN file history import. File delivery remains manual LAN sharing;
//! this module never reads or writes the system clipboard or emits sync events.

use super::*;
use std::io::Read;

impl HostState {
    /// The committed message is the durable receipt, including across restart.
    /// Only an explicitly requested, incomplete import may be retried.
    pub(super) fn lan_received_history_receipt(
        &self,
        auth_device: &str,
        message_id: &str,
    ) -> Result<Value, String> {
        let mut payload: Value = serde_json::from_str(
            &self
                .store()
                .lock()
                .get_lan_message(message_id)
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if payload["direction"] != "incoming" || payload["senderDeviceId"] != auth_device {
            return Err("消息不属于当前发送设备。".into());
        }
        if payload["historyImportRequested"] == true && payload["historyImportCompleted"] != true {
            let inbox = self.inner.lock().paths.inbox.clone();
            let result = received_file_clip(&payload, &inbox).and_then(|clip| {
                let inserted = self
                    .store()
                    .lock()
                    .import_lan_received_file_clip(message_id, &clip)
                    .map_err(|error| error.to_string())?;
                if inserted {
                    self.emit_clip_changed(std::slice::from_ref(&clip), &[]);
                    self.trigger_cleanup();
                }
                Ok(())
            });
            // File delivery has already committed. An optional history failure
            // is separate from delivery and remains recoverable on a retry.
            let store = self.store();
            let db = store.lock();
            payload = serde_json::from_str(
                &db.get_lan_message(message_id)
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            if let Err(error) = result {
                if payload["historyImportCompleted"] != true {
                    payload["historyImportError"] = json!(error);
                    if let Err(save_error) = db.update_lan_message(message_id, &payload.to_string())
                    {
                        tracing::warn!(%save_error, "Could not persist optional LAN history import failure");
                    }
                }
            }
            drop(db);
            self.push_event(
                crate::lan::SHARE_CHANGED_EVENT,
                json!({
                    "messageId":message_id, "conversationId":payload["conversationId"],
                }),
            );
        }
        let mut receipt = json!({
            "delivered": true,
            "receivedAtUtc": payload.get("receivedAtUtc").cloned().unwrap_or_else(|| json!(Utc::now())),
        });
        for key in [
            "historyImportRequested",
            "historyImportCompleted",
            "historyItemId",
            "historyImportError",
        ] {
            if let Some(value) = payload.get(key) {
                receipt[key] = value.clone();
            }
        }
        Ok(receipt)
    }
}

fn received_file_clip(payload: &Value, inbox: &Path) -> Result<ClipRecord, String> {
    use sha2::{Digest, Sha256};

    let message_id = payload["id"].as_str().ok_or("接收消息缺少标识。")?;
    let sender = payload["senderDeviceId"]
        .as_str()
        .ok_or("接收消息缺少来源设备。")?;
    let attachments = payload["attachments"]
        .as_array()
        .ok_or("接收消息缺少附件。")?;
    if payload["status"] != "received" || attachments.is_empty() {
        return Err("只能将完整收到的文件加入剪贴板历史。".into());
    }
    let inbox = inbox
        .canonicalize()
        .map_err(|error| format!("接收目录不可用: {error}"))?;
    let mut paths = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        if attachment["status"] != "received" {
            return Err("附件尚未完整接收。".into());
        }
        let path = PathBuf::from(
            attachment["localPath"]
                .as_str()
                .ok_or("附件缺少本机文件。")?,
        );
        if !path.is_absolute() {
            return Err("接收文件必须使用本机绝对路径。".into());
        }
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("接收文件不可用: {error}"))?;
        if !canonical.starts_with(&inbox) || !canonical.is_file() {
            return Err("接收文件不在本机接收目录中。".into());
        }
        let mut file = std::fs::File::open(&canonical).map_err(|error| error.to_string())?;
        let expected_length = attachment["sizeBytes"]
            .as_u64()
            .ok_or("附件缺少有效长度。")?;
        if file.metadata().map_err(|error| error.to_string())?.len() != expected_length {
            return Err("本机接收文件长度已变化。".into());
        }
        let expected_hash = attachment["sha256"].as_str().ok_or("附件缺少校验摘要。")?;
        let mut hasher = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        if !format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected_hash) {
            return Err("本机接收文件校验失败。".into());
        }
        paths.push(path.to_string_lossy().into_owned());
    }
    let mut clip = ClipRecord::new_text("", "ClipHost");
    // Length-prefix the components so peer/message boundaries cannot collide.
    clip.id = format!(
        "lan-file-{}",
        crate::lan::sha256_utf8_hex(&format!("{}:{sender}{message_id}", sender.len()))
    );
    clip.kind = ClipKind::FileClipItem;
    clip.body_text = None;
    clip.content_hash = hash_files(&paths);
    clip.file_paths = paths;
    let alias = payload["senderAlias"]
        .as_str()
        .filter(|alias| !alias.is_empty())
        .unwrap_or(sender);
    clip.description = format!("局域网接收 · {alias}");
    if let Some(received_at) = payload
        .get("receivedAtUtc")
        .and_then(|value| serde_json::from_value::<DateTime<Utc>>(value.clone()).ok())
    {
        clip.create_time = received_at;
        clip.update_time = received_at;
    }
    Ok(clip)
}

#[cfg(test)]
#[path = "lan_received_history_tests.rs"]
mod tests;
