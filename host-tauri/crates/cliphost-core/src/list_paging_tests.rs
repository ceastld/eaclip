use super::*;

fn csharp_document_store(path: &std::path::Path) -> AppStore {
    rusqlite::Connection::open(path).unwrap().execute_batch(
        "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
         CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);
         CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);",
    ).unwrap();
    let store = AppStore::open(path).unwrap();
    assert!(store.is_document_store());
    store
}

/// Same-millisecond and exact-time ties, SubIndex bursts, pins, record and
/// usage access times, so every coarse block boundary is exercised.
fn seed(store: &AppStore) {
    let base = Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
    let mut state = 0x2545_f491_u64;
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let records: Vec<ClipRecord> = (0..420)
        .map(|index| {
            let mut item = ClipRecord::new_text(format!("paging {index}"), ["a.exe", "b.exe", ""][index % 3]);
            item.id = format!("page-{index:04}");
            let millis = next(60) as i64;
            item.create_time = base + chrono::Duration::milliseconds(millis)
                + chrono::Duration::nanoseconds(next(4) as i64 * 100_000);
            item.update_time = item.create_time;
            item.sub_index = next(3) as i32;
            item.is_top = next(9) == 0;
            item.last_access = match next(4) {
                0 => Some(item.create_time + chrono::Duration::nanoseconds(next(3) as i64 * 200_000)),
                1 => Some(item.create_time - chrono::Duration::milliseconds(5)),
                2 => Some(base + chrono::Duration::milliseconds(next(70) as i64)),
                _ => None,
            };
            item.usage_count = next(3) as i32;
            if index % 11 == 0 {
                item.kind = ClipKind::LinkClipItem;
            }
            item
        })
        .collect();
    store.insert_clips(&records).unwrap();
    for record in records.iter().step_by(7) {
        store.update_usage(&crate::store::clip_usage_key(&record.id)).unwrap();
    }
}

fn assert_pages_match_full_sort(store: &AppStore) {
    for sort_type in [None, Some(0), Some(1)] {
        for star in [0, 1, 2] {
            for usage in [0, 1, 2] {
                let keep_key = |key: &ClipListKey| {
                    (match star { 1 => key.is_top, 2 => !key.is_top, _ => true })
                        && (match usage { 1 => key.usage_count > 0, 2 => key.usage_count <= 0, _ => true })
                };
                let keep_item = |item: &ClipRecord| matches_star(item, star) && matches_usage(item, usage);
                let mut full: Vec<ClipRecord> = store.list_clips().unwrap().into_iter().filter(keep_item).collect();
                sort_clips(&mut full, sort_type, "");
                let keys: Vec<ClipListKey> = store.list_clip_keys().unwrap().into_iter().filter(keep_key).collect();
                assert_eq!(keys.len(), full.len(), "total sort={sort_type:?} star={star} usage={usage}");
                for (skip, take) in [(0, 100), (0, 7), (13, 29), (99, 100), (250, 100), (400, 100), (900, 10)] {
                    let expected: Vec<&str> = full.iter().skip(skip).take(take).map(|i| i.id.as_str()).collect();
                    let page = page_by_list_keys(store, keys.clone(), sort_type, skip, take).unwrap();
                    let actual: Vec<&str> = page.iter().map(|i| i.id.as_str()).collect();
                    assert_eq!(actual, expected, "sort={sort_type:?} star={star} usage={usage} skip={skip} take={take}");
                    for (a, b) in page.iter().zip(full.iter().skip(skip)) {
                        assert_eq!((a.usage_count, a.last_access), (b.usage_count, b.last_access));
                    }
                }
            }
        }
    }
}

