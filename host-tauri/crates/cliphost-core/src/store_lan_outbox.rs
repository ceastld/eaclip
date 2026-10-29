//! Retain only pending delivery deadlines between the outbox's lifecycle polls.
//! SQLite's change stamp covers writes through every LAN entry point, deletes,
//! and commits from other connections, including the C# document-store layout.
use super::{AppStore, StoreError};
use chrono::{DateTime, Utc};
use serde_json::Value;

pub(super) struct LanOutboxSchedule {
    stamp: (u64, i64),
    pending: Vec<(DateTime<Utc>, String)>,
    #[cfg(test)]
    refreshes: usize,
}

impl AppStore {
    pub(crate) fn due_lan_delivery_ids(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<String>, StoreError> {
        // Read the stamp before the rows: a concurrent commit during the read
        // must invalidate this snapshot on the next poll, not be acknowledged.
        let stamp = self.change_stamp()?;
        let mut cached = self.lan_outbox_schedule.borrow_mut();
        if cached.as_ref().is_none_or(|schedule| schedule.stamp != stamp) {
            let mut pending = Vec::new();
            for raw in self.list_json("lan_message")? {
                let Ok(payload) = serde_json::from_str::<Value>(&raw) else { continue; };
                if payload.get("direction").and_then(Value::as_str) != Some("outgoing") {
                    continue;
                }
                let Some(deliveries) = payload.get("deliveries").and_then(Value::as_array) else { continue; };
                for delivery in deliveries {
                    if !matches!(delivery.get("status").and_then(Value::as_str), Some("queued" | "failed")) {
                        continue;
                    }
                    let Some(id) = delivery.get("id").and_then(Value::as_str) else { continue; };
                    let at = delivery.get("nextAttemptAtUtc")
                        .and_then(|value| serde_json::from_value::<DateTime<Utc>>(value.clone()).ok())
                        .unwrap_or(DateTime::<Utc>::MIN_UTC);
                    pending.push((at, id.to_string()));
                }
            }
            pending.sort_by_key(|(at, _)| *at);
            *cached = Some(LanOutboxSchedule {
                stamp,
                pending,
                #[cfg(test)]
                refreshes: cached.as_ref().map_or(1, |schedule| schedule.refreshes + 1),
            });
        }
        Ok(cached.as_ref().into_iter().flat_map(|schedule| schedule.pending.iter())
            .take_while(|(at, _)| *at <= now)
            .take(limit)
            .map(|(_, id)| id.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use serde_json::json;

    fn payload(id: &str, status: &str, at: DateTime<Utc>) -> String {
        json!({"direction":"outgoing", "deliveries":[{
            "id":id, "status":status, "nextAttemptAtUtc":at,
        }]}).to_string()
    }

    fn refreshes(store: &AppStore) -> usize {
        store.lan_outbox_schedule.borrow().as_ref().unwrap().refreshes
    }

    #[test]
    fn lan_outbox_idle_reuses_rows_and_retries_at_the_existing_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        store.insert_lan_message("later", "dm:test", &payload("later:peer", "failed", now + Duration::seconds(30))).unwrap();
        store.insert_lan_message("ready", "dm:test", &payload("ready:peer", "queued", now)).unwrap();
        store.insert_lan_message("done", "dm:test", &payload("done:peer", "delivered", now)).unwrap();
        store.insert_lan_message("incoming", "dm:test", &json!({"direction":"incoming", "deliveries":[{"id":"incoming:peer","status":"queued"}]}).to_string()).unwrap();
        store.insert_lan_message("bad", "dm:test", "{invalid").unwrap();
        for second in 0..30 {
            assert_eq!(store.due_lan_delivery_ids(now + Duration::seconds(second), 4).unwrap(), ["ready:peer"]);
        }
        assert_eq!(refreshes(&store), 1, "idle polls must not read/decode the message history again");
        assert_eq!(store.due_lan_delivery_ids(now + Duration::seconds(30), 4).unwrap(), ["ready:peer", "later:peer"]);
        assert_eq!(store.due_lan_delivery_ids(now + Duration::seconds(30), 1).unwrap(), ["ready:peer"]);
        assert_eq!(refreshes(&store), 1, "time and batch-cap changes only inspect pending deadlines");
    }

    #[test]
    fn lan_outbox_writes_cancellation_deletion_and_manual_retry_invalidate_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
        let now = Utc::now();
        assert!(store.due_lan_delivery_ids(now, 4).unwrap().is_empty());
        store.insert_lan_message("message", "dm:test", &payload("message:peer", "queued", now)).unwrap();
        assert_eq!(store.due_lan_delivery_ids(now, 4).unwrap(), ["message:peer"]);
        for status in ["sending", "delivered", "canceled"] {
            store.update_lan_message("message", &payload("message:peer", status, now)).unwrap();
            assert!(store.due_lan_delivery_ids(now, 4).unwrap().is_empty());
        }
        store.update_lan_message("message", &payload("message:peer", "failed", now + Duration::seconds(60))).unwrap();
        assert!(store.due_lan_delivery_ids(now, 4).unwrap().is_empty());
        store.update_lan_message("message", &payload("message:peer", "queued", now)).unwrap();
        assert_eq!(store.due_lan_delivery_ids(now, 4).unwrap(), ["message:peer"]);
        store.delete_json("lan_message", "message").unwrap();
        assert!(store.due_lan_delivery_ids(now, 4).unwrap().is_empty());
        let scans = refreshes(&store);
        for _ in 0..20 { assert!(store.due_lan_delivery_ids(now, 4).unwrap().is_empty()); }
        assert_eq!(refreshes(&store), scans, "an empty outbox also stays cached");
    }

    #[test]
    fn lan_outbox_other_connection_commit_invalidates_cached_rows() {
        for document_store in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("userdata.db");
            if document_store {
                rusqlite::Connection::open(&path).unwrap().execute_batch(
                    "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL);
                     CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL);
                     CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL);",
                ).unwrap();
            }
            let store = AppStore::open(&path).unwrap();
            let writer = AppStore::open(&path).unwrap();
            assert_eq!(store.is_document_store(), document_store);
            let now = Utc::now();
            assert!(store.due_lan_delivery_ids(now, 4).unwrap().is_empty());
            writer.insert_lan_message("external", "dm:test", &payload("external:peer", "queued", now)).unwrap();
            assert_eq!(store.due_lan_delivery_ids(now, 4).unwrap(), ["external:peer"]);
            writer.delete_json("lan_message", "external").unwrap();
            assert!(store.due_lan_delivery_ids(now, 4).unwrap().is_empty());
            assert_eq!(refreshes(&store), 3);
        }
    }
}
