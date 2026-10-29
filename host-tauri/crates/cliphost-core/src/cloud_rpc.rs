//! Cloud diagnostics are read-only. Product sync stays disabled with the C# / SPA gate.
use serde_json::{json, Value};
use crate::quicker_rpc::{Registry, INTERACTIVE_TIMEOUT, BACKGROUND_TIMEOUT};

#[cfg(test)]
#[path = "cloud_rpc_tests.rs"]
mod tests;

pub const PUBLICLY_AVAILABLE: bool = false;
pub const DISABLED_REASON: &str = "云同步功能暂时下架。";
pub(crate) const MANIFEST_KEY: &str = "cea/states/sync/manifest";

pub(crate) fn status(registry: &Registry) -> Value {
    json!({"rpcConnected":registry.select().is_some(), "cloudSyncHostEnabled":false,
        "cloudSyncDisabledReason":DISABLED_REASON, "hint":DISABLED_REASON,
        "autoSyncEnabled":false, "lastAutoSyncUtc":Value::Null, "status":Value::Null, "error":Value::Null})
}

pub(crate) fn disabled_result(slot: Option<&str>) -> Value {
    json!({"slotId":slot.unwrap_or(crate::cloud_sync::FAVOR_SLOT), "success":false, "message":DISABLED_REASON})
}

pub(crate) fn probe(registry: &Registry, include_manifest: bool) -> Value {
    probe_with_timeout(registry, include_manifest, INTERACTIVE_TIMEOUT, BACKGROUND_TIMEOUT)
}

pub(crate) fn probe_with_timeout(registry: &Registry, include_manifest: bool,
    interactive: std::time::Duration, background: std::time::Duration) -> Value {
    let Some(session) = registry.select() else {
        return json!({"ok":false,"rpcConnected":false,
            "hint":"Start IntelliTools in Quicker and wait for the Rust pipe connection."});
    };
    let version = match session.call("quicker.getProtocolVersion", json!({}), interactive) {
        Ok(value) if value.as_i64().is_some() => value.as_i64().unwrap(),
        Ok(_) => return json!({"ok":false,"rpcConnected":true,"error":"Invalid Quicker protocol version."}),
        Err(error) => return json!({"ok":false,"rpcConnected":true,"error":error}),
    };
    let (manifest, error) = match session.call("quicker.readCloudState", json!({"CloudKey":MANIFEST_KEY}), background) {
        Ok(Value::String(text)) => (Some(text), None),
        Ok(Value::Null) => (None, None),
        Ok(_) => (None, Some("Invalid cloud state response.".to_string())),
        Err(error) => (None, Some(error)),
    };
    let hint = if error.is_some() { Some("Ensure Quicker is logged in and network is available.") }
        else if manifest.is_none() { Some("CloudState reachable; manifest empty.") } else { None };
    json!({"ok":error.is_none(),"rpcConnected":true,"protocolVersion":version,
        "cloudSyncSupported":version>=8,"cloudSyncHostEnabled":PUBLICLY_AVAILABLE,
        "cloudSyncDisabledReason":DISABLED_REASON,"manifestKey":MANIFEST_KEY,
        "manifestFound":manifest.as_deref().is_some_and(|text| !text.trim().is_empty()),
        "manifestLength":manifest.as_deref().map(|text| text.encode_utf16().count()).unwrap_or(0),
        "manifest":if include_manifest { manifest } else { None },"cloudError":error,"hint":hint})
}