#[test]
fn url_filter_paging_search_and_constraints_match_in_both_store_formats() {
    crate::clipboard::force_headless(true);
    for document_store in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("data"));
        paths.ensure().unwrap();
        if document_store { drop(csharp_document_store(&paths.db)); }
        let host = HostState::open(paths.clone(), 28965).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
        let mut rows = Vec::new();
        for (index, (id, kind, text)) in [
            ("plain", ClipKind::TextClipItem, "needle https://example.test/plain"),
            ("html", ClipKind::HtmlClipItem, "needle title\r\nhttps://example.test/html"),
            ("qq", ClipKind::QQRichTextItem, "needle\nmore\nhttps://example.test/qq"),
            ("link", ClipKind::LinkClipItem, "https://example.test/needle"),
            ("file", ClipKind::FileClipItem, "https://example.test/needle"),
            ("path", ClipKind::TextClipItem, "file:///C:/needle.txt"),
            ("metadata", ClipKind::HtmlClipItem, "needle plain text"),
            ("title", ClipKind::TextClipItem, "needle plain text"),
        ].into_iter().enumerate() {
            let mut row = ClipRecord::new_text(text, if id == "qq" { "other.exe" } else { "browser.exe" });
            row.id = id.into();
            row.kind = kind;
            row.create_time = base + chrono::Duration::seconds(index as i64);
            row.update_time = row.create_time;
            row.is_top = id == "plain";
            if id == "metadata" {
                row.html_source_url = Some("https://source.test/".into());
                row.html_data = Some("<a href='https://hidden.test'>needle plain text</a>".into());
            }
            if id == "title" { row.title = Some("https://title.test/needle".into()); }
            rows.push(row);
        }
        host.store().lock().insert_clips(&rows).unwrap();
        host.store().lock().update_usage(&crate::store::clip_usage_key("html")).unwrap();
        let page = |extra: Value| {
            let mut request = json!({"webSessionId":"url-filter", "type":"url", "take":2});
            request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            host.search_items(&request).unwrap()
        };
        let ids = |response: &Value| response["items"].as_array().unwrap().iter()
            .map(|row| row["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let first = page(json!({}));
        assert_eq!(first["total"], 5);
        assert_eq!(ids(&first), ["plain", "title"]);
        assert_eq!(first["hasMore"], true);
        let second = page(json!({"skip":2}));
        assert_eq!(ids(&second), ["link", "qq"]);
        assert_eq!(second["total"], 5);
        assert_eq!(second["hasMore"], true);
        let last = page(json!({"skip":4}));
        assert_eq!(ids(&last), ["html"]);
        assert_eq!(last["hasMore"], false);
        for extra in [json!({"query":"needle", "take":20}), json!({"createTimeFilter":-2,"createTimeFilterDate":"2026-10-01","take":20})] {
            let response = page(extra);
            assert_eq!(response["total"], 5);
            let mut found = ids(&response);
            found.sort();
            assert_eq!(found, ["html", "link", "plain", "qq", "title"]);
        }
        let constraints = json!({"processFilter":{"includeExeNames":["browser"]}, "filterItem":{"type":"url","starType":2}, "usageFilter":1});
        assert_eq!(ids(&page(constraints.clone())), ["html"]);
        let mut searched = constraints;
        searched["query"] = json!("needle");
        assert_eq!(ids(&page(searched)), ["html"]);
        assert_eq!(page(json!({"query":"no-such-url"}))["total"], 0);
        let store = host.store();
        let mut edited = store.lock().get_clip("html").unwrap();
        edited.body_text = Some("needle edited plain".into());
        store.lock().save_clip(&edited).unwrap();
        assert_eq!(page(json!({}))["total"], 4);
        assert_eq!(page(json!({"query":"needle"}))["total"], 4);
    }
}

#[test]
fn indexed_paging_matches_full_history_sort_in_native_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = AppStore::open(&dir.path().join("userdata.db")).unwrap();
    seed(&store);
    assert_pages_match_full_sort(&store);
}

#[test]
fn indexed_paging_matches_full_history_sort_in_csharp_document_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = csharp_document_store(&dir.path().join("userdata.db"));
    seed(&store);
    assert_pages_match_full_sort(&store);
    // Reopening rebuilds the projection from the original documents.
    drop(store);
    assert_pages_match_full_sort(&AppStore::open(&dir.path().join("userdata.db")).unwrap());
}

#[test]
fn search_pages_reuse_the_order_until_history_changes() {
    crate::clipboard::force_headless(true);
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
    let base = Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
    let records: Vec<ClipRecord> = (0..250)
        .map(|index| {
            let text = if index % 3 == 0 { format!("needle {index}") } else { format!("剪贴 nee dle {index}") };
            let mut item = ClipRecord::new_text(text, "search-pages");
            item.id = format!("search-{index:03}");
            item.create_time = base + chrono::Duration::seconds(index);
            item
        })
        .collect();
    host.store().lock().insert_clips(&records).unwrap();
    let page = |skip: i32| host.search_items(&json!({"webSessionId": "pages", "query": "needle", "skip": skip, "take": 40})).unwrap();
    let ids = |page: &Value| page["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let first = page(0);
    let cached = [page(40), page(80)];
    *host.search_pages.lock() = None;
    let fresh = [page(40), page(80)];
    assert_eq!(cached.iter().map(ids).collect::<Vec<_>>(), fresh.iter().map(ids).collect::<Vec<_>>());
    assert_eq!(cached[0]["total"], first["total"]);
    assert_eq!(cached[0], fresh[0]);
    let total = first["total"].as_i64().unwrap();
    host.add_text(&json!({"text": "needle added later"})).unwrap();
    assert_eq!(page(40)["total"].as_i64().unwrap(), total + 1);
    host.store().lock().update_usage(&crate::store::clip_usage_key("search-003")).unwrap();
    let usage = host.search_items(&json!({"webSessionId": "pages", "query": "needle", "usageFilter": 1, "take": 40})).unwrap();
    assert_eq!(ids(&usage), vec!["search-003".to_string()]);
}

fn csharp_host_fixture(count: usize, max_item_size: i32) -> (tempfile::TempDir, AppPaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("Ceastld"));
    std::fs::create_dir_all(&paths.configs).unwrap();
    std::fs::write(paths.configs.join("ClipConfig.json"), format!(r#"{{"MaxItemSize":{max_item_size}}}"#)).unwrap();
    let store = csharp_document_store(&paths.db);
    let base = Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
    let records: Vec<ClipRecord> = (0..count)
        .map(|index| {
            let mut item = ClipRecord::new_text(format!("deferred {index}"), "fixture");
            item.id = format!("deferred-{index:03}");
            item.create_time = base + chrono::Duration::seconds(index as i64);
            item.is_top = index % 10 == 0;
            item
        })
        .collect();
    store.insert_clips(&records).unwrap();
    (dir, paths)
}

#[test]
fn unprojected_document_store_refuses_reads_and_writes_until_projected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("userdata.db");
    drop(csharp_document_store(&path));
    let store = AppStore::open_unprojected(&path).unwrap();
    assert!(store.list_clips().unwrap_err().to_string().contains("尚未加载"));
    assert!(store.insert_clips(&[ClipRecord::new_text("early", "fixture")]).is_err());
    store.finish_projection().unwrap();
    assert!(store.list_clips().unwrap().is_empty());
    store.insert_clips(&[ClipRecord::new_text("after", "fixture")]).unwrap();
    // A malformed document is isolated, while its source remains protected.
    rusqlite::Connection::open(&path).unwrap()
        .execute("UPDATE clip_item SET Data='not json'", []).unwrap();
    let broken = AppStore::open_unprojected(&path).unwrap();
    broken.finish_projection().unwrap();
    let status = broken.document_recovery_status().unwrap();
    assert_eq!(status["readOnly"], true);
    assert_eq!(status["isolatedRows"], 1);
    assert!(broken.list_clips().unwrap().is_empty());
    assert!(broken.insert_clips(&[ClipRecord::new_text("lost", "fixture")]).unwrap_err().to_string().contains("DATABASE_RECOVERY_READ_ONLY"));
    let raw: String = rusqlite::Connection::open(&path).unwrap()
        .query_row("SELECT Data FROM clip_item", [], |row| row.get(0)).unwrap();
    assert_eq!(raw, "not json");
}

#[test]
fn failed_document_projection_keeps_reads_and_writes_closed_after_sqlite_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("userdata.db");
    drop(csharp_document_store(&path));
    let store = AppStore::open_unprojected(&path).unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let error = store.finish_projection().unwrap_err().to_string();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(error.contains("locked"), "{error}");
    assert!(store.list_clips().unwrap_err().to_string().contains("加载失败"));
    assert!(store.insert_clips(&[ClipRecord::new_text("lost", "fixture")]).is_err());
}

#[test]
fn deferred_projection_serves_the_full_history_after_startup() {
    crate::clipboard::force_headless(true);
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
    let (_dir, paths) = csharp_host_fixture(60, 100);
    let host = HostState::open(paths, 28965).unwrap();
    let page = host.search_items(&json!({"webSessionId": "deferred", "take": 100})).unwrap();
    assert_eq!(page["total"], 60);
    assert_eq!(host.inner.lock().config.max_item_size, 100);
}

#[test]
fn history_over_the_limit_still_raises_it_before_any_cleanup() {
    crate::clipboard::force_headless(true);
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
    let (_dir, paths) = csharp_host_fixture(60, 20);
    let host = HostState::open(paths, 28965).unwrap();
    // 54 unpinned rows exceed the persisted limit of 20: opening raises it, so
    // a due cleanup keeps every reused record.
    assert_eq!(host.inner.lock().config.max_item_size, 54);
    host.trigger_cleanup();
    host.flush_cleanup_now();
    assert_eq!(host.search_items(&json!({"webSessionId": "limit", "take": 100})).unwrap()["total"], 60);
}
